//! A deterministic recap of a thread's finished turns.
//!
//! Size-based compaction ([`crate::Threshold`]) waits until history nears the
//! model's budget, which a short thread never reaches. Until then every
//! earlier Dex reply goes back to the model word for word, and the model
//! reuses its own long answer as the template for the next one. A recap
//! replaces the finished turns with what was asked, what Dex used and the
//! first lines of each reply, as soon as one reply since the last compaction
//! is long. It runs no model and costs nothing.
//!
//! The recap is an ordinary [`crate::Compaction`]: the same event, storage and
//! turn-boundary apply rule as a model summary, in the same envelope, so
//! attachment and stored-output references survive it exactly. It covers
//! every finished assistant step, so no signed step is replayed beside a
//! prefix it was not produced with.
//!
//! Nothing is planned when no reply since the last compaction is long, so an
//! unchanged thread writes nothing (`docs/formal/dex_turn_recap`).

use serde_json::Value;

use crate::{Compaction, Context, Entry, Message, Outcome, Output};

/// A finished reply at least this long triggers a recap.
pub const RECAP_REPLY_BYTES: usize = 1200;
/// History not yet compacted past this many bytes (about 40k tokens at four
/// bytes a token) triggers a recap whatever its replies, so a long thread of
/// short turns never carries hundreds of thousands of tokens into every turn
/// on its way to the model's limit.
pub const RECAP_HISTORY_BYTES: usize = 160 * 1024;
/// The recap text (without its envelope) stays under this.
const RECAP_BYTES: usize = 2400;
/// A reply shorter than this is kept whole in the recap.
const KEEP_REPLY_BYTES: usize = 320;
/// The first lines kept of a longer reply.
const REPLY_LEAD_BYTES: usize = 240;
/// The most kept of one person message.
const ASKED_BYTES: usize = 500;
/// Most tool uses named per turn.
const USES_PER_TURN: usize = 6;
/// A prior summary larger than this is left to size-based compaction.
const CARRIED_SUMMARY_BYTES: usize = 6 * 1024;
/// The most of an earlier model summary a fallback recap keeps.
const FALLBACK_CARRIED_BYTES: usize = 2 * 1024;
/// The most attachment references a fallback recap keeps when the exact
/// reference set cannot be formed.
const FALLBACK_REFERENCE_BYTES: usize = 4 * 1024;

const LEFT_OUT_TAIL: &str = "left out; the thread log keeps them.";

const HEADER: &str = "Recap of earlier turns in this conversation. Earlier Dex replies are shortened to their first lines; their full text is in the thread log at the cursor shown, and current facts come from this turn's reads.";

/// The recap to append after a finished turn, or `None` when none is due.
/// History past [`RECAP_HISTORY_BYTES`] is always recapped. Below that, a
/// recap is due when a finished reply since the last compaction reaches
/// [`RECAP_REPLY_BYTES`] and no covered message carries an attachment
/// (size-based compaction keeps those). Nothing is due while a turn is
/// running or has open calls.
// formal: begin dex_turn_recap/TurnRecap.Plan
pub fn recap_after_turn(ctx: &Context) -> Option<Compaction> {
    if ctx.turn_running() || ctx.open_step().is_some() || ctx.open_attempt().is_some() {
        return None;
    }
    let history = ctx.history();
    let cut = history.len();
    if !crate::compaction::valid_cut(history, cut) {
        return None;
    }
    let covered = &history[..cut];
    let uncompacted: usize = history
        .iter()
        .filter(|entry| !matches!(entry.message, Message::Summary { .. }))
        .map(|entry| entry.message.size())
        .sum();
    if uncompacted > RECAP_HISTORY_BYTES {
        // Past the cap the recap replaces the history whatever it holds,
        // attachments included (their references are kept).
        return Some(Compaction {
            covers_to: covered.last()?.cursor,
            summary: fallback(ctx, covered)?,
        });
    }
    let long_reply = history.iter().any(|entry| {
        matches!(&entry.message, Message::Assistant { text, calls, .. }
            if calls.is_empty() && text.len() >= RECAP_REPLY_BYTES)
    });
    if !long_reply {
        return None;
    }
    let attached = history.iter().any(
        |entry| matches!(&entry.message, Message::User { attachments, .. } if !attachments.is_empty()),
    );
    if attached {
        return None;
    }
    let (_, references, from, to) =
        crate::summary::material_with_evidence(covered, ctx.tool_evidence())?;
    let text = render(covered, Carry::DeclineOver(CARRIED_SUMMARY_BYTES))?;
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
/// declines for size: an earlier model summary is shortened, the oldest turn
/// blocks are left out with a note, and when the exact references cannot be
/// kept the attachment references that fit are kept and the note says the
/// thread log holds the rest. `None` only when `entries` holds no turn.
pub(crate) fn fallback(ctx: &Context, entries: &[Entry]) -> Option<String> {
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
    let mut text = render(entries, Carry::ShortenTo(FALLBACK_CARRIED_BYTES))?;
    if !kept_all {
        text.push_str("\n\nSome earlier attachments and stored results are not listed here; the thread log keeps them.");
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

/// The recap text: the header, any carried earlier summary, then one block
/// per turn, oldest blocks dropped first when over [`RECAP_BYTES`].
fn render(entries: &[Entry], carry: Carry) -> Option<String> {
    let mut carried = None;
    let mut dropped = 0usize;
    let mut blocks: Vec<String> = Vec::new();
    let mut turn: Option<Turn> = None;
    for entry in entries {
        match &entry.message {
            Message::Summary { text } => {
                let (earlier_blocks, foreign, left_out) = earlier(text);
                blocks.extend(earlier_blocks);
                dropped += left_out;
                if let Some(foreign) = foreign {
                    let foreign = match carry {
                        Carry::DeclineOver(max) if foreign.len() > max => return None,
                        Carry::ShortenTo(max) if foreign.len() > max => format!(
                            "{} (shortened; the thread log keeps it)",
                            clip(&foreign, max)
                        ),
                        _ => foreign,
                    };
                    carried = Some(one_block(&foreign));
                }
            }
            Message::User { text, .. } => {
                if let Some(done) = turn.take() {
                    blocks.push(done.render());
                }
                turn = Some(Turn {
                    cursor: entry.cursor.0,
                    asked: clip(text.trim(), ASKED_BYTES),
                    ..Turn::default()
                });
            }
            Message::Assistant { text, calls, .. } => {
                let current = turn.get_or_insert_with(|| Turn {
                    cursor: entry.cursor.0,
                    ..Turn::default()
                });
                for call in calls {
                    current
                        .calls
                        .push((call.id.to_string(), use_label(&call.tool, &call.args)));
                }
                if calls.is_empty() && !text.trim().is_empty() {
                    current.reply = Some((entry.cursor.0, text.trim().to_owned()));
                }
            }
            Message::Tool {
                call,
                outcome,
                output,
                ..
            } => {
                if let Some(current) = turn.as_mut() {
                    current
                        .outcomes
                        .push((call.to_string(), outcome_word(outcome, output)));
                }
            }
        }
    }
    if let Some(done) = turn.take() {
        blocks.push(done.render());
    }
    // The header and the left-out line (at most about 64 bytes) are always
    // budgeted; a carried summary has its own bound and does not crowd out
    // the turns.
    let fixed = HEADER.len() + 64;
    while blocks.len() > 1
        && fixed + blocks.iter().map(|b| b.len() + 2).sum::<usize>() > RECAP_BYTES
    {
        blocks.remove(0);
        dropped += 1;
    }
    if blocks.is_empty() {
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
        text.push_str("\n\n");
        text.push_str(&block);
    }
    Some(text)
}

#[derive(Default)]
struct Turn {
    cursor: i64,
    asked: String,
    calls: Vec<(String, String)>,
    outcomes: Vec<(String, &'static str)>,
    reply: Option<(i64, String)>,
}

impl Turn {
    fn render(&self) -> String {
        let mut lines = vec![format!("Turn at cursor {}:", self.cursor)];
        if !self.asked.is_empty() {
            lines.push(format!("- Asked: {}", self.asked));
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
            let mut line = format!("- Dex used: {}", uses.join("; "));
            if more > 0 {
                line.push_str(&format!("; and {more} more"));
            }
            lines.push(line);
        }
        if let Some((cursor, reply)) = &self.reply {
            if reply.len() <= KEEP_REPLY_BYTES {
                lines.push(format!("- Dex replied: {reply}"));
            } else {
                lines.push(format!(
                    "- Dex replied ({} characters, shortened; full text at cursor {cursor}): {}",
                    reply.chars().count(),
                    lead(reply)
                ));
            }
        }
        one_block(&lines.join("\n"))
    }
}

/// Turn blocks of an earlier recap, the text of any other earlier summary,
/// and how many turns the earlier recap had already left out.
fn earlier(text: &str) -> (Vec<String>, Option<String>, usize) {
    let summary = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value["summary"].as_str().map(str::to_owned));
    match summary {
        Some(summary) if summary.starts_with(HEADER) => {
            let mut blocks = Vec::new();
            let mut carried = None;
            let mut left_out = 0;
            for block in summary.split("\n\n").skip(1) {
                if block.starts_with("Turn at cursor ") {
                    blocks.push(block.to_owned());
                } else if let Some(earlier) = block.strip_prefix("Earlier summary: ") {
                    carried = Some(earlier.to_owned());
                } else if block.ends_with(LEFT_OUT_TAIL) {
                    left_out = block
                        .split_whitespace()
                        .next()
                        .and_then(|count| count.parse().ok())
                        .unwrap_or(0);
                }
            }
            (blocks, carried, left_out)
        }
        Some(summary) => (Vec::new(), Some(summary), 0),
        None => (Vec::new(), Some(text.to_owned()), 0),
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
