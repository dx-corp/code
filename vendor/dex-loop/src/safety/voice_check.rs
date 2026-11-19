//! Mechanical checks of a message Dex starts itself against its voice
//! standard (the Voice section of Dex's shared prompt).
//!
//! The prompt carries the judgment: what is worth an interruption, what to
//! name, what to leave out. Whatever a person can spot without judgment is
//! checked here instead: a field name, a raw timestamp, a code span or an HTTP
//! status in prose; words about Dex's own machinery ("the read data", "your
//! cutoff"); a boilerplate disclaimer; a generic offer, or any question when
//! the note says nothing needs the person; a pending question that asks for
//! more than one answer; a link labelled with its own URL;
//! two times stacked on one fact; "I closed out" followed by a failure; a
//! completion claim without its matching receipt; an exclamation mark; a
//! deadline with no date; a catalog test id or a record's numeric id.
//!
//! The checks live in the loop because every message Dex writes leaves
//! through it, whatever surface delivers it: [`standard`] is what the shared
//! reply path applies, by [`Occasion`], and [`rewrite`] is what it can repair
//! without a model.
//!
//! [`check`] reports each defect once, in the order of [`Defect`].
//! [`check_answer`] also grades a reply against the question it answers: asked
//! what matters most, Dex leads with one item and says how many more there
//! are, never the whole list. The offline cases in `evals/dex-voice/cases.toml`
//! grade recorded messages with them.

use crate::human_text::{MachineTerm, machine_terms};
use std::collections::{BTreeMap, BTreeSet};

/// One kind of defect a person can see without judgment.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Defect {
    /// A record field name in prose (`waiting_since`).
    FieldName,
    /// An RFC 3339 timestamp in prose.
    RawTimestamp,
    /// A backtick code span: a value only a machine reads.
    CodeSpan,
    /// "HTTP 500".
    HttpStatus,
    /// Words about Dex's reads rather than the person's records.
    Machinery,
    /// "this isn't a verdict on your security or compliance".
    Disclaimer,
    /// An offer that names nothing ("Want me to fix it?").
    GenericOffer,
    /// A question in a note that says nothing needs the person.
    QuestionWithNothingToDo,
    /// A pending question that asks the person for more than one answer.
    MultipleAsks,
    /// `<url|url>`: the link's label is its URL.
    UrlLabel,
    /// "since 13:11 UTC (Nov 12, 14:37 UTC)": a second time on one fact.
    StackedTimes,
    /// "I closed out compliance checks haven't run": a failure as a fix.
    BrokenClosedOut,
    /// An exclamation mark.
    Exclamation,
    /// "deadline: due": a deadline without a date.
    DeadlineWithoutDate,
    /// A catalog test id ("github-repo-internal-exposed") or a record's
    /// numeric id ("repository ID 1043229871"): names only a machine reads.
    InternalId,
    /// Asked what matters most, a list of several items instead of one item
    /// with how many more there are.
    ManyAtOnce,
    /// An alert longer than three sentences, not counting one closing offer
    /// line ("I'd <action> for <named records>. Want me to?").
    LongAlert,
    /// A URL shown as itself: a link with no label, or a URL left in the
    /// prose, instead of the record's name with the link on it.
    BareUrl,
    /// A first-person completion claim without its own matching owner receipt.
    UnreceiptedCompletionClaim,
}

impl Defect {
    /// Stable code for logs and eval cases.
    pub fn code(self) -> &'static str {
        match self {
            Defect::FieldName => "field_name",
            Defect::RawTimestamp => "raw_timestamp",
            Defect::CodeSpan => "code_span",
            Defect::HttpStatus => "http_status",
            Defect::Machinery => "machinery",
            Defect::Disclaimer => "disclaimer",
            Defect::GenericOffer => "generic_offer",
            Defect::QuestionWithNothingToDo => "question_with_nothing_to_do",
            Defect::MultipleAsks => "multiple_asks",
            Defect::UrlLabel => "url_label",
            Defect::StackedTimes => "stacked_times",
            Defect::BrokenClosedOut => "broken_closed_out",
            Defect::Exclamation => "exclamation",
            Defect::DeadlineWithoutDate => "deadline_without_date",
            Defect::InternalId => "internal_id",
            Defect::ManyAtOnce => "many_at_once",
            Defect::LongAlert => "long_alert",
            Defect::BareUrl => "bare_url",
            Defect::UnreceiptedCompletionClaim => "unreceipted_completion_claim",
        }
    }
}

/// Why Dex is writing. It decides which defects count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Occasion {
    /// Dex wrote first: an alert, a scheduled note, a reminder, a follow-up.
    /// Nobody asked in those words, so every defect counts.
    DexStarted,
    /// A reply to what a person or an agent asked. They may have asked
    /// about a field, an error or an id by name, so naming one is not a
    /// defect; a disclaimer, an offer that names nothing and the like
    /// still are.
    Reply,
    /// Dex is asking the person to answer a pending question. The question
    /// may name a field, but it should ask for only one answer.
    Question,
}

/// The defects that are a defect in any message, asked for or not. A
/// machine timestamp is one: a person who asks when something happened is
/// answered with a date and time, never `2026-11-11T15:06:46.833055Z`.
const NEVER_RIGHT: [Defect; 10] = [
    Defect::RawTimestamp,
    Defect::Machinery,
    Defect::Disclaimer,
    Defect::GenericOffer,
    Defect::UrlLabel,
    Defect::StackedTimes,
    Defect::BrokenClosedOut,
    Defect::Exclamation,
    Defect::DeadlineWithoutDate,
    Defect::UnreceiptedCompletionClaim,
];

const QUESTION_STARTERS: &[&str] = &[
    "who", "whom", "whose", "what", "which", "when", "where", "why", "how", "am", "is", "are",
    "was", "were", "do", "does", "did", "can", "could", "will", "would", "shall", "should", "have",
    "has", "had", "may", "might", "must",
];

/// Counts question asks in `text`, saturated at two.
///
/// Counts unquoted question sentences and clear interrogative clauses joined
/// by `and` or `also`. This intentionally does not infer compound asks from
/// shared-clause wording such as "Which workspace and account should I use?".
pub fn question_ask_count(text: &str) -> usize {
    let (_, prose) = split_links(text);
    let prose = strip_question_emphasis(&strip_quoted_examples(&strip_non_slack_urls(&prose)));
    let questions: Vec<String> = sentences(&prose)
        .into_iter()
        .filter(|sentence| sentence.ends_with('?'))
        .take(2)
        .collect();
    if questions.len() > 1
        || questions
            .first()
            .is_some_and(|sentence| has_interrogative_joiner(sentence))
    {
        2
    } else {
        questions.len()
    }
}

fn strip_question_emphasis(text: &str) -> String {
    let original: Vec<char> = text.chars().collect();
    let mut prose = original.clone();
    let mut at = 0;
    while at < original.len() {
        let marker = original[at];
        if !matches!(marker, '*' | '_') {
            at += 1;
            continue;
        }
        let width = emphasis_marker_width(&original, at, marker);
        if width > 2
            || (at > 0 && is_word_character(original[at - 1]))
            || original
                .get(at + width)
                .is_none_or(|character| character.is_whitespace())
        {
            at += width;
            continue;
        }
        let Some(end) = question_emphasis_end(&original, at + width, marker, width) else {
            at += width;
            continue;
        };
        prose[at..at + width].fill(' ');
        prose[end..end + width].fill(' ');
        at += width;
    }
    prose.into_iter().collect()
}

fn question_emphasis_end(text: &[char], start: usize, marker: char, width: usize) -> Option<usize> {
    let mut at = start;
    while at < text.len() {
        if matches!(text[at], '\n' | '\r') {
            return None;
        }
        if text[at] == marker {
            let run = emphasis_marker_width(text, at, marker);
            if run == width
                && at > start
                && !text[at - 1].is_whitespace()
                && (at + run == text.len() || !is_word_character(text[at + run]))
            {
                return Some(at);
            }
            at += run;
        } else {
            at += 1;
        }
    }
    None
}

fn emphasis_marker_width(text: &[char], start: usize, marker: char) -> usize {
    let mut end = start + 1;
    while text.get(end) == Some(&marker) {
        end += 1;
    }
    end - start
}

fn strip_non_slack_urls(text: &str) -> String {
    let original: Vec<char> = text.chars().collect();
    let mut prose = original.clone();
    let mut at = 0;
    while at < original.len() {
        if original[at] == ']' && original.get(at + 1) == Some(&'(') {
            let start = at + 2;
            if url_scheme_len(&original, start).is_some()
                && let Some(end) = markdown_url_end(&original, start)
            {
                mask_url(&original, &mut prose, start, end);
                at = end + 1;
                continue;
            }
        }
        if let Some(length) = url_scheme_len(&original, at)
            && (at == 0 || !is_word_character(original[at - 1]))
        {
            let start = at;
            let mut end = at + length;
            let mut parentheses = 0;
            while end < original.len() {
                match original[end] {
                    '(' => parentheses += 1,
                    ')' if parentheses > 0 => parentheses -= 1,
                    ')' | '<' | '>' => break,
                    character if character.is_whitespace() => break,
                    _ => {}
                }
                end += 1;
            }
            while end > start && is_url_trailing_punctuation(original[end - 1]) {
                end -= 1;
            }
            mask_url(&original, &mut prose, start, end);
            at = end.max(start + length);
            continue;
        }
        at += 1;
    }
    prose.into_iter().collect()
}

fn url_scheme_len(text: &[char], at: usize) -> Option<usize> {
    ["https://", "http://", "www."].iter().find_map(|prefix| {
        let end = at.checked_add(prefix.len())?;
        let candidate = text.get(at..end)?;
        candidate
            .iter()
            .zip(prefix.chars())
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(&expected))
            .then_some(prefix.len())
    })
}

fn markdown_url_end(text: &[char], start: usize) -> Option<usize> {
    let mut parentheses = 0;
    for (at, character) in text.iter().enumerate().skip(start) {
        match character {
            '(' => parentheses += 1,
            ')' if parentheses == 0 => return Some(at),
            ')' => parentheses -= 1,
            '\n' | '\r' => return None,
            _ => {}
        }
    }
    None
}

fn mask_url(original: &[char], prose: &mut [char], start: usize, end: usize) {
    for (index, character) in original.iter().enumerate().take(end).skip(start) {
        if !matches!(character, '\n' | '\r') {
            prose[index] = ' ';
        }
    }
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_url_trailing_punctuation(character: char) -> bool {
    matches!(character, '.' | ',' | ';' | ':' | '!' | '?')
}

fn strip_quoted_examples(text: &str) -> String {
    let original: Vec<char> = text.chars().collect();
    let mut prose = original.clone();
    let mut at = 0;
    while at < original.len() {
        let closing = match original[at] {
            '"' => Some('"'),
            '“' => Some('”'),
            '‘' => Some('’'),
            '\'' if !is_apostrophe(&original, at) => Some('\''),
            _ => None,
        };
        let Some(closing) = closing else {
            at += 1;
            continue;
        };
        let Some(end) = quoted_example_end(&original, at, closing) else {
            at += 1;
            continue;
        };
        for (index, character) in original.iter().enumerate().take(end + 1).skip(at) {
            if !matches!(character, '\n' | '\r') {
                prose[index] = ' ';
            }
        }
        at = end + 1;
    }
    prose.into_iter().collect()
}

fn quoted_example_end(text: &[char], start: usize, closing: char) -> Option<usize> {
    let mut escaped = false;
    for (at, character) in text.iter().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
            continue;
        }
        if *character == '\\' {
            escaped = true;
            continue;
        }
        if *character == closing && !is_apostrophe(text, at) {
            return Some(at);
        }
    }
    None
}

fn is_apostrophe(text: &[char], at: usize) -> bool {
    matches!(text.get(at), Some('\'' | '’'))
        && at > 0
        && at + 1 < text.len()
        && text[at - 1].is_alphanumeric()
        && text[at + 1].is_alphanumeric()
}

fn has_interrogative_joiner(sentence: &str) -> bool {
    let lower = sentence.to_ascii_lowercase();
    ["and", "also"].iter().any(|joiner| {
        lower.match_indices(joiner).any(|(start, _)| {
            let end = start + joiner.len();
            let bytes = lower.as_bytes();
            let word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
            (start == 0 || !word_byte(bytes[start - 1]))
                && (end == bytes.len() || !word_byte(bytes[end]))
                && starts_interrogative_clause(&sentence[..start])
                && starts_interrogative_clause(&sentence[end..])
        })
    })
}

fn starts_interrogative_clause(clause: &str) -> bool {
    let clause = clause.trim_start_matches(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                ',' | ';' | ':' | '(' | '[' | '{' | '"' | '“' | '‘'
            )
    });
    let lower = clause.to_ascii_lowercase();
    let mut words = lower
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_ascii_alphanumeric()));
    let mut first = words.next().unwrap_or("");
    if first == "also" {
        first = words.next().unwrap_or("");
    }
    QUESTION_STARTERS.contains(&first)
}

/// The voice standard for one message: every defect of [`check`] when Dex
/// started it, and only those no question excuses when it is a reply. A
/// pending question also must not ask for more than one answer, using a
/// deterministic sentence and interrogative-clause heuristic rather than
/// semantic language understanding.
pub fn standard(occasion: Occasion, text: &str) -> Vec<Defect> {
    standard_for(occasion, text, check(text))
}

/// The voice standard for a turn, including whether each first-person
/// completion claim has a matching successful receipt. Matching is a
/// conservative heuristic over operation names and selected JSON fields, not
/// typed proof; unrecognized operations or targets fail closed.
pub fn standard_for_turn(
    occasion: Occasion,
    text: &str,
    outputs: &[crate::TurnOutput],
) -> Vec<Defect> {
    standard_for(occasion, text, check_turn(text, outputs))
}

fn standard_for(occasion: Occasion, text: &str, mut found: Vec<Defect>) -> Vec<Defect> {
    if occasion == Occasion::Question && question_ask_count(text) > 1 {
        found.push(Defect::MultipleAsks);
        found.sort();
        found.dedup();
    }
    match occasion {
        Occasion::DexStarted => found,
        Occasion::Reply => found
            .into_iter()
            .filter(|defect| NEVER_RIGHT.contains(defect))
            .collect(),
        Occasion::Question => found
            .into_iter()
            .filter(|defect| NEVER_RIGHT.contains(defect) || *defect == Defect::MultipleAsks)
            .collect(),
    }
}

/// Every ordinary defect in `text`, plus a defect when any individual
/// first-person completion claim lacks its own matching successful receipt.
fn check_turn(text: &str, outputs: &[crate::TurnOutput]) -> Vec<Defect> {
    let mut found = check(text);
    let claims = completion_claims(text);
    if !claims.is_empty() && !all_claims_have_receipts(&claims, outputs) {
        found.push(Defect::UnreceiptedCompletionClaim);
        found.sort();
        found.dedup();
    }
    found
}

/// `text` with the defects a rewrite can remove without changing what it
/// says about the person's records: a machine timestamp and a field name as
/// a person says them ([`crate::human_text::humanize`]), and no sentence
/// that is only a disclaimer or an offer that names nothing. Every other
/// sentence, link and line break is kept exactly. What is left is for
/// [`standard`] to report: a rewrite never invents wording.
pub fn rewrite(text: &str) -> String {
    let text = crate::human_text::humanize(text);
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let kept: Vec<&str> = line_sentences(line)
            .into_iter()
            .filter(|sentence| !is_filler(sentence))
            .collect();
        let rebuilt = kept.concat();
        // A line that held only filler goes with it; a line that was
        // already blank stays, so paragraphs keep their spacing.
        if rebuilt.trim().is_empty() && !line.trim().is_empty() {
            continue;
        }
        lines.push(rebuilt.trim_end().to_owned());
    }
    // Dropping a paragraph leaves no doubled or trailing blank lines.
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        if line.is_empty() && out.last().is_none_or(String::is_empty) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out.join("\n")
}

/// The disclaimers and nameless offers whose whole sentence says nothing
/// about the person's records, so dropping it loses no fact. "Bottom line:"
/// and "let me know if" can open a sentence that does state one, so those
/// are reported, never dropped.
const FILLER: [&str; 7] = [
    "isn't a verdict",
    "is not a verdict",
    "want me to fix it?",
    "want me to look?",
    "anything else i can",
    "happy to help",
    "is there anything specific",
];

/// Whether a sentence is only a disclaimer or an offer that names nothing.
/// A sentence that carries a link is never one: the link is a fact.
pub(crate) fn is_filler(sentence: &str) -> bool {
    if sentence.contains('<') || sentence.contains("](") || sentence.contains("http") {
        return false;
    }
    let lower = sentence.to_lowercase().replace('’', "'");
    FILLER.iter().any(|phrase| lower.contains(phrase))
}

/// One line as its sentences, each with the whitespace that follows it, so
/// joining them gives the line back. A `.`, `?` or `!` followed by
/// whitespace ends a sentence, except inside a Slack link (`<url|label>`).
/// The end of a Slack link followed by whitespace ends one too: a link that
/// closes a line of facts ("<url|Reconnect GitHub> Want me to fix it?") is
/// not part of the offer after it.
pub(crate) fn line_sentences(line: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut start = 0;
    let mut in_link = false;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '<' => in_link = true,
            '.' | '?' | '!' | '>' if !in_link || c == '>' => {
                in_link = false;
                if chars.peek().is_some_and(|(_, next)| next.is_whitespace()) {
                    let mut end = at + c.len_utf8();
                    while let Some((space_at, space)) = chars.peek().copied() {
                        if !space.is_whitespace() {
                            break;
                        }
                        end = space_at + space.len_utf8();
                        chars.next();
                    }
                    found.push(&line[start..end]);
                    start = end;
                }
            }
            _ => {}
        }
    }
    if start < line.len() {
        found.push(&line[start..]);
    }
    found
}

/// Phrases about Dex's reads, never the person's records.
const MACHINERY: [&str; 5] = [
    "read data",
    "your cutoff",
    "the data shows",
    "in the data",
    "tool call",
];

/// Boilerplate about what a message is not.
const DISCLAIMERS: [&str; 3] = ["isn't a verdict", "is not a verdict", "bottom line:"];

/// Offers that name nothing Dex would do.
const GENERIC_OFFERS: [&str; 6] = [
    "want me to fix it?",
    "want me to look?",
    "let me know if",
    "anything else i can",
    "happy to help",
    "is there anything specific",
];

/// A note's own words for "nothing needs you".
const NOTHING_NEEDED: [&str; 3] = [
    "nothing new needs you",
    "nothing needs you",
    "nothing needs your attention",
];

/// The provider prefixes of catalog test ids ("github-repo-internal-exposed",
/// "aws-check-s3-bucket-no-mfa-delete").
const TEST_ID_PREFIXES: [&str; 8] = [
    "aws-", "azure-", "gcp-", "github-", "gws-", "google-", "okta-", "slack-",
];

/// How a person asks for the one thing that matters most.
const PRIORITY_ASKS: [&str; 9] = [
    "most important",
    "most urgent",
    "top priority",
    "biggest risk",
    "where should i start",
    "where do i start",
    "what should i fix first",
    "what should i do first",
    "one thing",
];

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Every defect in `text`, each once, in [`Defect`] order.
pub fn check(text: &str) -> Vec<Defect> {
    let mut found = Vec::new();
    let (links, prose) = split_links(text);
    let lower = prose.to_lowercase().replace('’', "'");
    // A value in backticks is still that value to the person.
    for term in machine_terms(&text.replace('`', "")) {
        found.push(match term {
            MachineTerm::FieldName(_) => Defect::FieldName,
            MachineTerm::Timestamp(_) => Defect::RawTimestamp,
        });
    }
    if prose.contains('`') {
        found.push(Defect::CodeSpan);
    }
    if has_http_status(&prose) {
        found.push(Defect::HttpStatus);
    }
    if MACHINERY.iter().any(|phrase| lower.contains(phrase)) {
        found.push(Defect::Machinery);
    }
    if DISCLAIMERS.iter().any(|phrase| lower.contains(phrase)) {
        found.push(Defect::Disclaimer);
    }
    if GENERIC_OFFERS.iter().any(|phrase| lower.contains(phrase)) {
        found.push(Defect::GenericOffer);
    }
    if lower.contains('?') && NOTHING_NEEDED.iter().any(|phrase| lower.contains(phrase)) {
        found.push(Defect::QuestionWithNothingToDo);
    }
    if links.iter().any(|(_, label)| {
        let label = label.trim();
        label.starts_with("http://") || label.starts_with("https://") || label.starts_with("www.")
    }) {
        found.push(Defect::UrlLabel);
    }
    if links.iter().any(|(url, label)| {
        label.trim().is_empty() && (url.starts_with("http") || url.starts_with("www."))
    }) || ["http://", "https://", "www."]
        .iter()
        .any(|scheme| unlabelled_prose(&prose, &links).contains(scheme))
    {
        found.push(Defect::BareUrl);
    }
    if has_stacked_times(&prose) {
        found.push(Defect::StackedTimes);
    }
    if has_broken_closed_out(&lower) {
        found.push(Defect::BrokenClosedOut);
    }
    if prose.contains('!') {
        found.push(Defect::Exclamation);
    }
    if lower.contains("deadline: due") {
        found.push(Defect::DeadlineWithoutDate);
    }
    // A test id inside backticks is still that id to the person.
    if has_internal_id(&prose.replace('`', " ")) {
        found.push(Defect::InternalId);
    }
    found.sort();
    found.dedup();
    found
}

/// Every defect in `text` as a reply to `question`: [`check`], and
/// [`Defect::ManyAtOnce`] when the person asked what matters most and the
/// reply lists more than one item. "4 more on the Tests page" is how the rest
/// are named, not a second item.
/// Every defect in `text` as an alert: [`check`], and [`Defect::LongAlert`]
/// when [`alert_sentences`] is more than three.
pub fn check_alert(text: &str) -> Vec<Defect> {
    let mut found = check(text);
    if alert_sentences(text) > 3 {
        found.push(Defect::LongAlert);
        found.sort();
        found.dedup();
    }
    found
}

/// How a sentence that offers the one next step begins ("I'd turn on
/// enforcement for those 3.", "I can require 2-step verification for it.").
const OFFER_STARTS: [&str; 4] = ["i'd ", "i would ", "i can ", "i could "];

/// A question that asks to take the offered step on its own ("Want me to?",
/// "Shall I turn it on?").
const OFFER_QUESTIONS: [&str; 6] = [
    "want me to",
    "should i ",
    "shall i ",
    "can i ",
    "would you like me to",
    "do you want me to",
];

/// An alert's sentences, not counting one closing offer line: an offer
/// ("I'd ...", "I can ...") followed by a question ("Want me to?"), or one
/// question that is itself the offer ("I can require it, want me to?",
/// "Shall I turn it on?"). An alert is three sentences plus that optional
/// offer.
pub fn alert_sentences(text: &str) -> usize {
    let all = sentences(text);
    let lower = |sentence: &str| sentence.to_lowercase().replace('’', "'");
    let offer = |sentence: &str| {
        let sentence = lower(sentence);
        OFFER_STARTS.iter().any(|start| sentence.starts_with(start))
    };
    let question = |sentence: &str| {
        let sentence = lower(sentence);
        sentence.ends_with('?')
            && (OFFER_STARTS.iter().any(|start| sentence.starts_with(start))
                || OFFER_QUESTIONS.iter().any(|ask| sentence.starts_with(ask)))
    };
    let trailing = match all.as_slice() {
        [.., before, last] if last.ends_with('?') && offer(before) && !offer(last) => 2,
        [.., last] if question(last) => 1,
        _ => 0,
    };
    all.len() - trailing
}

/// The sentences a person reads in `text`: a `.`, `?` or `!` followed by a
/// space or the end of a line ends one, and so does a line with no end
/// mark. Links count as their labels, so the dots in a URL, a time or a
/// version end nothing.
pub fn sentences(text: &str) -> Vec<String> {
    let (_, prose) = split_links(text);
    let mut found = Vec::new();
    for line in prose.lines() {
        let chars: Vec<char> = line.trim().chars().collect();
        let mut current = String::new();
        for (at, c) in chars.iter().enumerate() {
            current.push(*c);
            let ends = matches!(c, '.' | '?' | '!')
                && chars.get(at + 1).is_none_or(|next| next.is_whitespace());
            if ends && current.chars().any(char::is_alphanumeric) {
                found.push(current.trim().to_owned());
                current.clear();
            }
        }
        if current.chars().any(char::is_alphanumeric) {
            found.push(current.trim().to_owned());
        }
    }
    found
}

pub fn check_answer(question: &str, text: &str) -> Vec<Defect> {
    let mut found = check(text);
    let asked = question.to_lowercase().replace('’', "'");
    if PRIORITY_ASKS.iter().any(|ask| asked.contains(ask)) && list_items(text) > 1 {
        found.push(Defect::ManyAtOnce);
        found.sort();
        found.dedup();
    }
    found
}

/// Lines that start a list item: "-", "•" or "*" and a space, or "1." /
/// "2)" and a space, after any bold or italic markers ("**1. Foo**").
fn list_items(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let line = line.trim_start();
            if ["- ", "• ", "* "]
                .iter()
                .any(|bullet| line.starts_with(bullet))
            {
                return true;
            }
            let line = line.trim_start_matches(['*', '_']);
            let digits = line.bytes().take_while(u8::is_ascii_digit).count();
            (1..=2).contains(&digits) && {
                let rest = &line[digits..];
                rest.starts_with(". ") || rest.starts_with(") ")
            }
        })
        .count()
}

/// A catalog test id ("github-repo-internal-exposed": a provider prefix and
/// at least three lowercase hyphenated parts) or a numeric id after the word
/// "ID" ("repository ID 1043229871", "ID: 52341").
fn has_internal_id(prose: &str) -> bool {
    let words: Vec<&str> = prose.split_whitespace().collect();
    words.iter().enumerate().any(|(at, word)| {
        let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        let test_id = TEST_ID_PREFIXES
            .iter()
            .any(|prefix| core.starts_with(prefix))
            && core.split('-').count() >= 3
            && core.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            });
        let id_word = matches!(
            word.trim_end_matches([':', '#'])
                .to_ascii_lowercase()
                .as_str(),
            "id" | "ids"
        );
        let numeric_id = id_word
            && words.get(at + 1).is_some_and(|next| {
                let next = next
                    .trim_start_matches('#')
                    .trim_end_matches([',', '.', ')', ';']);
                next.len() >= 4 && next.bytes().all(|b| b.is_ascii_digit())
            });
        test_id || numeric_id
    })
}

/// Slack links (`<url|label>`, `<url>`) and the text with each replaced by
/// its label, so URLs never read as prose.
pub(crate) fn split_links(text: &str) -> (Vec<(String, String)>, String) {
    let mut links = Vec::new();
    let mut prose = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        prose.push_str(&rest[..start]);
        let inner = &rest[start + 1..start + len];
        let (url, label) = inner.split_once('|').unwrap_or((inner, ""));
        prose.push_str(label);
        links.push((url.to_owned(), label.to_owned()));
        rest = &rest[start + len + 1..];
    }
    prose.push_str(rest);
    (links, prose)
}

/// `prose` without the labels of links whose label is a URL: those are
/// [`Defect::UrlLabel`], not a URL left in the prose.
fn unlabelled_prose(prose: &str, links: &[(String, String)]) -> String {
    links
        .iter()
        .map(|(_, label)| label.trim())
        .filter(|label| {
            label.starts_with("http://")
                || label.starts_with("https://")
                || label.starts_with("www.")
        })
        .fold(prose.to_owned(), |prose, label| {
            prose.replacen(label, "", 1)
        })
}

/// "HTTP" followed by a three-digit status.
fn has_http_status(prose: &str) -> bool {
    prose.match_indices("HTTP ").any(|(at, _)| {
        let tail = &prose.as_bytes()[at + 5..];
        tail.len() >= 3
            && tail[..3].iter().all(u8::is_ascii_digit)
            && tail.get(3).is_none_or(|b| !b.is_ascii_digit())
    })
}

/// "UTC (Nov 12, ..." : a time immediately followed by another in brackets.
fn has_stacked_times(prose: &str) -> bool {
    prose.match_indices("UTC (").any(|(at, _)| {
        let tail = &prose[at + 5..];
        MONTHS.iter().any(|month| tail.starts_with(month))
    })
}

/// "closed out" followed, in the same sentence, by words that state a
/// failure still going on.
fn has_broken_closed_out(lower: &str) -> bool {
    lower.match_indices("closed out ").any(|(at, _)| {
        let sentence = lower[at..].split(['.', '\n']).next().unwrap_or("");
        [" haven't ", " hasn't ", " have been ", " has been "]
            .iter()
            .any(|failure| sentence.contains(failure))
    })
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CompletionAction {
    Filed,
    Fixed,
    Sent,
}

impl CompletionAction {
    fn matches_tool(self, tool: &str) -> bool {
        let words = word_spans(tool);
        let Some((operation, _, _)) = words.last() else {
            return false;
        };
        let Some(resource_type) = acted_on_resource_type(tool) else {
            return false;
        };
        match self {
            Self::Filed => matches!(operation.as_str(), "create" | "file" | "submit"),
            Self::Fixed => matches!(
                operation.as_str(),
                "fix" | "repair" | "resolve" | "remediate"
            ),
            Self::Sent => {
                matches!(operation.as_str(), "send" | "reply" | "deliver")
                    || operation == "post" && matches!(resource_type, "email" | "message")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CompletionTarget {
    resource_type: Option<String>,
    subject: BTreeSet<String>,
    ids: BTreeSet<String>,
    recipient: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CompletionClaim {
    action: CompletionAction,
    target: CompletionTarget,
}

fn completion_claims(text: &str) -> Vec<CompletionClaim> {
    let mut claims = Vec::new();
    for sentence in text.lines().flat_map(line_sentences) {
        let sentence = sentence.trim();
        let words = word_spans(sentence);
        let mut first_person = false;
        let mut actions = Vec::new();
        for index in 0..words.len() {
            if words[index].0 == "i" {
                first_person = true;
                continue;
            }
            let action = match words[index].0.as_str() {
                "filed" => Some(CompletionAction::Filed),
                "fixed" => Some(CompletionAction::Fixed),
                "sent" => Some(CompletionAction::Sent),
                _ => None,
            };
            let Some(action) = action else {
                continue;
            };
            let mut action_prefix = index;
            while action_prefix > 0 && is_completion_prefix(&words[action_prefix - 1].0) {
                action_prefix -= 1;
            }
            let previous = action_prefix.checked_sub(1).map(|at| words[at].0.as_str());
            let explicit = previous == Some("i")
                || (previous == Some("ve")
                    && action_prefix >= 2
                    && words[action_prefix - 2].0 == "i");
            let coordinated = first_person
                && (matches!(previous, Some("and" | "but" | "then" | "also"))
                    || action_prefix > 0
                        && sentence[words[action_prefix - 1].2..words[action_prefix].1]
                            .contains(','));
            let claimed = (explicit || coordinated) && !negated_completion(sentence, &words, index);
            actions.push((index, action_prefix, action, claimed));
            if explicit || coordinated {
                first_person = true;
            }
        }
        for (at, (word_index, _, action, claimed)) in actions.iter().enumerate() {
            if !*claimed {
                continue;
            }
            let start = words[*word_index].2;
            let end = actions
                .get(at + 1)
                .map(|(_, next_prefix, _, _)| words[*next_prefix].1)
                .unwrap_or(sentence.len());
            claims.push(CompletionClaim {
                action: *action,
                target: completion_target(&sentence[start..end]),
            });
        }
    }
    claims
}

fn is_completion_prefix(word: &str) -> bool {
    matches!(
        word,
        "already"
            | "also"
            | "actually"
            | "been"
            | "finally"
            | "had"
            | "has"
            | "have"
            | "indeed"
            | "just"
            | "now"
            | "personally"
            | "recently"
            | "successfully"
            | "ve"
    )
}

fn negated_completion(
    sentence: &str,
    words: &[(String, usize, usize)],
    action_index: usize,
) -> bool {
    const NEGATIONS: &[&str] = &[
        "not", "never", "no", "none", "nothing", "nobody", "nowhere", "don", "didn", "haven",
        "hasn", "isn", "wasn", "won",
    ];
    let action_start = words[action_index].1;
    let prefix = &sentence[..action_start];
    let boundary = prefix.rfind([',', ';', ':']).map_or(0, |at| at + 1);
    let preceding = word_spans(&prefix[boundary..]);
    let scope_start = preceding
        .iter()
        .rposition(|(word, _, _)| {
            matches!(
                word.as_str(),
                "and" | "also" | "but" | "however" | "instead" | "then"
            )
        })
        .map_or(0, |at| at + 1);
    preceding[scope_start..]
        .iter()
        .any(|(word, _, _)| NEGATIONS.contains(&word.as_str()))
        || words.get(action_index + 1).is_some_and(|(word, _, _)| {
            matches!(
                word.as_str(),
                "no" | "none" | "nothing" | "nobody" | "never" | "not"
            )
        })
}

fn word_spans(text: &str) -> Vec<(String, usize, usize)> {
    let mut words = Vec::new();
    let mut start = None;
    for (at, character) in text.char_indices() {
        if character.is_alphanumeric() {
            start.get_or_insert(at);
        } else if let Some(start) = start.take() {
            words.push((text[start..at].to_lowercase(), start, at));
        }
    }
    if let Some(start) = start {
        words.push((text[start..].to_lowercase(), start, text.len()));
    }
    words
}

fn is_target_filler(word: &str) -> bool {
    matches!(
        word,
        "a" | "about"
            | "already"
            | "also"
            | "an"
            | "and"
            | "as"
            | "at"
            | "be"
            | "been"
            | "being"
            | "but"
            | "by"
            | "did"
            | "do"
            | "does"
            | "done"
            | "for"
            | "from"
            | "had"
            | "has"
            | "have"
            | "i"
            | "in"
            | "into"
            | "is"
            | "it"
            | "its"
            | "just"
            | "me"
            | "my"
            | "now"
            | "of"
            | "on"
            | "one"
            | "our"
            | "out"
            | "please"
            | "successfully"
            | "the"
            | "them"
            | "then"
            | "these"
            | "this"
            | "those"
            | "to"
            | "up"
            | "us"
            | "ve"
            | "was"
            | "we"
            | "were"
            | "with"
            | "you"
            | "your"
    )
}

fn target_terms(text: &str) -> BTreeSet<String> {
    word_spans(text)
        .into_iter()
        .map(|(word, _, _)| word)
        .filter(|word| !is_target_filler(word))
        .collect()
}

fn completion_target(text: &str) -> CompletionTarget {
    let words = word_spans(text);
    let recipient_span = words
        .iter()
        .find(|(word, _, _)| word == "to")
        .map(|(_, start, end)| (*start, *end));
    let (subject_text, recipient_text) =
        recipient_span.map_or((text, ""), |(start, end)| (&text[..start], &text[end..]));
    let resource_word = resource_type_word(subject_text);
    let resource_type = resource_word
        .as_deref()
        .and_then(canonical_resource_type)
        .map(str::to_owned);
    let mut subject = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for word in target_terms(subject_text) {
        if resource_word.as_ref() == Some(&word) {
            continue;
        }
        if word.chars().any(|character| character.is_ascii_digit()) {
            ids.insert(word);
        } else {
            subject.insert(word);
        }
    }
    CompletionTarget {
        resource_type,
        subject,
        ids,
        recipient: target_terms(recipient_text),
    }
}

fn resource_type_word(text: &str) -> Option<String> {
    let candidate = word_spans(text)
        .into_iter()
        .map(|(word, _, _)| word)
        .filter(|word| !is_target_filler(word))
        .rev()
        .find(|word| !word.chars().any(|character| character.is_ascii_digit()))?;
    canonical_resource_type(&candidate).map(|_| candidate)
}

fn canonical_resource_type(word: &str) -> Option<&'static str> {
    match word {
        "ticket" | "tickets" | "issue" | "issues" => Some("issue"),
        "document" | "documents" => Some("document"),
        "email" | "emails" | "mail" | "gmail" | "outlook" => Some("email"),
        "message" | "messages" => Some("message"),
        "incident" | "incidents" => Some("incident"),
        "report" | "reports" => Some("report"),
        "request" | "requests" => Some("request"),
        "task" | "tasks" => Some("task"),
        "file" | "files" => Some("file"),
        "comment" | "comments" => Some("comment"),
        "record" | "records" => Some("record"),
        "feedback" => Some("feedback"),
        _ => None,
    }
}

fn acted_on_resource_type(tool: &str) -> Option<&'static str> {
    word_spans(tool)
        .into_iter()
        .rev()
        .find_map(|(word, _, _)| canonical_resource_type(&word))
}

fn acted_on_resource_object<'a>(
    tool: &str,
    args: &'a serde_json::Value,
) -> Option<&'a serde_json::Value> {
    let (word, _, _) = word_spans(tool)
        .into_iter()
        .rev()
        .find(|(word, _, _)| canonical_resource_type(word).is_some())?;
    let kind = canonical_resource_type(&word)?;
    args.get(word.as_str())
        .or_else(|| args.get(kind))
        .filter(|value| value.is_object())
}

fn collect_direct_named_values(value: &serde_json::Value, keys: &[&str], values: &mut Vec<String>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, value) in object {
        if keys.contains(&key.as_str()) {
            if let Some(value) = value.as_str() {
                values.push(value.to_owned());
            } else if value.is_number() {
                values.push(value.to_string());
            }
        }
    }
}

fn collect_action_values(
    tool: &str,
    args: &serde_json::Value,
    keys: &[&str],
    values: &mut Vec<String>,
) {
    collect_direct_named_values(args, keys, values);
    if let Some(resource) = acted_on_resource_object(tool, args) {
        collect_direct_named_values(resource, keys, values);
    }
}

fn receipt_matches_claim(claim: &CompletionClaim, output: &crate::TurnOutput) -> bool {
    if output.receipt.is_none()
        || (claim.target.subject.is_empty()
            && claim.target.ids.is_empty()
            && claim.target.recipient.is_empty())
    {
        return false;
    }
    let Some(tool) = &output.tool else {
        return false;
    };
    let Some(args) = output.args.as_ref() else {
        return false;
    };
    if !claim.action.matches_tool(tool.as_str()) {
        return false;
    }
    let Some(resource_type) = acted_on_resource_type(tool.as_str()) else {
        return false;
    };
    if claim
        .target
        .resource_type
        .as_deref()
        .is_some_and(|claimed| claimed != resource_type)
    {
        return false;
    }

    const SUBJECT_FIELDS: &[&str] = &[
        "title",
        "summary",
        "name",
        "subject",
        "incident_key",
        "issue_key",
        "ticket_key",
    ];
    const ID_FIELDS: &[&str] = &[
        "resource_id",
        "incident_id",
        "issue_id",
        "ticket_id",
        "document_id",
        "message_id",
        "incident_key",
        "issue_key",
        "ticket_key",
    ];
    const RECIPIENT_FIELDS: &[&str] = &[
        "to",
        "recipient",
        "recipient_email",
        "to_email",
        "email_address",
        "channel_id",
        "team_id",
        "conversation_id",
        "thread_id",
    ];

    let mut subject_fields = SUBJECT_FIELDS.to_vec();
    if claim.action == CompletionAction::Sent {
        subject_fields.extend(["content", "text", "body"]);
    }
    let mut subjects = Vec::new();
    let mut ids = Vec::new();
    let mut recipients = Vec::new();
    collect_action_values(tool.as_str(), args, &subject_fields, &mut subjects);
    collect_action_values(tool.as_str(), args, ID_FIELDS, &mut ids);
    collect_action_values(tool.as_str(), args, RECIPIENT_FIELDS, &mut recipients);
    if let Some(resource) = acted_on_resource_object(tool.as_str(), args) {
        collect_direct_named_values(resource, &["id", "key"], &mut ids);
    }

    terms_match_one_value(&claim.target.subject, &subjects)
        && terms_match_one_value(&claim.target.ids, &ids)
        && terms_match_one_value(&claim.target.recipient, &recipients)
}

fn all_claims_have_receipts(claims: &[CompletionClaim], outputs: &[crate::TurnOutput]) -> bool {
    let claims: Vec<_> = claims.iter().collect::<BTreeSet<_>>().into_iter().collect();
    let mut receipts: BTreeMap<String, Option<&crate::TurnOutput>> = BTreeMap::new();
    for output in outputs {
        let Some(receipt) = &output.receipt else {
            continue;
        };
        let entry = receipts
            .entry(receipt.as_str().to_owned())
            .or_insert(Some(output));
        if let Some(previous) = *entry
            && (previous.tool != output.tool || previous.args != output.args)
        {
            *entry = None;
        }
    }
    let outputs: Vec<_> = receipts.into_values().flatten().collect();
    if claims.len() > outputs.len() {
        return false;
    }
    let mut assigned = vec![None; outputs.len()];
    for claim_index in 0..claims.len() {
        let mut seen = vec![false; outputs.len()];
        if !assign_receipt(claim_index, &claims, &outputs, &mut assigned, &mut seen) {
            return false;
        }
    }
    true
}

fn assign_receipt(
    claim_index: usize,
    claims: &[&CompletionClaim],
    outputs: &[&crate::TurnOutput],
    assigned: &mut [Option<usize>],
    seen: &mut [bool],
) -> bool {
    for output_index in 0..outputs.len() {
        if seen[output_index] || !receipt_matches_claim(claims[claim_index], outputs[output_index])
        {
            continue;
        }
        seen[output_index] = true;
        let available = match assigned[output_index] {
            Some(prior) => assign_receipt(prior, claims, outputs, assigned, seen),
            None => true,
        };
        if available {
            assigned[output_index] = Some(claim_index);
            return true;
        }
    }
    false
}

fn terms_match_one_value(terms: &BTreeSet<String>, values: &[String]) -> bool {
    terms.is_empty()
        || values
            .iter()
            .any(|value| terms.is_subset(&target_terms(value)))
}

#[cfg(test)]
mod completion_claim_tests {
    use super::*;
    use crate::{
        ApprovalMode, CallId, Cursor, Event, Outcome, PrincipalId, ProposedCall, ThreadId, TurnId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_CALL_ID: AtomicUsize = AtomicUsize::new(0);

    fn output(tool: &str, args: serde_json::Value, receipt: Option<&str>) -> crate::TurnOutput {
        crate::TurnOutput {
            call: CallId::new(format!(
                "call-{}",
                NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)
            )),
            tool: Some(crate::ToolName::new(tool)),
            args: Some(args),
            output: crate::Output::Text("operation completed".into()),
            receipt: receipt.map(crate::ReceiptId::new),
        }
    }

    fn pagerduty_create_args(title: &str, incident_key: &str, details: &str) -> serde_json::Value {
        serde_json::json!({
            "from": "operator@example.test",
            "incident": {
                "type": "incident",
                "title": title,
                "service": {"type": "service_reference", "id": "PSVC123"},
                "urgency": "high",
                "incident_key": incident_key,
                "body": {"type": "incident_body", "details": details},
            },
        })
    }

    fn pagerduty_resolve_args(incident_id: &str) -> serde_json::Value {
        serde_json::json!({
            "from": "operator@example.test",
            "incident": {"type": "incident_reference", "status": "resolved"},
            "incident_id": incident_id,
        })
    }

    fn teams_send_args(content: &str) -> serde_json::Value {
        serde_json::json!({
            "team_id": "TEAM123",
            "channel_id": "CHANNEL456",
            "content": content,
        })
    }

    #[test]
    fn completion_claim_needs_a_successful_output_with_a_receipt() {
        let claim = "I filed the billing outage incident.";
        let no_receipt = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            None,
        );
        assert!(check_turn(claim, &[]).contains(&Defect::UnreceiptedCompletionClaim));
        assert!(check_turn(claim, &[no_receipt]).contains(&Defect::UnreceiptedCompletionClaim));
        assert!(
            standard_for_turn(Occasion::Reply, claim, &[])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
    }

    #[test]
    fn failed_receipt_does_not_enter_the_turn_success_outputs() {
        let call = ProposedCall::new(
            CallId::new("call-file"),
            crate::ToolName::new("pagerduty.incident.create"),
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            PrincipalId::new("alice"),
        );
        let events = vec![
            (
                Cursor(1),
                Event::UserMessage {
                    interaction_mode: crate::InteractionMode::Unspecified,
                    turn: TurnId::new("turn-1"),
                    message_id: None,
                    principal: PrincipalId::new("alice"),
                    text: "File a billing outage incident".into(),
                    attachments: vec![],
                    client_tools: vec![],
                    authorized_tools: vec![],
                    model_binding: None,
                    context_window_tokens: None,
                    voice: None,
                    approval_mode: ApprovalMode::Interactive,
                    page: None,
                    asker: None,
                },
            ),
            (
                Cursor(2),
                Event::ModelStepCompleted {
                    interruption: None,
                    step: 1,
                    text: String::new(),
                    calls: vec![call.clone()],
                    reasoning: None,
                    served: None,
                    timing: None,
                },
            ),
            (
                Cursor(3),
                Event::ToolFinished {
                    call: call.id,
                    outcome: Outcome::Failed,
                    output: crate::Output::Text("not filed".into()),
                    receipt: Some(crate::ReceiptId::new("failed-receipt")),
                    summary: None,
                },
            ),
        ];
        let context = crate::rehydrate(
            ThreadId {
                org: "org".into(),
                workspace: "workspace".into(),
                thread: "thread".into(),
            },
            &events,
        );
        assert!(context.turn_outputs().is_empty());
        assert!(
            check_turn(
                "I filed the billing outage incident.",
                context.turn_outputs()
            )
            .contains(&Defect::UnreceiptedCompletionClaim)
        );
    }

    #[test]
    fn subject_resource_type_and_action_must_match_the_receipt() {
        let unrelated_subject = output(
            "pagerduty.incident.create",
            pagerduty_create_args("security review", "security-review", "Unrelated."),
            Some("receipt-security"),
        );
        assert!(
            check_turn(
                "I filed the billing outage incident.",
                std::slice::from_ref(&unrelated_subject),
            )
            .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let wrong_resource = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-incident"),
        );
        assert!(
            check_turn("I filed the billing outage ticket.", &[wrong_resource])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let unidentified_target = output(
            "pagerduty.incident.resolve",
            pagerduty_resolve_args("PINC123"),
            Some("receipt-unidentified-target"),
        );
        assert!(
            check_turn("I fixed the incident.", &[unidentified_target])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let document = output(
            "dex.make",
            serde_json::json!({
                "kind": "document",
                "title": "billing outage",
                "content": {"blocks": [{"id": "b1", "kind": "PARAGRAPH", "body": "billing outage"}]},
            }),
            Some("receipt-document"),
        );
        assert!(
            check_turn("I filed the billing outage ticket.", &[document])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let title_update = output(
            "atlassian.jira.issue.summary.update",
            serde_json::json!({"issue_id": "10042", "summary": "login flow"}),
            Some("receipt-update"),
        );
        assert!(
            check_turn("I fixed the login flow issue 10042.", &[title_update])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let mut untyped_text = output(
            "pagerduty.incident.create",
            pagerduty_create_args("security review", "security-review", "billing outage"),
            Some("receipt-unrelated-text"),
        );
        untyped_text.output = crate::Output::Text("billing outage incident filed".into());
        assert!(
            check_turn("I filed the billing outage incident.", &[untyped_text])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
    }

    #[test]
    fn nested_incident_fields_cannot_impersonate_the_acting_resource() {
        let mut args = pagerduty_resolve_args("PINC123");
        args["incident"]["service"] = serde_json::json!({
            "id": "PSVC123",
            "title": "billing outage",
            "type": "document",
        });
        let receipt = output(
            "pagerduty.incident.resolve",
            args,
            Some("receipt-pagerduty-incident"),
        );

        for claim in [
            "I fixed incident PSVC123.",
            "I fixed the billing outage incident.",
            "I fixed document PINC123.",
        ] {
            assert!(
                check_turn(claim, std::slice::from_ref(&receipt))
                    .contains(&Defect::UnreceiptedCompletionClaim),
                "{claim}"
            );
        }
    }

    #[test]
    fn realistic_matching_receipts_cover_filed_fixed_and_sent_claims() {
        let filed = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-billing"),
        );
        let fixed = output(
            "pagerduty.incident.resolve",
            pagerduty_resolve_args("PINC123"),
            Some("receipt-resolved"),
        );
        let sent = output(
            "microsoft-teams.message.send",
            teams_send_args("Queue-lag alert: the worker is behind."),
            Some("receipt-sent"),
        );
        assert!(
            !check_turn("I have filed the billing outage incident.", &[filed])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
        assert!(
            !check_turn("I fixed incident PINC123.", &[fixed])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
        assert!(
            !check_turn("I successfully sent the queue-lag alert.", &[sent])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
    }

    #[test]
    fn mixed_claims_need_corresponding_receipts_for_each_target() {
        let claim = "I filed the billing outage incident, fixed incident PINC123, and sent the queue-lag alert.";
        let filed = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-filed"),
        );
        let fixed = output(
            "pagerduty.incident.resolve",
            pagerduty_resolve_args("PINC123"),
            Some("receipt-fixed"),
        );
        let sent = output(
            "microsoft-teams.message.send",
            teams_send_args("Queue-lag alert: the worker is behind."),
            Some("receipt-sent"),
        );
        assert!(
            check_turn(claim, std::slice::from_ref(&filed))
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
        assert!(
            check_turn(claim, &[filed.clone(), fixed.clone()])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
        assert!(
            !check_turn(claim, &[filed, fixed, sent]).contains(&Defect::UnreceiptedCompletionClaim)
        );

        let repeated = "I filed the billing outage incident. I filed the billing outage incident.";
        let one_receipt = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-one"),
        );
        assert!(
            !check_turn(repeated, std::slice::from_ref(&one_receipt))
                .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let two_targets =
            "I filed the billing outage incident and filed the security review incident.";
        let billing = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-shared"),
        );
        let security = output(
            "pagerduty.incident.create",
            pagerduty_create_args("security review", "security-review", "Investigate it."),
            Some("receipt-shared"),
        );
        assert!(
            check_turn(two_targets, &[billing, security])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );

        let billing = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-one"),
        );
        let security = output(
            "pagerduty.incident.create",
            pagerduty_create_args("security review", "security-review", "Investigate it."),
            Some("receipt-two"),
        );
        assert!(
            !check_turn(two_targets, &[billing, security])
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
    }

    #[test]
    fn negation_in_a_prior_target_does_not_hide_a_coordinated_claim() {
        let claim = "I filed the no-MFA incident and sent the queue-lag alert.";
        let filed = output(
            "pagerduty.incident.create",
            pagerduty_create_args("no-MFA incident", "no-MFA", "Investigate it."),
            Some("receipt-no-mfa"),
        );
        let sent = output(
            "microsoft-teams.message.send",
            teams_send_args("Queue-lag alert: the worker is behind."),
            Some("receipt-queue-lag"),
        );

        assert!(
            check_turn(claim, std::slice::from_ref(&filed))
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
        assert!(!check_turn(claim, &[filed, sent]).contains(&Defect::UnreceiptedCompletionClaim));
    }

    #[test]
    fn later_action_prefix_is_not_part_of_the_previous_target() {
        let claim = "I filed the billing outage incident and recently sent the queue-lag alert.";
        let filed = output(
            "pagerduty.incident.create",
            pagerduty_create_args("billing outage", "billing-outage", "Investigate it."),
            Some("receipt-billing"),
        );
        let sent = output(
            "microsoft-teams.message.send",
            teams_send_args("Queue-lag alert: the worker is behind."),
            Some("receipt-queue-lag"),
        );

        assert!(
            check_turn(claim, std::slice::from_ref(&filed))
                .contains(&Defect::UnreceiptedCompletionClaim)
        );
        assert!(!check_turn(claim, &[filed, sent]).contains(&Defect::UnreceiptedCompletionClaim));
    }

    #[test]
    fn negated_claims_are_not_completion_claims() {
        for (text, action) in [
            ("I have not filed the billing outage incident.", "filed"),
            ("I never sent the queue-lag alert.", "sent"),
            ("I fixed no incident.", "fixed"),
        ] {
            assert!(
                !check_turn(text, &[]).contains(&Defect::UnreceiptedCompletionClaim),
                "{text}"
            );
            let words = word_spans(text);
            let action_index = words
                .iter()
                .position(|(word, _, _)| word == action)
                .expect("completion action");
            assert!(negated_completion(text, &words, action_index), "{text}");
        }
    }
}
