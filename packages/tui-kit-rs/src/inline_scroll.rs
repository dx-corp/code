//! Inline Scrolling with Scroll Regions
//!
//! This module provides scroll region manipulation for inline TUI mode.
//! Unlike alternate screen mode, inline mode allows the terminal's native
//! scrollback to capture output history.
//!
//! The key technique is using DECSTBM (Set Top and Bottom Margins) to create
//! scroll regions, then using Reverse Index (RI) to scroll content within
//! those regions without affecting the viewport.
//!
//! This is critical for SSH sessions where the terminal scrollback provides
//! the primary way to review previous output.
//!
//! Ported from OpenAI Codex CLI (MIT licensed).

use std::fmt;
use std::io::{self, Write};

use crossterm::Command;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor, SetColors,
    SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use ratatui::prelude::IntoCrossterm;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};

use crate::ansi_commands::{ResetScrollRegion, SetScrollRegion};

// ─────────────────────────────────────────────────────────────────────────────
// INLINE HISTORY INSERTION
// ─────────────────────────────────────────────────────────────────────────────

/// Insert lines into the terminal scrollback above the current viewport.
///
/// This uses scroll regions to insert content without disturbing the viewport:
///
/// ```text
/// ┌─Screen───────────────────────┐
/// │┌╌Scroll region╌╌╌╌╌╌╌╌╌╌╌╌╌╌┐│
/// │┆                            ┆│
/// │┆   ← History inserted here  ┆│
/// │┆                            ┆│
/// │█╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┘│
/// │╭─Viewport───────────────────╮│
/// ││   ← Viewport stays fixed   ││
/// │╰────────────────────────────╯│
/// └──────────────────────────────┘
/// ```
///
/// # Arguments
///
/// * `writer` - The output writer (typically stdout)
/// * `viewport_top` - The top row of the viewport (0-based)
/// * `viewport_bottom` - The bottom row of the viewport (0-based, exclusive)
/// * `screen_height` - Total terminal height
/// * `lines` - Lines to insert above the viewport
/// * `cursor_restore_pos` - Position to restore cursor after insertion
///
/// # Returns
///
/// The new viewport top position (may shift down if there was room below).
pub fn insert_history_lines<W: Write>(
    writer: &mut W,
    viewport_top: u16,
    viewport_bottom: u16,
    screen_height: u16,
    lines: &[Line<'_>],
    width: u16,
    cursor_restore_pos: (u16, u16),
) -> io::Result<u16> {
    if lines.is_empty() || width == 0 {
        return Ok(viewport_top);
    }

    // Pre-wrap lines for consistent display
    let wrapped = wrap_lines_for_terminal(lines, width as usize);
    let wrapped_count = wrapped.len() as u16;

    let mut new_viewport_top = viewport_top;

    // If the viewport is not at the bottom of the screen, we can scroll it down
    // to make room for history above
    if viewport_bottom < screen_height {
        let available_below = screen_height - viewport_bottom;
        let scroll_amount = wrapped_count.min(available_below);

        if scroll_amount > 0 {
            // Set scroll region from viewport top to screen bottom
            let top_1based = viewport_top + 1;
            queue!(writer, SetScrollRegion(top_1based..screen_height))?;
            queue!(writer, MoveTo(0, viewport_top))?;

            // Emit Reverse Index to scroll region down
            for _ in 0..scroll_amount {
                queue!(writer, ReverseIndex)?;
            }

            queue!(writer, ResetScrollRegion)?;

            new_viewport_top = viewport_top + scroll_amount;
        }
    }

    // Now set scroll region from top of screen to viewport top
    // This is where we'll insert the history lines
    let cursor_top = new_viewport_top.saturating_sub(1);

    if new_viewport_top > 0 {
        queue!(writer, SetScrollRegion(1..new_viewport_top))?;
        queue!(writer, MoveTo(0, cursor_top))?;

        // Write each wrapped line
        for line in &wrapped {
            queue!(writer, Print("\r\n"))?;

            // Set line-level colors
            queue!(
                writer,
                SetColors(Colors::new(
                    line.style
                        .fg
                        .map_or(CColor::Reset, IntoCrossterm::into_crossterm),
                    line.style
                        .bg
                        .map_or(CColor::Reset, IntoCrossterm::into_crossterm)
                ))
            )?;

            queue!(writer, Clear(ClearType::UntilNewLine))?;

            // Write spans with merged styles
            let merged_spans: Vec<Span<'_>> = line
                .spans
                .iter()
                .map(|s| Span {
                    style: s.style.patch(line.style),
                    content: s.content.clone(),
                })
                .collect();

            write_styled_spans(writer, &merged_spans)?;
        }

        queue!(writer, ResetScrollRegion)?;
    }

    // Restore cursor position
    queue!(writer, MoveTo(cursor_restore_pos.0, cursor_restore_pos.1))?;

    writer.flush()?;

    Ok(new_viewport_top)
}

// ─────────────────────────────────────────────────────────────────────────────
// REVERSE INDEX COMMAND
// ─────────────────────────────────────────────────────────────────────────────

/// Reverse Index (RI) - Move cursor up one line, scrolling if at top of region.
///
/// When the cursor is at the top margin of the scroll region, this command
/// scrolls the content down by one line (inserting a blank line at the top).
///
/// # ANSI Sequence
///
/// `ESC M` - Reverse Index
struct ReverseIndex;

impl Command for ReverseIndex {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1bM")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LINE WRAPPING FOR TERMINAL
// ─────────────────────────────────────────────────────────────────────────────

/// Wrap lines for terminal output using word-aware wrapping.
///
/// This ensures the terminal scrollback displays wrapped content consistently
/// with the TUI rendering.
fn wrap_lines_for_terminal<'a>(lines: &'a [Line<'a>], width: usize) -> Vec<Line<'a>> {
    let mut result = Vec::new();

    for line in lines {
        let wrapped = crate::wrapping::word_wrap_line(line, width);
        result.extend(wrapped);
    }

    result
}

// ─────────────────────────────────────────────────────────────────────────────
// STYLED SPAN OUTPUT
// ─────────────────────────────────────────────────────────────────────────────

/// Write styled spans to a writer using ANSI escape sequences.
///
/// Unlike ratatui's own `Buffer::set_stringn`, this writer has no built-in
/// control-character filtering (that is the point of this module: native
/// scrollback bypasses the `Buffer` entirely), so `span.content` is
/// sanitized here before being queued for write.
fn write_styled_spans<W: Write>(writer: &mut W, spans: &[Span<'_>]) -> io::Result<()> {
    let mut current_fg = Color::Reset;
    let mut current_bg = Color::Reset;
    let mut current_modifier = Modifier::empty();

    for span in spans {
        let mut modifier = Modifier::empty();
        modifier.insert(span.style.add_modifier);
        modifier.remove(span.style.sub_modifier);

        // Apply modifier changes
        if modifier != current_modifier {
            apply_modifier_diff(writer, current_modifier, modifier)?;
            current_modifier = modifier;
        }

        // Apply color changes
        let next_fg = span.style.fg.unwrap_or(Color::Reset);
        let next_bg = span.style.bg.unwrap_or(Color::Reset);

        if next_fg != current_fg || next_bg != current_bg {
            queue!(
                writer,
                SetColors(Colors::new(
                    next_fg.into_crossterm(),
                    next_bg.into_crossterm(),
                ))
            )?;
            current_fg = next_fg;
            current_bg = next_bg;
        }

        // Write content, sanitized: this writer deliberately bypasses
        // ratatui's `Buffer`, which is what normally filters control
        // characters (including ESC) out of rendered content. A span whose
        // content is *exactly* a trusted, already-sanitized OSC 8 hyperlink
        // wrapper (see `crate::hyperlink::is_exact_sanitized_hyperlink`) is
        // passed through unchanged instead: blanket-stripping it would
        // remove the wrapper's own ESC/BEL bytes and silently break a
        // legitimate `file://` link written via `crate::hyperlink`.
        let content = if crate::hyperlink::is_exact_sanitized_hyperlink(&span.content) {
            span.content.to_string()
        } else {
            crate::output_sanitize::sanitize_control_chars(&span.content)
        };
        queue!(writer, Print(content))?;
    }

    // Reset all attributes at end
    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(Attribute::Reset)
    )?;

    Ok(())
}

/// Apply the difference between two modifier sets.
fn apply_modifier_diff<W: Write>(writer: &mut W, from: Modifier, to: Modifier) -> io::Result<()> {
    let removed = from - to;
    let added = to - from;

    // Remove modifiers
    if removed.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(Attribute::NoReverse))?;
    }
    if removed.contains(Modifier::BOLD) {
        queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(Attribute::Dim))?;
        }
    }
    if removed.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::NoItalic))?;
    }
    if removed.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::NoUnderline))?;
    }
    if removed.contains(Modifier::DIM) {
        queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(Attribute::NotCrossedOut))?;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(Attribute::NoBlink))?;
    }

    // Add modifiers
    if added.contains(Modifier::REVERSED) {
        queue!(writer, SetAttribute(Attribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(writer, SetAttribute(Attribute::Italic))?;
    }
    if added.contains(Modifier::UNDERLINED) {
        queue!(writer, SetAttribute(Attribute::Underlined))?;
    }
    if added.contains(Modifier::DIM) {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(writer, SetAttribute(Attribute::CrossedOut))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(writer, SetAttribute(Attribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(writer, SetAttribute(Attribute::RapidBlink))?;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SCROLL REGION UTILITIES
// ─────────────────────────────────────────────────────────────────────────────

/// Scroll a region down by N lines using Reverse Index.
///
/// This scrolls content within the current scroll region downward,
/// inserting blank lines at the top.
pub fn scroll_region_down<W: Write>(
    writer: &mut W,
    region_start: u16,
    region_end: u16,
    lines: u16,
) -> io::Result<()> {
    if lines == 0 {
        return Ok(());
    }

    queue!(writer, SetScrollRegion(region_start..region_end))?;
    queue!(writer, MoveTo(0, region_start.saturating_sub(1)))?;

    for _ in 0..lines {
        queue!(writer, ReverseIndex)?;
    }

    queue!(writer, ResetScrollRegion)?;
    writer.flush()
}

/// Scroll a region up by N lines using newlines.
///
/// This scrolls content within the current scroll region upward,
/// inserting blank lines at the bottom.
pub fn scroll_region_up<W: Write>(
    writer: &mut W,
    region_start: u16,
    region_end: u16,
    lines: u16,
) -> io::Result<()> {
    if lines == 0 {
        return Ok(());
    }

    queue!(writer, SetScrollRegion(region_start..region_end))?;
    queue!(writer, MoveTo(0, region_end.saturating_sub(1)))?;

    for _ in 0..lines {
        queue!(writer, Print("\n"))?;
    }

    queue!(writer, ResetScrollRegion)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn reverse_index_emits_correct_sequence() {
        let mut buf = String::new();
        ReverseIndex.write_ansi(&mut buf).unwrap();
        assert_eq!(buf, "\x1bM");
    }

    #[test]
    fn wrap_lines_handles_empty() {
        let lines: Vec<Line<'_>> = vec![];
        let wrapped = wrap_lines_for_terminal(&lines, 80);
        assert!(wrapped.is_empty());
    }

    #[test]
    fn wrap_lines_preserves_short_lines() {
        let lines = vec![Line::from("short")];
        let wrapped = wrap_lines_for_terminal(&lines, 80);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn modifier_diff_applies_bold() {
        let mut buf = Vec::new();
        apply_modifier_diff(&mut buf, Modifier::empty(), Modifier::BOLD).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b[1m")); // Bold escape sequence
    }

    #[test]
    fn modifier_diff_removes_bold() {
        let mut buf = Vec::new();
        apply_modifier_diff(&mut buf, Modifier::BOLD, Modifier::empty()).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b[22m")); // Normal intensity
    }

    #[test]
    fn write_styled_spans_outputs_colors() {
        let spans = vec![
            Span::styled("red", Style::default().fg(Color::Red)),
            Span::raw(" normal"),
        ];

        let mut buf = Vec::new();
        write_styled_spans(&mut buf, &spans).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("red"));
        assert!(output.contains("normal"));
    }

    #[test]
    fn write_styled_spans_strips_control_chars_from_content() {
        // A minimal OSC-0 (set title) sequence embedded in span content.
        let spans = vec![Span::raw("before\x1b]0;evil\x07after")];

        let mut buf = Vec::new();
        write_styled_spans(&mut buf, &spans).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("before]0;evilafter"));
        assert!(!output.contains("\x1b]0;evil\x07"));
        assert!(!output.contains('\x07'));
    }

    #[test]
    fn write_styled_spans_preserves_trusted_file_link_wrapper() {
        let link_text = crate::hyperlink::wrap_in_link("file:///tmp/report.md", "report.md");
        let spans = vec![Span::raw(link_text.clone())];

        let mut buf = Vec::new();
        write_styled_spans(&mut buf, &spans).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains(&link_text),
            "a trusted, already-sanitized OSC 8 wrapper must survive intact, \
             got: {output:?}"
        );
    }

    #[test]
    fn write_styled_spans_preserves_ordinary_text() {
        let spans = vec![Span::raw("plain\ttext\nhere")];

        let mut buf = Vec::new();
        write_styled_spans(&mut buf, &spans).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("plain\ttext\nhere"));
    }
}
