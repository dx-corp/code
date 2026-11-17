//! A deterministic recap of a thread's earlier turns.
//!
//! Size-based compaction ([`crate::Threshold`]) waits until history nears the
//! model's budget, which a short thread never reaches. Until then every
//! earlier Dex reply goes back to the model word for word, and the model
//! reuses its own long answer as the template for the next one. A recap
//! replaces the turns before the last exchange with what each person said,
//! what Dex used and the first lines of each reply, once one of those replies
//! is long. It runs no model and costs nothing.
//!
//! What stays exact:
//!
//! - The last exchange (the last person message, Dex's steps and its whole
//!   reply) is never recapped after a turn, so "do number 9" still finds the
//!   list it refers to. A long reply is shortened one turn later.
//! - What each person said is kept in their own words within its own budget
//!   ([`SAID_BYTES`]). When the detail of an old turn no longer fits, only
//!   Dex's side of it goes; a constraint set on the first turn is still there
//!   many turns later. Past that budget the oldest messages are shortened to
//!   their first lines before any is left out.
//! - When the recap replaces a declined model summary while a turn is
//!   running, that turn's own steps and results are not part of it unless the
//!   turn alone is too large, and then each inline result is kept as returned
//!   within [`CURRENT_RESULTS_BYTES`].
//!
//! The recap is an ordinary [`crate::Compaction`]: the same event, storage and
//! turn-boundary apply rule as a model summary, in the same envelope, so
//! attachment and stored-output references survive it exactly. An assistant
//! step kept after it is replayed without its thinking, as after any
//! compaction (`last_compaction_cursor`).
//!
//! Nothing is planned when no reply before the last exchange is long, so an
//! unchanged thread writes nothing (`docs/formal/dex_turn_recap`).

use serde_json::Value;

use crate::{Compaction, Context, Entry, Message, Outcome, Output};

/// A finished reply at least this long is recapped once a later exchange
/// has finished.
pub const RECAP_REPLY_BYTES: usize = 1200;
/// History not yet compacted past this many bytes (about 40k tokens at four
/// bytes a token) triggers a recap whatever its replies, so a long thread of
/// short turns never carries hundreds of thousands of tokens into every turn
/// on its way to the model's limit.
pub const RECAP_HISTORY_BYTES: usize = 160 * 1024;
/// The turn blocks (what Dex used and replied, beside what was asked) stay
/// under this; older turns keep only what the person said.
const RECAP_BYTES: usize = 2400;
/// Everything people said in turns whose detail no longer fits stays under
/// this (about 4k tokens).
const SAID_BYTES: usize = 16 * 1024;
/// The first lines kept of a person message once [`SAID_BYTES`] is full.
const SAID_LEAD_BYTES: usize = 200;
/// A reply shorter than this is kept whole in the recap.
const KEEP_REPLY_BYTES: usize = 320;
/// The first lines kept of a longer reply.
const REPLY_LEAD_BYTES: usize = 240;
/// The most kept of one person message.
const ASKED_BYTES: usize = 1200;
/// Most tool uses named per turn.
const USES_PER_TURN: usize = 6;
/// A prior summary larger than this is left to size-based compaction.
const CARRIED_SUMMARY_BYTES: usize = 6 * 1024;
/// The most of an earlier model summary a fallback recap keeps.
const FALLBACK_CARRIED_BYTES: usize = 2 * 1024;
/// The most attachment references a fallback recap keeps when the exact
/// reference set cannot be formed.
const FALLBACK_REFERENCE_BYTES: usize = 4 * 1024;
/// The most kept of the running turn's own results when a fallback recap
/// has to cover them.
const CURRENT_RESULTS_BYTES: usize = 24 * 1024;

const LEFT_OUT_TAIL: &str = "left out of this recap.";
/// The tail recaps were first written with; still read from stored recaps.
const LEGACY_LEFT_OUT_TAIL: &str = "left out; the thread log keeps them.";

const HEADER: &str = "Recap of earlier turns in this conversation. What each person said is in their own words. Earlier Dex replies are shortened to their first lines, and the rest of each is not available in this turn: when someone refers to a part that is not shown, read the records again instead of guessing. Current facts come from this turn's reads.";
/// The header recaps were first written with. It said the full text of a
/// reply was in the thread log, which no customer turn can read. Stored
/// recaps that begin with it are still recognised and rewritten.
const LEGACY_HEADER: &str = "Recap of earlier turns in this conversation. Earlier Dex replies are shortened to their first lines; their full text is in the thread log at the cursor shown, and current facts come from this turn's reads.";

const TURN_PREFIX: &str = "Turn at cursor ";
const SAID_PREFIX: &str = "Said at cursor ";
const CURRENT_PREFIX: &str = "This turn so far";

/// Where the last exchange starts: the first message of the turn the last
/// person message belongs to.
fn last_exchange(history: &[Entry]) -> Option<usize> {
    let last = history
        .iter()
        .rev()
        .find_map(|entry| match &entry.message {
            Message::User { turn, .. } => Some(turn),
            _ => None,
        })?;
    history
        .iter()
        .position(|entry| matches!(&entry.message, Message::User { turn, .. } if turn == last))
}

fn bytes(entries: &[Entry]) -> usize {
    entries
        .iter()
        .filter(|entry| !matches!(entry.message, Message::Summary { .. }))
        .map(|entry| entry.message.size())
        .sum()
}

/// The recap to append after a finished turn, or `None` when none is due.
/// It covers the turns before the last exchange, which stays as it was.
/// History past [`RECAP_HISTORY_BYTES`] is always recapped. Below that, a
/// recap is due when a finished reply before the last exchange reaches
/// [`RECAP_REPLY_BYTES`] and no covered message carries an attachment
/// (size-based compaction keeps those). Nothing is due while a turn is
/// running or has open calls.
// formal: begin dex_turn_recap/TurnRecap.Plan
pub fn recap_after_turn(ctx: &Context) -> Option<Compaction> {
    if ctx.turn_running() || ctx.open_step().is_some() || ctx.open_attempt().is_some() {
        return None;
    }
    let history = ctx.history();
    let exchange = last_exchange(history)?;
    if bytes(history) > RECAP_HISTORY_BYTES {
        // Past the cap the recap replaces the earlier turns whatever they
        // hold, attachments included (their references are kept). A last
        // exchange that is itself most of the cap keeps only its reply.
        let cut = if crate::compaction::valid_cut(history, exchange)
            && bytes(&history[exchange..]) <= RECAP_HISTORY_BYTES / 2
        {
            exchange
        } else if crate::compaction::valid_cut(history, history.len() - 1)
            && matches!(&history.last()?.message, Message::Assistant { calls, .. } if calls.is_empty())
        {
            history.len() - 1
        } else if crate::compaction::valid_cut(history, history.len()) {
            history.len()
        } else {
            return None;
        };
        let covered = &history[..cut];
        return Some(Compaction {
            covers_to: covered.last()?.cursor,
            summary: fallback(ctx, covered)?,
        });
    }
    if !crate::compaction::valid_cut(history, exchange) {
        return None;
    }
    let covered = &history[..exchange];
    let long_reply = covered.iter().any(|entry| {
        matches!(&entry.message, Message::Assistant { text, calls, .. }
            if calls.is_empty() && text.len() >= RECAP_REPLY_BYTES)
    });
    if !long_reply {
        return None;
    }
    let attached = covered.iter().any(
        |entry| matches!(&entry.message, Message::User { attachments, .. } if !attachments.is_empty()),
    );
    if attached {
        return None;
    }
    let (_, references, from, to) =
        crate::summary::material_with_evidence(covered, ctx.tool_evidence())?;
    let text = render(covered, Carry::DeclineOver(CARRIED_SUMMARY_BYTES), None)?;
    Some(Compaction {
        covers_to: covered.last()?.cursor,
        summary: crate::summary::envelope(from, to, &references, &text),
    })
}
// formal: end dex_turn_recap/TurnRecap.Plan

/// The recap that replaces `entries` when the model summary of them was
/// declined (it failed, timed out, or could not keep its references): the
/// same deterministic recap, in the same envelope, so a turn whose history
/// outgrew the model's context still answers instead of ending. It never
/// declines for size: an earlier model summary is shortened, the oldest
/// person messages are shortened and then left out with a note, and when the
/// exact references cannot be kept the attachment references that fit are
/// kept and the note says the rest are not listed. `None` only when
/// `entries` holds no turn.
pub(crate) fn fallback(ctx: &Context, entries: &[Entry]) -> Option<String> {
    fallback_with(ctx, entries, None)
}

/// [`fallback`] for entries that include the running turn's own steps, which
/// start at `current_from`: their inline results are kept as returned within
/// [`CURRENT_RESULTS_BYTES`] instead of being reduced to an outcome word.
pub(crate) fn fallback_mid_turn(
    ctx: &Context,
    entries: &[Entry],
    current_from: usize,
) -> Option<String> {
    fallback_with(ctx, entries, Some(current_from))
}

fn fallback_with(ctx: &Context, entries: &[Entry], current_from: Option<usize>) -> Option<String> {
    let first = entries.first()?.cursor.0;
    let last = entries.last()?.cursor.0;
    let (references, from, kept_all) =
        match crate::summary::material_with_evidence(entries, ctx.tool_evidence()) {
            Some((_, references, from, _)) => (references, from, true),
            None => {
                let mut kept = Vec::new();
                let mut bytes = 0;
                for entry in entries.iter().rev() {
                    if let Message::User { attachments, .. } = &entry.message {
                        for attachment in attachments {
                            let reference = attachment.to_string();
                            bytes += reference.len() + 4;
                            if bytes > FALLBACK_REFERENCE_BYTES {
                                break;
                            }
                            kept.push(reference);
                        }
                    }
                }
                kept.sort();
                kept.dedup();
                (kept, first, false)
            }
        };
    let mut text = render(
        entries,
        Carry::ShortenTo(FALLBACK_CARRIED_BYTES),
        current_from,
    )?;
    if !kept_all {
        text.push_str("\n\nSome earlier attachments and stored results are not listed here.");
    }
    Some(crate::summary::envelope(
        from.min(first),
        last,
        &references,
        &text,
    ))
}

/// What to do with an earlier summary that is not a recap.
#[derive(Clone, Copy)]
enum Carry {
    /// Leave the history to size compaction when it is larger than this.
    DeclineOver(usize),
    /// Keep at most this much of it, marked as shortened.
    ShortenTo(usize),
}

/// One blank-line-separated part of a recap, oldest first: person messages
/// whose turn detail no longer fits, then whole turns.
enum Block {
    Said(String),
    Turn(String),
}

/// The recap text: the header, any carried earlier summary, what people said
/// in turns whose detail no longer fits, one block per later turn, and the
/// running turn's results when `current_from` names where its steps start.
fn render(entries: &[Entry], carry: Carry, current_from: Option<usize>) -> Option<String> {
    let mut carried = None;
    let mut dropped = 0usize;
    let mut blocks: Vec<Block> = Vec::new();
    let mut turn: Option<Turn> = None;
    let mut current = Current::default();
    for (index, entry) in entries.iter().enumerate() {
        let in_current = current_from.is_some_and(|from| index >= from);
        if in_current && let Some(done) = turn.take() {
            blocks.push(Block::Turn(done.render()));
        }
        match &entry.message {
            Message::Summary { text } => {
                let earlier = earlier(text);
                blocks.extend(earlier.blocks);
                dropped += earlier.left_out;
                if in_current {
                    // A recap made earlier in this same turn already holds
                    // some of its results.
                    current.carried = earlier.current;
                }
                if let Some(foreign) = earlier.foreign {
                    let foreign = match carry {
                        Carry::DeclineOver(max) if foreign.len() > max => return None,
                        Carry::ShortenTo(max) if foreign.len() > max => {
                            format!("{} (shortened)", clip(&foreign, max))
                        }
                        _ => foreign,
                    };
                    carried = Some(one_block(&foreign));
                }
            }
            Message::User { text, .. } => {
                if let Some(done) = turn.take() {
                    blocks.push(Block::Turn(done.render()));
                }
                turn = Some(Turn {
                    cursor: entry.cursor.0,
                    asked: clip(text.trim(), ASKED_BYTES),
                    ..Turn::default()
                });
            }
            Message::Assistant { text, calls, .. } => {
                if in_current {
                    current.from.get_or_insert(entry.cursor.0);
                    for call in calls {
                        current
                            .calls
                            .push((call.id.to_string(), use_label(&call.tool, &call.args)));
                    }
                    continue;
                }
                let turn = turn.get_or_insert_with(|| Turn {
                    cursor: entry.cursor.0,
                    ..Turn::default()
                });
                for call in calls {
                    turn.calls
                        .push((call.id.to_string(), use_label(&call.tool, &call.args)));
                }
                if calls.is_empty() && !text.trim().is_empty() {
                    turn.reply = Some(text.trim().to_owned());
                }
            }
            Message::Tool {
                call,
                outcome,
                output,
                ..
            } => {
                if in_current {
                    current.results.push((
                        call.to_string(),
                        outcome_word(outcome, output),
                        returned(output),
                    ));
                } else if let Some(turn) = turn.as_mut() {
                    turn.outcomes
                        .push((call.to_string(), outcome_word(outcome, output)));
                }
            }
        }
    }
    if let Some(done) = turn.take() {
        blocks.push(Block::Turn(done.render()));
    }
    let current = current.render();
    // Turn detail is budgeted on its own. A turn that no longer fits keeps
    // what the person said and loses only Dex's side of it.
    let fixed = HEADER.len() + 64;
    let turn_bytes = |blocks: &[Block]| -> usize {
        blocks
            .iter()
            .map(|block| match block {
                Block::Turn(text) => text.len() + 2,
                Block::Said(_) => 0,
            })
            .sum()
    };
    while fixed + turn_bytes(&blocks) > RECAP_BYTES {
        let turns = blocks
            .iter()
            .filter(|block| matches!(block, Block::Turn(_)))
            .count();
        // The latest turn keeps its detail unless the running turn follows it.
        if turns <= usize::from(current.is_none()) {
            break;
        }
        let Some(at) = blocks
            .iter()
            .position(|block| matches!(block, Block::Turn(_)))
        else {
            break;
        };
        let Block::Turn(text) = &blocks[at] else {
            break;
        };
        match said(text) {
            Some(said) => blocks[at] = Block::Said(said),
            None => {
                blocks.remove(at);
                dropped += 1;
            }
        }
    }
    // What people said has its own budget: the oldest messages are shortened
    // to their first lines, and only then left out, oldest first.
    let said_bytes = |blocks: &[Block]| -> usize {
        blocks
            .iter()
            .map(|block| match block {
                Block::Said(text) => text.len() + 2,
                Block::Turn(_) => 0,
            })
            .sum()
    };
    for at in 0..blocks.len() {
        if said_bytes(&blocks) <= SAID_BYTES {
            break;
        }
        if let Block::Said(text) = &mut blocks[at]
            && text.len() > SAID_LEAD_BYTES
        {
            *text = clip(text, SAID_LEAD_BYTES);
        }
    }
    while said_bytes(&blocks) > SAID_BYTES {
        let Some(at) = blocks
            .iter()
            .position(|block| matches!(block, Block::Said(_)))
        else {
            break;
        };
        blocks.remove(at);
        dropped += 1;
    }
    if blocks.is_empty() && current.is_none() {
        return None;
    }
    let mut text = String::from(HEADER);
    if let Some(carried) = carried {
        text.push_str("\n\nEarlier summary: ");
        text.push_str(&carried);
    }
    if dropped > 0 {
        text.push_str(&format!(
            "\n\n{dropped} earlier turn{} {LEFT_OUT_TAIL}",
            if dropped == 1 { " is" } else { "s are" }
        ));
    }
    for block in blocks {
        let (Block::Said(block) | Block::Turn(block)) = block;
        text.push_str("\n\n");
        text.push_str(&block);
    }
    if let Some(current) = current {
        text.push_str("\n\n");
        text.push_str(&current);
    }
    Some(text)
}

#[derive(Default)]
struct Turn {
    cursor: i64,
    asked: String,
    calls: Vec<(String, String)>,
    outcomes: Vec<(String, &'static str)>,
    reply: Option<String>,
}

impl Turn {
    fn render(&self) -> String {
        let mut lines = vec![format!("{TURN_PREFIX}{}:", self.cursor)];
        if !self.asked.is_empty() {
            // Later lines of what the person said are indented, so no line
            // of theirs reads as one of the recap's own.
            lines.push(format!(
                "- Asked: {}",
                one_block(&self.asked).replace('\n', "\n  ")
            ));
        }
        let mut uses: Vec<String> = Vec::new();
        for (id, label) in &self.calls {
            let outcome = self
                .outcomes
                .iter()
                .find(|(call, _)| call == id)
                .map_or("no result", |(_, word)| word);
            let used = format!("{label} ({outcome})");
            if !uses.contains(&used) {
                uses.push(used);
            }
        }
        if !uses.is_empty() {
            let more = uses.len().saturating_sub(USES_PER_TURN);
            uses.truncate(USES_PER_TURN);
            let mut line = format!("{USED_LINE}{}", uses.join("; "));
            if more > 0 {
                line.push_str(&format!("; and {more} more"));
            }
            lines.push(line);
        }
        if let Some(reply) = &self.reply {
            if reply.len() <= KEEP_REPLY_BYTES {
                lines.push(format!("{REPLIED_LINE}: {reply}"));
            } else {
                lines.push(format!(
                    "{REPLIED_LINE} ({} characters, shortened): {}",
                    reply.chars().count(),
                    lead(reply)
                ));
            }
        }
        one_block(&lines.join("\n"))
    }
}

const ASKED_LINE: &str = "- Asked: ";
const USED_LINE: &str = "- Dex used: ";
const REPLIED_LINE: &str = "- Dex replied";

/// What the person said in a turn block, as its own block; `None` when the
/// turn holds no person message.
fn said(turn: &str) -> Option<String> {
    let mut lines = turn.lines();
    let cursor = lines
        .next()?
        .strip_prefix(TURN_PREFIX)?
        .strip_suffix(':')?
        .to_owned();
    let first = lines.next()?.strip_prefix(ASKED_LINE)?;
    let mut text = format!("{SAID_PREFIX}{cursor}: {first}");
    for line in lines {
        if line.starts_with(USED_LINE) || line.starts_with(REPLIED_LINE) {
            break;
        }
        text.push('\n');
        text.push_str(line);
    }
    Some(text)
}

/// The running turn's steps a fallback recap covers.
#[derive(Default)]
struct Current {
    from: Option<i64>,
    /// The same block from a recap made earlier in this turn.
    carried: Option<String>,
    calls: Vec<(String, String)>,
    results: Vec<(String, &'static str, String)>,
}

impl Current {
    fn render(&self) -> Option<String> {
        if self.calls.is_empty() && self.carried.is_none() {
            return None;
        }
        let mut lines = Vec::new();
        let mut budget = CURRENT_RESULTS_BYTES;
        match &self.carried {
            Some(carried) => {
                let carried = clip(carried, CURRENT_RESULTS_BYTES / 2);
                budget -= carried.len().min(budget);
                lines.push(carried);
            }
            None => lines.push(format!(
                "{CURRENT_PREFIX} (from cursor {}), each result as it was returned:",
                self.from.unwrap_or_default()
            )),
        }
        let share = budget / self.calls.len().max(1);
        for (id, label) in &self.calls {
            match self.results.iter().find(|(call, _, _)| call == id) {
                Some((_, outcome, returned)) if returned.is_empty() => {
                    lines.push(format!("- {label} ({outcome})"));
                }
                Some((_, outcome, returned)) => lines.push(format!(
                    "- {label} ({outcome}): {}",
                    clip(returned, share).replace('\n', "\n  ")
                )),
                None => lines.push(format!("- {label} (no result)")),
            }
        }
        Some(one_block(&lines.join("\n")))
    }
}

/// What a tool returned, as far as the log holds it inline. A stored result
/// is named by its reference, which the envelope keeps.
fn returned(output: &Output) -> String {
    match output {
        Output::Text(text) => text.trim().to_owned(),
        Output::Ref(reference) => format!("stored as {reference}"),
        Output::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                crate::OutputBlock::Text { text } => Some(text.trim()),
                crate::OutputBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// What an earlier summary holds.
#[derive(Default)]
struct Earlier {
    /// Person messages and turn blocks of an earlier recap.
    blocks: Vec<Block>,
    /// The text of an earlier summary that is not a recap.
    foreign: Option<String>,
    /// How many turns the earlier recap had already left out.
    left_out: usize,
    /// The running turn's results an earlier recap kept.
    current: Option<String>,
}

fn earlier(text: &str) -> Earlier {
    let summary = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value["summary"].as_str().map(str::to_owned));
    match summary {
        Some(summary) if summary.starts_with(HEADER) || summary.starts_with(LEGACY_HEADER) => {
            let mut earlier = Earlier::default();
            for block in summary.split("\n\n").skip(1) {
                if block.starts_with(TURN_PREFIX) {
                    earlier.blocks.push(Block::Turn(block.to_owned()));
                } else if block.starts_with(SAID_PREFIX) {
                    earlier.blocks.push(Block::Said(block.to_owned()));
                } else if block.starts_with(CURRENT_PREFIX) {
                    earlier.current = Some(block.to_owned());
                } else if let Some(foreign) = block.strip_prefix("Earlier summary: ") {
                    earlier.foreign = Some(foreign.to_owned());
                } else if block.ends_with(LEFT_OUT_TAIL) || block.ends_with(LEGACY_LEFT_OUT_TAIL) {
                    earlier.left_out = block
                        .split_whitespace()
                        .next()
                        .and_then(|count| count.parse().ok())
                        .unwrap_or(0);
                }
            }
            earlier
        }
        Some(summary) => Earlier {
            foreign: Some(summary),
            ..Earlier::default()
        },
        None => Earlier {
            foreign: Some(text.to_owned()),
            ..Earlier::default()
        },
    }
}

/// The tool and the first identifying string argument, e.g. `read GetOverview`.
fn use_label(tool: &crate::ToolName, args: &Value) -> String {
    let target = ["op", "ref", "operation", "name", "query"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(|target| clip(target.trim(), 80));
    match target {
        Some(target) if !target.is_empty() => format!("{tool} {target}"),
        _ => tool.to_string(),
    }
}

fn outcome_word(outcome: &Outcome, output: &Output) -> &'static str {
    match outcome {
        Outcome::Succeeded if matches!(output, Output::Ref(_)) => "ok, stored result",
        Outcome::Succeeded => "ok",
        Outcome::Failed => "failed",
        Outcome::Running => "running",
        Outcome::Unknown => "outcome unknown",
    }
}

/// The reply's first lines: up to [`REPLY_LEAD_BYTES`], ended at the last
/// sentence or line break inside that window when there is one.
fn lead(reply: &str) -> String {
    let window = clip_bytes(reply, REPLY_LEAD_BYTES);
    let end = window
        .rfind(['\n'])
        .into_iter()
        .chain(window.rfind(". ").map(|at| at + 1))
        .max()
        .filter(|&at| at >= REPLY_LEAD_BYTES / 3);
    match end {
        Some(at) => format!("{} …", window[..at].trim_end()),
        None => format!("{} …", window.trim_end()),
    }
}

/// Blank lines separate blocks in a recap, so none may appear inside one.
fn one_block(text: &str) -> String {
    let mut text = text.replace("\r\n", "\n");
    while text.contains("\n\n") {
        text = text.replace("\n\n", "\n");
    }
    text
}

/// `text` up to `max` bytes at a character boundary, marked when cut.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    format!("{} …", clip_bytes(text, max).trim_end())
}

fn clip_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
#[path = "recap_tests.rs"]
mod tests;
