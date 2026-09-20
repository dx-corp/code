//! Shimmer animation effect
//!
//! Wall-clock raised-cosine sheen for busy text and multi-line brand art.
//! Motion grammar matches Grok Build's welcome logo (rest between sweeps,
//! soft pulse, ~12 fps quantization) with Deixic violet brand colors from
//! `evalops/deixic` (`#6857fe` solid violet, soft highlight).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::color_utils::{blend, has_true_color_support};

// ─────────────────────────────────────────────────────────────────────────────
// DEIXIC BRAND PALETTE
// ─────────────────────────────────────────────────────────────────────────────

/// Deixic solid violet (`--dx-violet-solid` / `--primary` on platform UI).
pub const DEIXIC_VIOLET: (u8, u8, u8) = (0x68, 0x57, 0xfe);

/// Accessible violet for dark terminal controls; solid brand violet remains unchanged.
pub const DEIXIC_ACCENT: (u8, u8, u8) = (0xab, 0xa0, 0xff);

/// Hover shade of Deixic violet (`--dx-violet-solid-hover` / `#5847e6`).
pub const DEIXIC_VIOLET_HOVER: (u8, u8, u8) = (0x58, 0x47, 0xe6);

/// Soft violet tint (`--dx-violet-soft` / `#efebff`).
pub const DEIXIC_SOFT: (u8, u8, u8) = (0xef, 0xeb, 0xff);

/// Ink used on the marketing site (`--dx-ink`).
pub const DEIXIC_INK: (u8, u8, u8) = (0x17, 0x16, 0x24);

/// Resting (dim) tone for logo glyphs — deeper violet-gray.
pub const DEIXIC_LOGO_BASE: (u8, u8, u8) = (0x4a, 0x42, 0x9a);

/// Peak highlight on logo sheen.
pub const DEIXIC_LOGO_HILITE: (u8, u8, u8) = (0xef, 0xeb, 0xff);

/// Near-black surface used by the focused Maestro stage.
pub const DEIXIC_SURFACE: (u8, u8, u8) = (0x17, 0x16, 0x24);

/// Neutral gray border used for structural chrome.
pub const DEIXIC_BORDER: (u8, u8, u8) = (0x4e, 0x47, 0x64);

/// Muted gray for supporting labels and placeholder text.
pub const DEIXIC_MUTED: (u8, u8, u8) = (0xb0, 0xa8, 0xc6);

/// Soft text color for primary interactive content.
pub const DEIXIC_TEXT: (u8, u8, u8) = (0xef, 0xeb, 0xff);

// ─────────────────────────────────────────────────────────────────────────────
// PROCESS CLOCK
// ─────────────────────────────────────────────────────────────────────────────

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Elapsed time since first shimmer use (shared so effects stay in phase).
#[must_use]
pub fn elapsed_since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

/// Animation phase in seconds (wall-clock, frame-rate independent).
#[must_use]
pub fn anim_phase_secs() -> f32 {
    elapsed_since_start().as_secs_f32()
}

/// Welcome / idle shimmer redraw cadence. Sweep is slow; 12 fps is enough and
/// avoids full event-loop paint rate while idle.
pub const SHIMMER_FPS: f32 = 12.0;

/// Quantized shimmer frame for dirty-tracking idle welcome paints.
#[must_use]
pub fn shimmer_frame() -> u64 {
    (anim_phase_secs() * SHIMMER_FPS) as u64
}

// ─────────────────────────────────────────────────────────────────────────────
// SHINE (Grok-style motion grammar)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-position shine opacity in `[0, 1]`.
///
/// `pos` is a normalized coordinate along the sweep axis (0..1 for 1D text,
/// or diagonal position for multi-line art). A raised-cosine band sweeps
/// with rest between passes; a gentle global pulse sits under it.
#[must_use]
pub fn shine_opacity(pos: f32, secs: f32) -> f32 {
    const BAND: f32 = 0.38;
    const CYCLE: f32 = 4.0;
    const SWEEP_FRAC: f32 = 0.32;
    const SHINE: f32 = 0.45;
    const PULSE: f32 = 0.08;
    const PULSE_SECS: f32 = 5.0;

    let p = (secs % CYCLE) / CYCLE;
    let q = (p / SWEEP_FRAC).min(1.0);
    let band_pos = -BAND + q * (1.0 + 2.0 * BAND);
    let pulse = PULSE * (0.5 - 0.5 * (std::f32::consts::TAU * secs / PULSE_SECS).cos());

    let d = (pos - band_pos).abs();
    let shine = if d < BAND {
        0.5 * (1.0 + (std::f32::consts::PI * d / BAND).cos())
    } else {
        0.0
    };
    (pulse + SHINE * shine).clamp(0.0, 1.0)
}

// ─────────────────────────────────────────────────────────────────────────────
// SHIMMER CONFIG
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the linear text shimmer.
#[derive(Debug, Clone)]
pub struct ShimmerConfig {
    /// Base color for non-highlighted text.
    pub base_color: (u8, u8, u8),
    /// Highlight color for the shimmer peak.
    pub highlight_color: (u8, u8, u8),
    /// Whether true color is supported.
    pub has_true_color: bool,
    /// When true, use continuous legacy sweep (no rest). Prefer false.
    pub continuous: bool,
}

impl Default for ShimmerConfig {
    fn default() -> Self {
        Self {
            // Muted violet-gray resting tone; peak is soft white-violet.
            base_color: (0x6e, 0x68, 0xa8),
            highlight_color: DEIXIC_SOFT,
            has_true_color: true,
            continuous: false,
        }
    }
}

impl ShimmerConfig {
    /// Config with automatic terminal capability detection.
    #[must_use]
    pub fn auto() -> Self {
        Self {
            has_true_color: has_true_color_support(),
            ..Default::default()
        }
    }

    /// Deixic brand defaults (same as [`Default`] with true-color auto).
    #[must_use]
    pub fn deixic() -> Self {
        Self::auto()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LINEAR TEXT SHIMMER
// ─────────────────────────────────────────────────────────────────────────────

/// Create shimmer spans for the given text (Deixic defaults).
#[must_use]
pub fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    shimmer_spans_with_config(text, &ShimmerConfig::auto())
}

/// Create shimmer spans with custom configuration.
#[must_use]
pub fn shimmer_spans_with_config(text: &str, config: &ShimmerConfig) -> Vec<Span<'static>> {
    shimmer_spans_at(text, config, anim_phase_secs())
}

/// Create shimmer spans at a specific elapsed time (tests / locked frames).
#[must_use]
pub fn shimmer_spans_at_time(text: &str, elapsed: Duration) -> Vec<Span<'static>> {
    shimmer_spans_at(text, &ShimmerConfig::auto(), elapsed.as_secs_f32())
}

fn shimmer_spans_at(text: &str, config: &ShimmerConfig, secs: f32) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let n = chars.len().max(1) as f32;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len());

    for (i, ch) in chars.iter().enumerate() {
        let intensity = if config.continuous {
            // Legacy continuous left→right band with padding.
            let padding = 10.0;
            let period = n + padding * 2.0;
            let sweep_seconds = 2.0f32;
            let band_half = 5.0f32;
            let pos_f = (secs % sweep_seconds) / sweep_seconds * period;
            let i_pos = i as f32 + padding;
            let dist = (i_pos - pos_f).abs();
            if dist <= band_half {
                0.5 * (1.0 + (std::f32::consts::PI * (dist / band_half)).cos())
            } else {
                0.0
            }
        } else {
            // Normalized position; rest/pulse via shine_opacity.
            let t = if n <= 1.0 { 0.5 } else { i as f32 / (n - 1.0) };
            shine_opacity(t, secs)
        };

        spans.push(Span::styled(
            ch.to_string(),
            style_for_opacity(intensity, config),
        ));
    }

    spans
}

fn style_for_opacity(opacity: f32, config: &ShimmerConfig) -> Style {
    if config.has_true_color {
        let (r, g, b) = blend(
            config.highlight_color,
            config.base_color,
            opacity.clamp(0.0, 1.0),
        );
        Style::default()
            .fg(Color::Rgb(r, g, b))
            .add_modifier(Modifier::BOLD)
    } else {
        fallback_style_for_intensity(opacity)
    }
}

fn fallback_style_for_intensity(intensity: f32) -> Style {
    if intensity < 0.2 {
        Style::default().add_modifier(Modifier::DIM)
    } else if intensity < 0.6 {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

/// Create a shimmer line from text.
#[must_use]
pub fn shimmer_line(text: &str) -> Line<'static> {
    Line::from(shimmer_spans(text))
}

/// Create a shimmer line with custom configuration.
#[must_use]
pub fn shimmer_line_with_config(text: &str, config: &ShimmerConfig) -> Line<'static> {
    Line::from(shimmer_spans_with_config(text, config))
}

// ─────────────────────────────────────────────────────────────────────────────
// DIAGONAL MULTI-LINE SHEEN (welcome / logo)
// ─────────────────────────────────────────────────────────────────────────────

/// Render multi-line art with a bottom-left → top-right diagonal sheen.
///
/// Adjacent glyphs that share a color are coalesced into one [`Span`] to
/// keep per-frame allocation low.
#[must_use]
pub fn diagonal_shimmer_lines(
    lines: &[&str],
    base: (u8, u8, u8),
    hilite: (u8, u8, u8),
) -> Vec<Line<'static>> {
    diagonal_shimmer_lines_at(
        lines,
        base,
        hilite,
        anim_phase_secs(),
        has_true_color_support(),
    )
}

/// Same as [`diagonal_shimmer_lines`] with an explicit clock (tests).
#[must_use]
pub fn diagonal_shimmer_lines_at(
    lines: &[&str],
    base: (u8, u8, u8),
    hilite: (u8, u8, u8),
    secs: f32,
    true_color: bool,
) -> Vec<Line<'static>> {
    let non_empty: Vec<&str> = lines.iter().copied().filter(|l| !l.is_empty()).collect();
    if non_empty.is_empty() {
        return Vec::new();
    }

    let rows = non_empty.len().max(1) as f32;
    let cols = non_empty
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(1)
        .max(1) as f32;

    non_empty
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;
            let mut run_style: Style = Style::default();

            for (col, ch) in line.chars().enumerate() {
                // Sweep along bottom-left → top-right: col up, row down.
                let diag = (col as f32 + (rows - 1.0 - row as f32)) / (cols + rows);
                let opacity = shine_opacity(diag, secs);
                let (style, color) = if true_color {
                    let (r, g, b) = blend(hilite, base, opacity);
                    let c = Color::Rgb(r, g, b);
                    (Style::default().fg(c), Some(c))
                } else {
                    (fallback_style_for_intensity(opacity), None)
                };

                if true_color {
                    if run_color != color {
                        if let Some(prev) = run_color {
                            spans.push(Span::styled(
                                std::mem::take(&mut run),
                                Style::default().fg(prev),
                            ));
                        }
                        run_color = color;
                    }
                    run.push(ch);
                } else {
                    // Modifier path: coalesce only identical styles.
                    if !run.is_empty() && run_style != style {
                        spans.push(Span::styled(std::mem::take(&mut run), run_style));
                    }
                    run_style = style;
                    run.push(ch);
                }
            }

            if true_color {
                if let Some(prev) = run_color {
                    spans.push(Span::styled(run, Style::default().fg(prev)));
                }
            } else if !run.is_empty() {
                spans.push(Span::styled(run, run_style));
            }

            Line::from(spans)
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// TESTS
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_empty_text() {
        assert!(shimmer_spans("").is_empty());
    }

    #[test]
    fn shimmer_creates_spans_for_each_char() {
        assert_eq!(shimmer_spans("hello").len(), 5);
    }

    #[test]
    fn shimmer_spans_are_styled() {
        for span in shimmer_spans("test") {
            assert_eq!(span.content.chars().count(), 1);
        }
    }

    #[test]
    fn shimmer_at_time_works() {
        assert_eq!(
            shimmer_spans_at_time("hello", Duration::from_millis(500)).len(),
            5
        );
    }

    #[test]
    fn fallback_style_varies_with_intensity() {
        let dim = fallback_style_for_intensity(0.1);
        let normal = fallback_style_for_intensity(0.4);
        let bold = fallback_style_for_intensity(0.8);

        assert!(dim.add_modifier.contains(Modifier::DIM));
        assert!(!normal.add_modifier.contains(Modifier::DIM));
        assert!(bold.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn shimmer_config_auto_creates_config() {
        let config = ShimmerConfig::auto();
        assert_eq!(config.base_color.0, 0x6e);
    }

    #[test]
    fn shimmer_line_creates_line() {
        assert_eq!(shimmer_line("test").spans.len(), 4);
    }

    #[test]
    fn shine_opacity_stays_in_unit_range() {
        let mut secs = 0.0;
        while secs < 10.0 {
            for i in 0..=20 {
                let pos = i as f32 / 20.0;
                let op = shine_opacity(pos, secs);
                assert!(
                    (0.0..=1.0).contains(&op),
                    "opacity {op} out of range at pos {pos}, secs {secs}"
                );
            }
            secs += 0.13;
        }
    }

    #[test]
    fn shine_band_sweeps_across() {
        let brightest = |secs: f32| -> f32 {
            (0..=100)
                .map(|i| i as f32 / 100.0)
                .max_by(|a, b| {
                    shine_opacity(*a, secs)
                        .partial_cmp(&shine_opacity(*b, secs))
                        .unwrap()
                })
                .unwrap()
        };
        let early = brightest(0.1);
        let mid = brightest(0.4);
        let late = brightest(0.7);
        assert!(early < mid, "early {early} should precede mid {mid}");
        assert!(mid < late, "mid {mid} should precede late {late}");
    }

    #[test]
    fn shine_rests_dim_between_sweeps() {
        // secs % 4.0 = 2.0 → past SWEEP_FRAC, rest phase.
        let op = shine_opacity(0.5, 6.0);
        assert!(op < 0.2, "resting opacity {op} should stay dim");
    }

    #[test]
    fn diagonal_shimmer_produces_lines() {
        let art = ["  ██  ", " █  █ ", "  ██  "];
        let lines =
            diagonal_shimmer_lines_at(&art, DEIXIC_LOGO_BASE, DEIXIC_LOGO_HILITE, 0.2, true);
        assert_eq!(lines.len(), 3);
        assert!(!lines[0].spans.is_empty());
    }

    #[test]
    fn shimmer_frame_is_monotonic_with_time() {
        // Just ensure it is a finite counter; absolute value depends on process age.
        let _ = shimmer_frame();
    }

    #[test]
    fn deixic_violet_matches_marketing_hex() {
        assert_eq!(DEIXIC_VIOLET, (0x68, 0x57, 0xfe));
        assert_eq!(DEIXIC_VIOLET_HOVER, (0x58, 0x47, 0xe6));
        assert_eq!(DEIXIC_SOFT, (0xef, 0xeb, 0xff));
    }
}
