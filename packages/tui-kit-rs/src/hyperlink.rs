//! OSC-8 Hyperlink Support
//!
//! Terminal hyperlinks using the OSC 8 escape sequence.
//! Works in modern terminals like iTerm2, Kitty, WezTerm, etc.
//!
//! Ported from the TypeScript Maestro TUI.

use ratatui::style::Style;
use ratatui::text::Span;

// ─────────────────────────────────────────────────────────────────────────────
// OSC-8 HYPERLINK FORMAT
// ─────────────────────────────────────────────────────────────────────────────

/// OSC 8 start sequence: `ESC ] 8 ; ;`
const OSC8_START: &str = "\x1b]8;;";

/// OSC 8 terminator: `BEL` (0x07)
const OSC8_TERM: &str = "\x07";

/// OSC 8 end sequence (empty URL to close link)
const OSC8_END: &str = "\x1b]8;;\x07";

// ─────────────────────────────────────────────────────────────────────────────
// HYPERLINK FUNCTIONS
// ─────────────────────────────────────────────────────────────────────────────

/// Format a clickable hyperlink using OSC 8 escape sequences.
///
/// # Arguments
/// * `url` - The URL to link to
/// * `label` - Optional display text (defaults to URL)
///
/// `url` and `label` are sanitized (via [`crate::notifications::sanitize_osc_text`],
/// the same helper `osc_set_title` uses) before being interpolated into the
/// escape sequence. This is a "safe by construction" primitive: its output
/// is written straight to the terminal by direct `print!`/`write!` callers
/// (native scrollback, non-TUI output) as well as through ratatui `Span`s,
/// so it must not trust its inputs regardless of which sink ends up using it.
///
/// # Example
/// ```
/// use maestro_tui_kit::hyperlink::format_link;
///
/// // With custom label
/// let link = format_link("https://example.com", Some("Example"));
/// // Renders as clickable "Example" in supported terminals
///
/// // URL as label
/// let link = format_link("https://example.com", None);
/// ```
#[must_use]
pub fn format_link(url: &str, label: Option<&str>) -> String {
    let url = crate::notifications::sanitize_osc_text(url);
    let text = label.map_or_else(|| url.clone(), crate::notifications::sanitize_osc_text);
    format!("{OSC8_START}{url}{OSC8_TERM}{text}{OSC8_END}")
}

/// Format a hyperlink with fallback for non-TTY output.
///
/// Returns `label (url)` format when not in a terminal.
#[must_use]
pub fn format_link_with_fallback(url: &str, label: Option<&str>, is_tty: bool) -> String {
    let text = label.unwrap_or(url);
    if !is_tty {
        // No OSC framing is ever emitted on this branch -- it returns
        // plain `"{text} ({url})"` or `url` alone, never an escape
        // sequence -- so there is nothing here for control characters to
        // break out of. Sanitizing anyway would silently corrupt
        // legitimate whitespace (tabs, newlines) in a url/label when this
        // output is piped or redirected to a file, for zero safety
        // benefit; see `format_link`, `link_start`, and `wrap_in_link`
        // below for the paths that actually need it.
        if label.is_some() {
            return format!("{text} ({url})");
        }
        return url.to_string();
    }
    format_link(url, label)
}

/// Create the OSC 8 start sequence for a URL.
///
/// Use this for manual link construction when you need
/// to apply styling to the link text. Sanitized for the same reason as
/// [`format_link`].
#[must_use]
pub fn link_start(url: &str) -> String {
    let url = crate::notifications::sanitize_osc_text(url);
    format!("{OSC8_START}{url}{OSC8_TERM}")
}

/// The OSC 8 end sequence to close a hyperlink.
#[must_use]
pub fn link_end() -> &'static str {
    OSC8_END
}

/// Wrap text in hyperlink sequences.
///
/// Lower-level function for when you need more control. `url` is sanitized
/// via `link_start`; `text` (the visible label, printed literally between
/// the OSC 8 markers) is sanitized here too, for the same reason as
/// [`format_link`]'s label.
#[must_use]
pub fn wrap_in_link(url: &str, text: &str) -> String {
    let text = crate::notifications::sanitize_osc_text(text);
    format!("{}{}{}", link_start(url), text, OSC8_END)
}

// ─────────────────────────────────────────────────────────────────────────────
// RATATUI INTEGRATION
// ─────────────────────────────────────────────────────────────────────────────

/// Create a Span with hyperlink.
///
/// Note: ratatui's hyperlink support depends on the terminal.
/// This embeds the OSC 8 sequences in the content.
#[must_use]
pub fn link_span(url: &str, label: &str, style: Style) -> Span<'static> {
    Span::styled(format_link(url, Some(label)), style)
}

/// Create a Span with hyperlink using URL as label.
#[must_use]
pub fn url_span(url: &str, style: Style) -> Span<'static> {
    Span::styled(format_link(url, None), style)
}

// ─────────────────────────────────────────────────────────────────────────────
// LINK DETECTION
// ─────────────────────────────────────────────────────────────────────────────

/// Check if a string contains OSC 8 hyperlink sequences.
#[must_use]
pub fn contains_hyperlink(s: &str) -> bool {
    s.contains(OSC8_START)
}

/// Whether `s` is *exactly* one well-formed OSC 8 hyperlink wrapper already
/// produced by this module's own constructors: `OSC8_START <url>
/// OSC8_TERM <label> OSC8_END`, with the url and label segments containing
/// no control characters at all.
///
/// Used by native-scrollback writers (`terminal::history`,
/// `inline_scroll`) that otherwise strip every control character from span
/// content: those writers have no way to tell a trusted hyperlink embedded
/// by [`link_span`]/[`url_span`]/[`wrap_in_link`] (whose url/label are
/// already sanitized at construction, per those functions' doc comments)
/// from attacker-controlled text, so a blanket strip would also remove the
/// wrapper's own ESC/BEL bytes and leave a dead, unclickable link.
///
/// This checks the *exact, whole-string* structure rather than merely
/// `contains_hyperlink`, which matters for safety: requiring the url and
/// label to be control-character-free over their *entire* span (not just
/// "found `OSC8_START` somewhere") means any span with so much as one
/// extra control byte anywhere -- e.g. attacker-controlled text merely
/// containing the `OSC8_START` substring, with a real escape sequence
/// elsewhere in the same span -- fails this check and falls through to
/// full sanitization. A span that legitimately matches this exact grammar
/// contains no bytes capable of an escape/OSC/CSI injection beyond the
/// hyperlink wrapper itself, so treating it as already-safe cannot
/// reintroduce the injection this module's sanitization defends against;
/// at worst a spoofed url/label can only misrepresent the link's target,
/// which is inherent to hyperlinks in general and not specific to this
/// pass-through.
#[must_use]
pub fn is_exact_sanitized_hyperlink(s: &str) -> bool {
    let Some(after_start) = s.strip_prefix(OSC8_START) else {
        return false;
    };
    let Some(term_at) = after_start.find(OSC8_TERM) else {
        return false;
    };
    let (url, after_term) = after_start.split_at(term_at);
    let after_term = &after_term[OSC8_TERM.len()..];
    let Some(label) = after_term.strip_suffix(OSC8_END) else {
        return false;
    };
    !url.chars().any(char::is_control) && !label.chars().any(char::is_control)
}

/// Strip OSC 8 hyperlink sequences from a string.
///
/// Returns the visible text without link formatting.
#[must_use]
pub fn strip_hyperlinks(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Check for OSC 8 sequence
            if chars.peek() == Some(&']') {
                chars.next(); // consume ']'
                if chars.peek() == Some(&'8') {
                    chars.next(); // consume '8'
                    if chars.peek() == Some(&';') {
                        chars.next(); // consume ';'
                        if chars.peek() == Some(&';') {
                            chars.next(); // consume ';'
                            // Skip until BEL (0x07) or ST (ESC \)
                            while let Some(ch) = chars.next() {
                                if ch == '\x07' {
                                    break;
                                }
                                if ch == '\x1b' && chars.peek() == Some(&'\\') {
                                    chars.next();
                                    break;
                                }
                            }
                            continue;
                        }
                    }
                }
            }
            // Not a hyperlink sequence, keep the escape
            result.push(c);
        } else {
            result.push(c);
        }
    }

    result
}

/// Extract URLs from text containing OSC 8 hyperlinks.
#[must_use]
pub fn extract_urls(s: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut remaining = s;

    while let Some(start) = remaining.find(OSC8_START) {
        let after_start = &remaining[start + OSC8_START.len()..];
        if let Some(end) = after_start.find(OSC8_TERM) {
            let url = &after_start[..end];
            if !url.is_empty() {
                urls.push(url.to_string());
            }
            remaining = &after_start[end + OSC8_TERM.len()..];
        } else {
            break;
        }
    }

    urls
}

// ─────────────────────────────────────────────────────────────────────────────
// TESTS
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_link_with_label() {
        let link = format_link("https://example.com", Some("Example"));
        assert!(link.contains("https://example.com"));
        assert!(link.contains("Example"));
        assert!(link.starts_with(OSC8_START));
        assert!(link.ends_with(OSC8_END));
    }

    #[test]
    fn format_link_url_as_label() {
        let link = format_link("https://example.com", None);
        // URL appears twice: once in the link, once as text
        assert_eq!(link.matches("https://example.com").count(), 2);
    }

    #[test]
    fn format_link_strips_escape_from_url_and_label() {
        // A minimal OSC-0 (set title) sequence embedded in untrusted input.
        // `format_link`'s own OSC-8 wrapper legitimately contains 2 ESC and
        // 2 BEL bytes (`OSC8_START` + `OSC8_TERM` + `OSC8_END`); only the
        // *embedded* escape/BEL from the malicious url/label must be gone.
        let malicious_url = "https://example.com\x1b]0;pwned\x07";
        let malicious_label = "click\x1b]0;pwned\x07me";
        let link = format_link(malicious_url, Some(malicious_label));
        assert!(!link.contains("\x1b]0;pwned\x07"));
        assert_eq!(link.matches('\x1b').count(), 2);
        assert_eq!(link.matches('\x07').count(), 2);
        assert!(link.contains("https://example.com"));
        // Only the control bytes are stripped; the surrounding printable
        // characters from the (now-inert) escape sequence remain as
        // harmless visible text.
        assert!(link.contains("click]0;pwnedme"));
    }

    #[test]
    fn link_start_strips_control_chars() {
        // `link_start` legitimately emits one ESC as part of `OSC8_START`;
        // the embedded CSI sequence in the url must not survive alongside it.
        let start = link_start("https://example.com\x1b[31m");
        assert!(!start.contains("\x1b[31m"));
        assert_eq!(start.matches('\x1b').count(), 1);
        assert!(start.contains("https://example.com"));
    }

    #[test]
    fn wrap_in_link_strips_control_chars_from_text() {
        // `wrap_in_link` legitimately emits 2 ESC and 2 BEL bytes via
        // `link_start` + `OSC8_END`; the embedded OSC-9 sequence in `text`
        // must not survive alongside them.
        let wrapped = wrap_in_link("https://example.com", "safe\x1b]9;evil\x07text");
        assert!(!wrapped.contains("\x1b]9;evil\x07"));
        assert_eq!(wrapped.matches('\x1b').count(), 2);
        assert_eq!(wrapped.matches('\x07').count(), 2);
        assert!(wrapped.contains("safe]9;eviltext"));
    }

    #[test]
    fn format_link_fallback_non_tty() {
        let link = format_link_with_fallback("https://example.com", Some("Example"), false);
        assert_eq!(link, "Example (https://example.com)");
    }

    /// The non-TTY fallback never emits OSC framing -- no escape sequence
    /// for a control character to break out of -- so it must not sanitize
    /// at all: legitimate whitespace in the label/url must survive
    /// byte-exact when this output is piped or redirected.
    #[test]
    fn format_link_fallback_non_tty_preserves_whitespace() {
        let link =
            format_link_with_fallback("https://example.com", Some("multi\nline\tlabel"), false);
        assert_eq!(link, "multi\nline\tlabel (https://example.com)");

        let url_only = format_link_with_fallback("https://example.com/a\tb", None, false);
        assert_eq!(url_only, "https://example.com/a\tb");
    }

    #[test]
    fn format_link_fallback_tty() {
        let link = format_link_with_fallback("https://example.com", Some("Example"), true);
        assert!(link.contains(OSC8_START));
    }

    #[test]
    fn contains_hyperlink_detection() {
        let with_link = format_link("https://example.com", Some("test"));
        let without_link = "just plain text";

        assert!(contains_hyperlink(&with_link));
        assert!(!contains_hyperlink(without_link));
    }

    #[test]
    fn strip_hyperlinks_removes_links() {
        let link = format_link("https://example.com", Some("Example"));
        let stripped = strip_hyperlinks(&link);
        assert_eq!(stripped, "Example");
        assert!(!stripped.contains('\x1b'));
    }

    #[test]
    fn strip_hyperlinks_preserves_plain_text() {
        let text = "Hello, world!";
        assert_eq!(strip_hyperlinks(text), text);
    }

    #[test]
    fn extract_urls_finds_all() {
        let text = format!(
            "Check {} and {}",
            format_link("https://a.com", Some("A")),
            format_link("https://b.com", Some("B"))
        );
        let urls = extract_urls(&text);
        assert_eq!(urls, vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn wrap_in_link_works() {
        let wrapped = wrap_in_link("https://example.com", "Click me");
        assert!(wrapped.contains("https://example.com"));
        assert!(wrapped.contains("Click me"));
    }

    #[test]
    fn is_exact_sanitized_hyperlink_accepts_trusted_constructor_output() {
        assert!(is_exact_sanitized_hyperlink(&format_link(
            "file:///tmp/report.md",
            Some("report")
        )));
        assert!(is_exact_sanitized_hyperlink(&wrap_in_link(
            "file:///tmp/report.md",
            "report"
        )));
        assert!(is_exact_sanitized_hyperlink(&format_link(
            "file:///tmp/report.md",
            None
        )));
    }

    #[test]
    fn is_exact_sanitized_hyperlink_rejects_plain_text() {
        assert!(!is_exact_sanitized_hyperlink("just plain text"));
        assert!(!is_exact_sanitized_hyperlink(""));
    }

    #[test]
    fn is_exact_sanitized_hyperlink_rejects_extra_control_bytes_anywhere() {
        // A well-formed wrapper with one extra, un-sanitized escape tacked
        // on must not be treated as trusted: this is exactly the shape an
        // attacker-controlled span (not built through this module) could
        // try to spoof.
        let mostly_well_formed = format!("{}\x1b[31m", wrap_in_link("https://example.com", "x"));
        assert!(!is_exact_sanitized_hyperlink(&mostly_well_formed));

        let extra_before = format!("\x1b[31m{}", wrap_in_link("https://example.com", "x"));
        assert!(!is_exact_sanitized_hyperlink(&extra_before));

        // Control character inside what would be the label segment.
        let dirty_label =
            format!("{OSC8_START}https://example.com{OSC8_TERM}bad\x1b[31mlabel{OSC8_END}");
        assert!(!is_exact_sanitized_hyperlink(&dirty_label));
    }
}
