//! Animated spinner for loading indicators
//!
//! Provides a blinking dot spinner that works with or without true color.

use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::shimmer::shimmer_spans;

/// Check if terminal supports true color
fn has_true_color() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v == "truecolor" || v == "24bit")
        .unwrap_or(false)
}

/// Create an animated spinner span.
///
/// - With true color: uses shimmer effect on the bullet
/// - Without: alternates between filled and hollow bullet
#[must_use]
pub fn spinner(start_time: Option<Instant>, animations_enabled: bool) -> Span<'static> {
    if !animations_enabled {
        return Span::styled("*", Style::default().add_modifier(Modifier::DIM));
    }

    let elapsed = start_time.map(|st| st.elapsed()).unwrap_or_default();

    if has_true_color() {
        // Use shimmer effect
        shimmer_spans("*")
            .into_iter()
            .next()
            .unwrap_or_else(|| Span::raw("*"))
    } else {
        // Fallback: blink between filled and hollow
        let blink_on = (elapsed.as_millis() / 600) & 1 == 0;
        if blink_on {
            Span::raw("*")
        } else {
            Span::styled(".", Style::default().add_modifier(Modifier::DIM))
        }
    }
}

/// Braille spinner frames for more animated effect
pub const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Get a braille spinner frame based on elapsed time
#[must_use]
pub fn braille_spinner(start_time: Option<Instant>) -> Span<'static> {
    let elapsed = start_time.map(|st| st.elapsed()).unwrap_or_default();
    let frame_idx = (elapsed.as_millis() / 80) as usize % BRAILLE_FRAMES.len();
    Span::styled(
        BRAILLE_FRAMES[frame_idx],
        Style::default()
            .fg(Color::Rgb(
                crate::shimmer::DEIXIC_VIOLET.0,
                crate::shimmer::DEIXIC_VIOLET.1,
                crate::shimmer::DEIXIC_VIOLET.2,
            ))
            .add_modifier(Modifier::BOLD),
    )
}

/// Simple dot spinner frames
pub const DOT_FRAMES: &[&str] = &["   ", ".  ", ".. ", "..."];

/// Get a dot spinner frame
#[must_use]
pub fn dot_spinner(start_time: Option<Instant>) -> &'static str {
    let elapsed = start_time.map(|st| st.elapsed()).unwrap_or_default();
    let frame_idx = (elapsed.as_millis() / 400) as usize % DOT_FRAMES.len();
    DOT_FRAMES[frame_idx]
}
