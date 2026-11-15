//! Mechanical checks of a message Dex starts itself against its voice
//! standard (the Voice section of Dex's shared prompt).
//!
//! The prompt carries the judgment: what is worth an interruption, what to
//! name, what to leave out. Whatever a person can spot without judgment is
//! checked here instead: a field name, a raw timestamp, a code span or an HTTP
//! status in prose; words about Dex's own machinery ("the read data", "your
//! cutoff"); a boilerplate disclaimer; a generic offer, or any question when
//! the note says nothing needs the person; a link labelled with its own URL;
//! two times stacked on one fact; "I closed out" followed by a failure; an
//! exclamation mark; a deadline with no date; a catalog test id or a record's
//! numeric id.
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
            Defect::UrlLabel => "url_label",
            Defect::StackedTimes => "stacked_times",
            Defect::BrokenClosedOut => "broken_closed_out",
            Defect::Exclamation => "exclamation",
            Defect::DeadlineWithoutDate => "deadline_without_date",
            Defect::InternalId => "internal_id",
            Defect::ManyAtOnce => "many_at_once",
            Defect::LongAlert => "long_alert",
            Defect::BareUrl => "bare_url",
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
}

/// The defects that are a defect in any message, asked for or not. A
/// machine timestamp is one: a person who asks when something happened is
/// answered with a date and time, never `2026-11-11T15:06:46.833055Z`.
const NEVER_RIGHT: [Defect; 9] = [
    Defect::RawTimestamp,
    Defect::Machinery,
    Defect::Disclaimer,
    Defect::GenericOffer,
    Defect::UrlLabel,
    Defect::StackedTimes,
    Defect::BrokenClosedOut,
    Defect::Exclamation,
    Defect::DeadlineWithoutDate,
];

/// The voice standard for one message: every defect of [`check`] when Dex
/// started it, and only those no question excuses when it is a reply.
pub fn standard(occasion: Occasion, text: &str) -> Vec<Defect> {
    let found = check(text);
    match occasion {
        Occasion::DexStarted => found,
        Occasion::Reply => found
            .into_iter()
            .filter(|defect| NEVER_RIGHT.contains(defect))
            .collect(),
    }
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
