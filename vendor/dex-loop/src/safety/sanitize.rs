//! Customer-safe text: every text delta passes through a `Sanitizer` before
//! it is appended, so what streams is what persists.

use std::sync::Arc;

/// Makes one `DeltaFilter` per model call.
pub trait Sanitizer: Send + Sync {
    type Filter: DeltaFilter;

    fn filter(&self) -> Self::Filter;
}

/// A streaming filter over one model call's text.
pub trait DeltaFilter: Send {
    /// Returns the text that is safe to emit now. Text that could still be the
    /// start of a forbidden term is held back.
    fn push(&mut self, text: &str) -> String;

    /// Returns whatever is still held back, filtered. Called once at the end.
    fn finish(&mut self) -> String;
}

/// Replaces forbidden terms (internal tool names, product and host names)
/// with customer-safe replacements, ASCII case-insensitively, holding back at
/// most one term's length of text. The host builds it from the registry
/// (tool name to label) and its fixed lexicon. Linear in text length times
/// lexicon size; a host with a large lexicon can supply an automaton instead.
#[derive(Clone, Debug, Default)]
pub struct Lexicon {
    /// Longest term first, so the longest match wins.
    terms: Arc<[(String, String)]>,
}

impl Lexicon {
    pub fn new<T, R>(terms: impl IntoIterator<Item = (T, R)>) -> Self
    where
        T: Into<String>,
        R: Into<String>,
    {
        let mut terms: Vec<(String, String)> = terms
            .into_iter()
            .map(|(term, replacement)| (term.into(), replacement.into()))
            .filter(|(term, _)| !term.is_empty())
            .collect();
        terms.sort_by_key(|(term, _)| std::cmp::Reverse(term.len()));
        Self {
            terms: terms.into(),
        }
    }
}

impl Sanitizer for Lexicon {
    type Filter = LexiconFilter;

    fn filter(&self) -> LexiconFilter {
        LexiconFilter {
            terms: Arc::clone(&self.terms),
            held: String::new(),
        }
    }
}

/// The per-call state of a `Lexicon`.
#[derive(Debug)]
pub struct LexiconFilter {
    terms: Arc<[(String, String)]>,
    held: String,
}

impl DeltaFilter for LexiconFilter {
    fn push(&mut self, text: &str) -> String {
        self.held.push_str(text);
        let (out, consumed) = scan(&self.terms, &self.held, false);
        self.held.drain(..consumed);
        out
    }

    fn finish(&mut self) -> String {
        let (out, _) = scan(&self.terms, &self.held, true);
        self.held.clear();
        out
    }
}

/// Emits `buf` with terms replaced. Unless `eof`, stops at the first position
/// where more text could still complete a term, and returns how many bytes it
/// consumed.
fn scan(terms: &[(String, String)], buf: &str, eof: bool) -> (String, usize) {
    let bytes = buf.as_bytes();
    let mut out = String::with_capacity(buf.len());
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let could_grow = terms.iter().any(|(term, _)| {
            term.len() > rest.len() && term.as_bytes()[..rest.len()].eq_ignore_ascii_case(rest)
        });
        if could_grow && !eof {
            return (out, at);
        }
        let matched = terms.iter().find(|(term, _)| {
            rest.len() >= term.len() && rest[..term.len()].eq_ignore_ascii_case(term.as_bytes())
        });
        if let Some((term, replacement)) = matched {
            out.push_str(replacement);
            // The matched bytes equal the term up to ASCII case, so they end
            // on a char boundary of `buf`.
            at += term.len();
            continue;
        }
        let Some(ch) = buf[at..].chars().next() else {
            break;
        };
        out.push(ch);
        at += ch.len_utf8();
    }
    (out, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lexicon: &Lexicon, chunks: &[&str]) -> String {
        let mut filter = lexicon.filter();
        let mut out: String = chunks.iter().map(|chunk| filter.push(chunk)).collect();
        out.push_str(&filter.finish());
        out
    }

    #[test]
    fn replaces_terms_split_across_chunks() {
        let lexicon = Lexicon::new([
            ("linear.search_issues", "Linear search"),
            ("Maestro", "Dex"),
        ]);
        assert_eq!(
            run(
                &lexicon,
                &["I called line", "ar.search_iss", "ues via maes", "tro."]
            ),
            "I called Linear search via Dex."
        );
    }

    #[test]
    fn prefers_the_longest_term() {
        let lexicon = Lexicon::new([("maestro", "Dex"), ("maestro-runtime", "the runtime")]);
        assert_eq!(
            run(&lexicon, &["a maestro", "-runtime b"]),
            "a the runtime b"
        );
        assert_eq!(run(&lexicon, &["a maestro", " b"]), "a Dex b");
    }

    #[test]
    fn holds_back_only_a_possible_prefix() {
        let lexicon = Lexicon::new([("sandboxwich", "the computer")]);
        let mut filter = lexicon.filter();
        assert_eq!(filter.push("hello sandbox"), "hello ");
        assert_eq!(filter.push("wiches!"), "the computeres!");
        assert_eq!(filter.push("héllo sa"), "héllo ");
        assert_eq!(filter.finish(), "sa");
    }

    #[test]
    fn empty_lexicon_passes_text_through() {
        let lexicon = Lexicon::default();
        let mut filter = lexicon.filter();
        assert_eq!(filter.push("héllo"), "héllo");
        assert_eq!(filter.finish(), "");
    }
}
