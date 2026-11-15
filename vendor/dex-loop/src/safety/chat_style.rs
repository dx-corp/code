//! The house style of a reply Dex posts in a chat channel (Slack, Teams, a
//! text message), checked and repaired without a model.
//!
//! [`crate::voice_check`] holds what no message may say. This holds the
//! shape of an answer in chat, where a person reads a message and not a
//! report: the answer comes first, it is about four sentences or five
//! bullets (fewer when the person asked for short, any length when they
//! asked for the detail), it has no headings or bold-label sections, it
//! shows no internal identifier, and when Dex cannot do something it says
//! what it can do instead.
//!
//! [`check`] reports each defect once, in [`Defect`] order. [`rewrite`]
//! repairs what it can without losing a fact or inventing a word: an
//! identifier in brackets goes, a sentence that only states one goes, a
//! heading line goes, a bold label loses its bold, and a sentence that is
//! only a disclaimer or a nameless offer goes. Length, a buried answer and a
//! bare refusal need the writer, so they are reported.
//!
//! The recorded cases in `evals/dex-voice/chat_cases.toml` are replies Dex
//! posted to the founder's Slack DM and the messages the style aims for.

use crate::voice_check::{is_filler, line_sentences, sentences, split_links};

/// One way a chat reply misses the house style.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Defect {
    /// The reply opens with a heading or a preamble ("Based on ...",
    /// "Here is ...", "Understood.") instead of the answer.
    BuriedLead,
    /// A heading line, or two or more list items that open with a bold
    /// label ("*Scope Limits:* ...").
    SectionLabel,
    /// An identifier only a machine reads: a connection, organization,
    /// principal or record id (`conn_oauth_0123...`), or a UUID.
    InternalId,
    /// More than the reply's [`Budget`].
    OverBudget,
    /// "I cannot ..." with nothing Dex will do instead and no question.
    BareRefusal,
}

/// Every defect, in order.
pub const ALL: [Defect; 5] = [
    Defect::BuriedLead,
    Defect::SectionLabel,
    Defect::InternalId,
    Defect::OverBudget,
    Defect::BareRefusal,
];

impl Defect {
    /// Stable code for logs and eval cases.
    pub fn code(self) -> &'static str {
        match self {
            Defect::BuriedLead => "buried_lead",
            Defect::SectionLabel => "section_label",
            Defect::InternalId => "internal_id",
            Defect::OverBudget => "over_budget",
            Defect::BareRefusal => "bare_refusal",
        }
    }
}

/// How much a chat reply may hold: sentences outside a list, and list items.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    pub sentences: usize,
    pub items: usize,
}

/// The default: about four sentences, or five bullets.
pub const DEFAULT_BUDGET: Budget = Budget {
    sentences: 4,
    items: 5,
};

/// What "keep it short" gets.
pub const SHORT_BUDGET: Budget = Budget {
    sentences: 2,
    items: 3,
};

/// How a person asks for a short answer.
const SHORT_ASKS: [&str; 11] = [
    "keep it short",
    "keep it brief",
    "short answer",
    "briefly",
    "be brief",
    "in brief",
    "tl;dr",
    "tldr",
    "one line",
    "one sentence",
    "quick answer",
];

/// How a person asks for the detail, a list or a written piece. Such a
/// reply has no budget and keeps its structure.
const DETAIL_ASKS: [&str; 16] = [
    "in detail",
    "detailed",
    "walk me through",
    "step by step",
    "list each",
    "list every",
    "list all",
    "full list",
    "complete list",
    "all of them",
    "for each",
    "for up to",
    "explain",
    "quote",
    "draft",
    "write ",
];

/// The budget for a reply to `question`: none when the person asked for the
/// detail, the short one when they asked for short, else the default.
pub fn budget(question: &str) -> Option<Budget> {
    let asked = question.to_lowercase().replace('’', "'");
    if SHORT_ASKS.iter().any(|ask| asked.contains(ask)) {
        return Some(SHORT_BUDGET);
    }
    if DETAIL_ASKS.iter().any(|ask| asked.contains(ask)) {
        return None;
    }
    Some(DEFAULT_BUDGET)
}

/// Openers that put something before the answer.
const PREAMBLES: [&str; 16] = [
    "based on ",
    "here is ",
    "here's ",
    "here are ",
    "sure",
    "certainly",
    "of course",
    "absolutely",
    "great question",
    "good question",
    "to answer ",
    "let me ",
    "understood",
    "as requested",
    "thanks for",
    "i'd be happy",
];

/// How Dex says it cannot do something.
const REFUSALS: [&str; 9] = [
    "i cannot",
    "i can't",
    "i can not",
    "i am unable",
    "i'm unable",
    "i am not able",
    "i'm not able",
    "i do not have access",
    "i don't have access",
];

/// How Dex says what it will do, has prepared, or offers to do.
const NEXT_STEPS: [&str; 14] = [
    "i can ",
    "i'll ",
    "i will ",
    "i'd ",
    "i would ",
    "i prepared",
    "i've prepared",
    "i have prepared",
    "i drafted",
    "i've drafted",
    "i have drafted",
    "i'm following",
    "i am following",
    "next:",
];

/// Every defect of `text`, a reply to `question`, as the person reads it
/// (Slack mrkdwn, or Markdown).
pub fn check(question: &str, text: &str) -> Vec<Defect> {
    let lines = prose_lines(text);
    let mut found = Vec::new();
    if has_buried_lead(text, &lines) {
        found.push(Defect::BuriedLead);
    }
    let headings = lines.iter().filter(|line| is_heading(line, 1)).count();
    let labels = lines
        .iter()
        .filter(|line| !is_heading(line, 1) && label(line, 1).is_some())
        .count();
    if headings > 0 || labels > 1 {
        found.push(Defect::SectionLabel);
    }
    if !internal_ids(text)
        .iter()
        .all(|id| id_was_asked_for(question, id))
    {
        found.push(Defect::InternalId);
    }
    if let Some(budget) = budget(question) {
        let (sentences, items) = size(&lines);
        if sentences > budget.sentences || items > budget.items {
            found.push(Defect::OverBudget);
        }
    }
    if is_bare_refusal(text) {
        found.push(Defect::BareRefusal);
    }
    found
}

/// `text`, in the Markdown Dex wrote it in, with the defects a rewrite can
/// remove without losing a fact: an internal id in brackets, a sentence
/// whose only fact is an internal id, a heading line and the rule under
/// it, the bold on a label, and a sentence that is only a disclaimer or an
/// offer that names nothing. A reply to a person who asked for the detail
/// keeps its headings and labels; an id the person asked about stays. Code
/// blocks are never touched, and every other word is kept as written.
pub fn rewrite(question: &str, text: &str) -> String {
    let keeps_structure = budget(question).is_none();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut changed = false;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_owned());
            continue;
        }
        if in_fence {
            out.push(line.to_owned());
            continue;
        }
        if !keeps_structure && (is_heading(line, 2) || is_rule(line)) {
            changed = true;
            continue;
        }
        let mut rebuilt = String::new();
        for sentence in line_sentences(line) {
            if is_filler(sentence) {
                continue;
            }
            if let Some(sentence) = without_ids(question, sentence) {
                rebuilt.push_str(&sentence);
            }
        }
        let unlabelled = if keeps_structure {
            None
        } else {
            label(&rebuilt, 2).map(|(prefix, inner, rest)| format!("{prefix}{inner}{rest}"))
        };
        let rebuilt = unlabelled.unwrap_or(rebuilt).trim_end().to_owned();
        if rebuilt == line.trim_end() {
            out.push(line.to_owned());
            continue;
        }
        changed = true;
        // A line left with nothing said nothing else.
        if after_marker(&rebuilt).trim().is_empty() {
            continue;
        }
        out.push(rebuilt);
    }
    if !changed {
        return text.to_owned();
    }
    // Dropping a line leaves no doubled, leading or trailing blank lines.
    let mut tidy: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in out {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.trim().is_empty() && tidy.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        tidy.push(line);
    }
    while tidy.last().is_some_and(|line| line.trim().is_empty()) {
        tidy.pop();
    }
    let rewritten = tidy.join("\n");
    if rewritten.trim().is_empty() {
        return text.to_owned();
    }
    rewritten
}

/// The lines a person reads as prose: not code blocks, not a line that is
/// only a link ("Open in Deixic"), not a rule.
fn prose_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.trim().is_empty() || is_rule(line) || is_only_a_link(line) {
            continue;
        }
        lines.push(line);
    }
    lines
}

/// "---", "***" or "___": a rule between sections.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|mark| line.chars().all(|c| c == *mark))
}

/// A line that is one Slack link and nothing else.
fn is_only_a_link(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('<') && line.ends_with('>') && line.matches('<').count() == 1
}

/// `line` after its list marker ("• ", "- ", "* ", "1. ", "2) "), or the
/// line without leading spaces when it has none. Always a suffix of `line`.
fn after_marker(line: &str) -> &str {
    let line = line.trim_start();
    for bullet in ["• ", "- ", "* ", "◦ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return rest.trim_start();
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=2).contains(&digits) {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

/// Whether `line` is a list item.
fn is_item(line: &str) -> bool {
    after_marker(line).len() != line.trim_start().len()
}

/// The bold span that opens `line` and what follows it. `stars` is the
/// fewest asterisks that mean bold: one in Slack mrkdwn, two in Markdown.
fn leading_bold(line: &str, stars: usize) -> Option<(&str, &str)> {
    let open = line.bytes().take_while(|b| *b == b'*').count();
    if open < stars || open > 2 {
        return None;
    }
    let body = &line[open..];
    let close = body.find(&"*".repeat(open))?;
    let inner = &body[..close];
    if inner.trim().is_empty() || inner.starts_with(' ') {
        return None;
    }
    Some((inner, &body[close + open..]))
}

/// A heading: "# Title", or a line that is one bold span and not a
/// statement ("*What Needs to Change*", "*Can I Do This Myself?*").
fn is_heading(line: &str, stars: usize) -> bool {
    let line = line.trim();
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes)
        && line[hashes..]
            .strip_prefix(' ')
            .is_some_and(|rest| !rest.trim().is_empty())
    {
        return true;
    }
    leading_bold(line, stars).is_some_and(|(inner, rest)| {
        rest.trim().is_empty() && !inner.trim_end().ends_with(['.', '!'])
    })
}

/// A line that opens, after any list marker, with a bold label
/// ("• *Scope Limits:* ..."): everything before the label, the label
/// without its bold, and the rest of the line.
fn label(line: &str, stars: usize) -> Option<(&str, &str, &str)> {
    let body = after_marker(line);
    let prefix = &line[..line.len() - body.len()];
    let (inner, rest) = leading_bold(body, stars)?;
    let is_label = inner.trim_end().ends_with(':') || rest.starts_with(':');
    (is_label && inner.chars().count() <= 80).then_some((prefix, inner, rest))
}

/// Sentences outside list items, and list items. A heading is neither.
fn size(lines: &[&str]) -> (usize, usize) {
    let items = lines.iter().filter(|line| is_item(line)).count();
    let prose: Vec<&str> = lines
        .iter()
        .filter(|line| !is_item(line) && !is_heading(line, 1))
        .copied()
        .collect();
    (sentences(&prose.join("\n")).len(), items)
}

/// Whether the reply opens with a heading or a preamble.
fn has_buried_lead(text: &str, lines: &[&str]) -> bool {
    let Some(first) = lines.first() else {
        return false;
    };
    if is_heading(first, 1) {
        return true;
    }
    let opening = sentences(text)
        .into_iter()
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .replace('’', "'");
    let opening = opening.trim_start_matches(['*', '_']);
    PREAMBLES
        .iter()
        .any(|preamble| opening.starts_with(preamble))
}

/// "I cannot ..." with no next step of Dex's own and no question.
fn is_bare_refusal(text: &str) -> bool {
    let (_, prose) = split_links(text);
    let mut lower = prose.to_lowercase().replace('’', "'");
    if !REFUSALS.iter().any(|refusal| lower.contains(refusal)) {
        return false;
    }
    // "I can not" is no offer.
    for refusal in REFUSALS {
        lower = lower.replace(refusal, " ");
    }
    let asks = sentences(&prose)
        .iter()
        .any(|sentence| sentence.ends_with('?'));
    !asks && !NEXT_STEPS.iter().any(|step| lower.contains(step))
}

/// What separates one word from the next when looking for an identifier.
fn is_id_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | '`' | '(' | ')' | ',' | ';' | '"' | '\'' | '[' | ']'
        )
}

/// The internal identifiers in the prose of `text`: links count as their
/// labels, so an id inside a URL is not one a person reads.
pub fn internal_ids(text: &str) -> Vec<String> {
    let (_, prose) = split_links(text);
    let mut ids: Vec<String> = prose
        .split(is_id_separator)
        .filter_map(internal_id)
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// `word` without the punctuation around it, when it is an internal
/// identifier: a UUID, or a prefix and a long lowercase hex tail
/// (`conn_oauth_0123456789abcdef`, `org_0123456789abcdef`,
/// `source:0123456789abcdef`).
fn internal_id(word: &str) -> Option<&str> {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let hex = |part: &str| {
        part.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let uuid = word.len() == 36
        && word.split('-').map(str::len).eq([8usize, 4, 4, 4, 12])
        && word.split('-').all(hex);
    let tail = word.rsplit(['_', ':']).next().unwrap_or("");
    let prefixed = tail.len() < word.len()
        && tail.len() >= 12
        && hex(tail)
        && tail.bytes().any(|b| b.is_ascii_digit());
    (uuid || prefixed).then_some(word)
}

/// Whether the person asked about ids, or named this one themselves.
fn id_was_asked_for(question: &str, id: &str) -> bool {
    let asked = question.to_lowercase();
    asked.contains(&id.to_lowercase())
        || asked
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| matches!(word, "id" | "ids" | "identifier" | "identifiers"))
}

/// Words that an identifier completes ("... principal ID is `user_0123`.").
const ID_LEADS: [&str; 6] = ["is", "is:", "id", "id:", "as", "was"];

/// One sentence, with the whitespace after it, without its internal ids.
/// An id in brackets is removed with its brackets. A sentence that ends in
/// an id it only introduces is `None`: the id was its only fact. Any other
/// sentence with an id is kept whole, for [`check`] to report.
fn without_ids(question: &str, sentence: &str) -> Option<String> {
    let is_id = |word: &str| {
        // A link's URL may hold an id; the person reads its label.
        !word.contains("](")
            && !word.contains('<')
            && !word.contains("://")
            && word.chars().any(char::is_alphanumeric)
            && word
                .split(is_id_separator)
                .filter(|part| part.chars().any(char::is_alphanumeric))
                .all(|part| internal_id(part).is_some_and(|id| !id_was_asked_for(question, id)))
    };
    let words: Vec<&str> = sentence.split(' ').collect();
    if !words.iter().any(|word| is_id(word)) {
        return Some(sentence.to_owned());
    }
    let trailing = &sentence[sentence.trim_end().len()..];
    let mut kept: Vec<String> = Vec::new();
    for (at, word) in words.iter().enumerate() {
        if !is_id(word) {
            kept.push((*word).to_owned());
            continue;
        }
        let core = word.trim_end();
        let bracketed = core.starts_with('(') && core.contains(')');
        if bracketed {
            // "Deixic (`org_0123`)." keeps the full stop after the bracket.
            let after = &core[core.rfind(')').map_or(core.len(), |close| close + 1)..];
            if !after.is_empty()
                && let Some(last) = kept.last_mut()
            {
                last.push_str(after);
            }
            continue;
        }
        let last_word = words[at + 1..].iter().all(|rest| rest.trim().is_empty());
        let lead = kept
            .last()
            .map(|word| word.trim_matches(['*', '_']).to_lowercase())
            .unwrap_or_default();
        if last_word && ID_LEADS.contains(&lead.as_str()) {
            return None;
        }
        return Some(sentence.to_owned());
    }
    let mut rebuilt = kept.join(" ");
    if !rebuilt.ends_with(trailing) {
        rebuilt = format!("{}{trailing}", rebuilt.trim_end());
    }
    Some(rebuilt)
}
