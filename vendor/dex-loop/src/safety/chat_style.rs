//! The house style of a reply Dex posts in a chat channel (Slack, Teams, a
//! text message), checked and repaired without a model.
//!
//! [`crate::voice_check`] holds what no message may say. This holds the
//! shape of an answer in chat, where a person reads a message and not a
//! report: the answer comes first, it is about four sentences or five
//! bullets (fewer when the person asked for short, any length when they
//! asked for the detail), it has no headings, bold-label sections or bold
//! for emphasis, it shows no internal identifier, when Dex cannot do
//! something it says what it can do instead, and it ends on the answer, a
//! commitment or a question, never on an offer left open. Its reasons are
//! about this person's records, and asked for one thing it gives one.
//!
//! [`check`] reports each defect once, in [`Defect`] order. [`rewrite`]
//! repairs what it can without losing a fact or inventing a word: an
//! identifier in brackets goes, a sentence that only states one goes, a
//! heading line goes, a bold label or phrase loses its bold, and a sentence
//! that is only a disclaimer or a nameless offer goes. Length, a buried
//! answer, a bare refusal, an open offer, general rationale and more than
//! was asked need the writer, so they are reported.
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
    /// Bold on a phrase inside a sentence ("You have *114 failing tests*").
    /// A chat message carries its weight in its words.
    BoldEmphasis,
    /// The reply ends on an offer left open ("I can list them by owner
    /// next."): neither a commitment ("I'll ...") nor a question the person
    /// can answer with yes ("Want me to ...?").
    OpenOffer,
    /// Rationale that would be true of any company ("Auditors scrutinize
    /// ...", "introduces an immediate risk of ..."). The why is one
    /// sentence about this person's records.
    GenericRationale,
    /// The person asked for one thing ("the one thing I should fix first")
    /// and the reply goes on to others ("the next highest priority items
    /// are ...").
    BeyondTheAsk,
    /// A name from our side in code, a read, field or enum the person never
    /// sees (`employment_facts`, `state: UNKNOWN`, `PERMISSION_DENIED`).
    /// Rewritten into its plain words.
    InternalName,
    /// Our own fault told to the person: a read, lookup or service of ours
    /// that is unavailable, failing or timed out ("personnel directory reads
    /// are currently unavailable"). Outages never reach customers: the
    /// rewrite says the answer is still updating instead.
    PlatformFault,
}

/// Every defect, in order.
pub const ALL: [Defect; 11] = [
    Defect::BuriedLead,
    Defect::SectionLabel,
    Defect::InternalId,
    Defect::OverBudget,
    Defect::BareRefusal,
    Defect::BoldEmphasis,
    Defect::OpenOffer,
    Defect::GenericRationale,
    Defect::BeyondTheAsk,
    Defect::InternalName,
    Defect::PlatformFault,
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
            Defect::BoldEmphasis => "bold_emphasis",
            Defect::OpenOffer => "open_offer",
            Defect::GenericRationale => "generic_rationale",
            Defect::BeyondTheAsk => "beyond_the_ask",
            Defect::InternalName => "internal_name",
            Defect::PlatformFault => "platform_fault",
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

/// How a closing sentence offers work without committing to it or asking.
const OPEN_OFFERS: [&str; 9] = [
    "i can ",
    "i could ",
    "i'm happy to ",
    "i am happy to ",
    "if you'd like",
    "if you would like",
    "if you want",
    "if it helps",
    "if helpful",
];

/// Rationale that holds for any company and says nothing of this one.
const GENERIC_RATIONALE: [&str; 11] = [
    "auditors scrutinize",
    "auditors strictly",
    "auditors look for",
    "introduces an immediate risk",
    "introduces severe",
    "principle of least privilege",
    "best practice",
    "why it matters",
    "attack surface",
    "blast radius",
    "it is important to",
];

/// How a person asks for one thing.
const ONE_ASKS: [&str; 8] = [
    "one thing",
    "single most",
    "single thing",
    "most important",
    "most urgent",
    "top priority",
    "fix first",
    "do first",
];

/// How a reply moves on to things nobody asked about.
const FURTHER_ITEMS: [&str; 10] = [
    "next highest",
    "next priorit",
    "other priorit",
    "additional high",
    "additional priorit",
    "additional item",
    "to address next",
    "beyond that",
    "you should also",
    "second priority",
];

/// What our side is called when a sentence reports it failing: a read, a
/// lookup, a service. A person's own connection ("the Okta connection
/// failed to sync") is theirs to fix and is not one of these.
const OUR_SIDE: [&str; 12] = [
    " read ",
    " reads ",
    " reads,",
    "read is ",
    "read was ",
    "lookup",
    "backend",
    "our side",
    "on our end",
    "internal service",
    "our service",
    "endpoint",
];

/// How a sentence says a read or service of ours is failing. "Failing"
/// alone describes a person's tests, so it is not one of these.
const FAULTS: [&str; 14] = [
    "unavailable",
    "read failed",
    "reads failed",
    "lookup failed",
    "could not be read",
    "couldn't be read",
    "erroring",
    "returned an error",
    "timed out",
    "timing out",
    " is down",
    " are down",
    "outage",
    "not responding",
];

/// What a reply says in place of our own fault: the answer may be
/// incomplete, and nothing about why.
pub const STILL_UPDATING: &str = "Part of this is still updating.";

/// How a person asks what Dex is able to do. "I can ..." answers that.
const CAPABILITY_ASKS: [&str; 6] = [
    "can you",
    "could you",
    "what can",
    "are you able",
    "able to",
    "something you can do",
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
    if lines.iter().any(|line| has_bold_emphasis(line, 1)) {
        found.push(Defect::BoldEmphasis);
    }
    if ends_on_open_offer(question, text, &lines) {
        found.push(Defect::OpenOffer);
    }
    let asked = question.to_lowercase().replace('’', "'");
    let (_, prose) = split_links(&lines.join("\n"));
    let prose = prose.to_lowercase().replace('’', "'");
    // A person who asked why, or for the detail, is owed the reasoning.
    let wants_reasons = budget(question).is_none() || asked.contains("why");
    if !wants_reasons
        && GENERIC_RATIONALE
            .iter()
            .any(|phrase| prose.contains(phrase))
    {
        found.push(Defect::GenericRationale);
    }
    if ONE_ASKS.iter().any(|ask| asked.contains(ask))
        && FURTHER_ITEMS.iter().any(|phrase| prose.contains(phrase))
    {
        found.push(Defect::BeyondTheAsk);
    }
    if lines
        .iter()
        .any(|line| plain_names(question, line) != *line)
    {
        found.push(Defect::InternalName);
    }
    if lines
        .iter()
        .flat_map(|line| line_sentences(line))
        .any(is_platform_fault)
    {
        found.push(Defect::PlatformFault);
    }
    found
}

/// Whether `sentence` tells the person that a read, lookup or service of
/// ours is unavailable or failing.
fn is_platform_fault(sentence: &str) -> bool {
    let lower = format!(" {} ", sentence.to_lowercase().replace('`', " "));
    OUR_SIDE.iter().any(|side| lower.contains(side)) && FAULTS.iter().any(|f| lower.contains(f))
}

/// Whether the content of a code span is a name from our side: a snake_case
/// read or field (`employment_facts`), a SCREAMING_CASE enum
/// (`PERMISSION_DENIED`) or a field with its enum value (`state: UNKNOWN`).
fn is_internal_name(code: &str) -> bool {
    let screaming = || {
        let parts: Vec<&str> = code.split('_').collect();
        parts.len() >= 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            })
            && code.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
    };
    let field = || {
        code.split_once(": ").is_some_and(|(name, value)| {
            !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                && value.len() >= 2
                && value
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
        })
    };
    crate::human_text::is_field_name(code) || screaming() || field()
}

/// Whether the person asked in our terms: code, a field, an API, or this
/// very name.
fn name_was_asked_for(question: &str, code: &str) -> bool {
    let asked = question.to_lowercase();
    question.contains('`')
        || asked.contains(&code.to_lowercase())
        || asked
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| {
                matches!(
                    word,
                    "field" | "fields" | "column" | "api" | "json" | "schema" | "enum"
                )
            })
}

/// `line` with each internal name in a code span written as its plain
/// words: `employment_facts` -> employment facts, `state: UNKNOWN` ->
/// state unknown. Every other code span, and a name the person asked
/// about, stays as written.
fn plain_names(question: &str, line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(len) = rest[open + 1..].find('`') else {
            break;
        };
        let code = &rest[open + 1..open + 1 + len];
        out.push_str(&rest[..open]);
        if is_internal_name(code) && !name_was_asked_for(question, code) {
            out.push_str(&code.replace(": ", " ").replace('_', " ").to_lowercase());
        } else {
            out.push_str(&rest[open..open + len + 2]);
        }
        rest = &rest[open + len + 2..];
    }
    out.push_str(rest);
    out
}

/// `text`, in the Markdown Dex wrote it in, with the defects a rewrite can
/// remove without losing a fact: an internal id in brackets, a sentence
/// whose only fact is an internal id, a heading line and the rule under
/// it, the bold on a label or a phrase, a sentence that is only a
/// disclaimer or an offer that names nothing, and an internal name in code
/// (written as its plain words). A sentence telling the person that a read
/// or service of ours is failing becomes [`STILL_UPDATING`], once. A reply to a person who asked
/// for the detail keeps its headings, labels and bold; an id the person
/// asked about stays. Code blocks are never touched, and every other word
/// is kept as written.
pub fn rewrite(question: &str, text: &str) -> String {
    let keeps_structure = budget(question).is_none();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut changed = false;
    let mut said_updating = false;
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
            // Our own fault never reaches the person: the answer is still
            // updating, said once, whatever the fault was.
            if is_platform_fault(sentence) {
                if !said_updating {
                    said_updating = true;
                    let trailing = &sentence[sentence.trim_end().len()..];
                    rebuilt.push_str(STILL_UPDATING);
                    rebuilt.push_str(trailing);
                }
                continue;
            }
            let sentence = &plain_names(question, sentence);
            if let Some(sentence) = without_ids(question, sentence) {
                rebuilt.push_str(&sentence);
            }
        }
        let unlabelled = if keeps_structure {
            None
        } else {
            label(&rebuilt, 2).map(|(prefix, inner, rest)| format!("{prefix}{inner}{rest}"))
        };
        let rebuilt = unlabelled.unwrap_or(rebuilt);
        let rebuilt = if keeps_structure {
            rebuilt
        } else {
            without_bold(&rebuilt, 2)
        };
        let rebuilt = rebuilt.trim_end().to_owned();
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

/// The bold spans of `line` as byte ranges, markers included, outside code
/// spans and Slack links. `stars` is the marker: one asterisk in Slack
/// mrkdwn, two in Markdown. A span opens on a marker that starts a word and
/// closes on one that ends a word, so "2 * 3", "*.rs" and "s3:Get*" are not
/// bold.
fn bold_spans(line: &str, stars: usize) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let marker = |at: usize| {
        at + stars <= bytes.len()
            && bytes[at..at + stars].iter().all(|b| *b == b'*')
            && bytes.get(at + stars) != Some(&b'*')
            && (at == 0 || bytes[at - 1] != b'*')
    };
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    let (mut in_code, mut in_link) = (false, false);
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'`' => in_code = !in_code,
            b'<' if !in_code => in_link = true,
            b'>' if !in_code => in_link = false,
            b'*' if !in_code && !in_link && marker(at) => {
                let before = line[..at].chars().next_back();
                let after = line[at + stars..].chars().next();
                match open {
                    None if after.is_some_and(|c| !c.is_whitespace())
                        && before.is_none_or(|c| !c.is_alphanumeric()) =>
                    {
                        open = Some(at);
                    }
                    Some(start)
                        if before.is_some_and(|c| !c.is_whitespace())
                            && after.is_none_or(|c| !c.is_alphanumeric())
                            && at > start + stars =>
                    {
                        spans.push((start, at + stars));
                        open = None;
                    }
                    _ => {}
                }
                at += stars;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    spans
}

/// Whether `line` has bold that is neither a heading nor the label that
/// opens it: emphasis inside a sentence.
fn has_bold_emphasis(line: &str, stars: usize) -> bool {
    if is_heading(line, stars) {
        return false;
    }
    let body = label(line, stars).map_or_else(|| after_marker(line), |(_, _, rest)| rest);
    !bold_spans(body, stars).is_empty()
}

/// `line` with the markers of its bold spans removed and every word kept.
fn without_bold(line: &str, stars: usize) -> String {
    let body = after_marker(line);
    let prefix = &line[..line.len() - body.len()];
    let mut out = String::from(prefix);
    let mut from = 0;
    for (start, end) in bold_spans(body, stars) {
        out.push_str(&body[from..start]);
        out.push_str(&body[start + stars..end - stars]);
        from = end;
    }
    out.push_str(&body[from..]);
    out
}

/// Whether the reply ends on an offer left open: its last sentence offers
/// work ("I can ... next.") without committing to it or asking. Not when the
/// person asked what Dex can do, and not beside a refusal or a question,
/// where "I can ..." is the thing Dex will do instead.
fn ends_on_open_offer(question: &str, text: &str, lines: &[&str]) -> bool {
    let asked = question.to_lowercase().replace('’', "'");
    if CAPABILITY_ASKS.iter().any(|ask| asked.contains(ask)) {
        return false;
    }
    let (_, prose) = split_links(&lines.join("\n"));
    let all = sentences(&prose);
    let Some(last) = all.last() else {
        return false;
    };
    if all.iter().any(|sentence| sentence.ends_with('?')) {
        return false;
    }
    let (_, whole) = split_links(text);
    let whole = whole.to_lowercase().replace('’', "'");
    if REFUSALS.iter().any(|refusal| whole.contains(refusal)) {
        return false;
    }
    let last = last.to_lowercase().replace('’', "'");
    let last = last.trim_start_matches(|c: char| !c.is_alphanumeric());
    OPEN_OFFERS.iter().any(|offer| last.starts_with(offer))
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
    // "I can not" is no offer, and "so I can inspect the logs" is what the
    // missing access is for, not a next step.
    for refusal in REFUSALS {
        lower = lower.replace(refusal, " ");
    }
    for purpose in ["so i can ", "so that i can "] {
        lower = lower.replace(purpose, " ");
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
