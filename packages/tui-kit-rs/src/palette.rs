//! Terminal color palette with intelligent fallbacks
//!
//! Detects terminal color capabilities and provides appropriate colors.

use ratatui::style::Color;

/// Terminal color capability level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    /// No color support
    None,
    /// Basic 16 colors
    Basic,
    /// 256 colors (xterm)
    Indexed,
    /// True color (16 million colors)
    TrueColor,
}

impl ColorLevel {
    /// Detect the terminal's color capability
    #[must_use]
    pub fn detect() -> Self {
        // Check for explicit disabling
        if std::env::var("NO_COLOR").is_ok() {
            return Self::None;
        }

        // Check for force color
        if let Ok(force) = std::env::var("FORCE_COLOR") {
            if force == "0" {
                return Self::None;
            }
        }

        // Use supports-color crate for detection
        if let Some(level) = supports_color::on_cached(supports_color::Stream::Stdout) {
            if level.has_16m {
                return Self::TrueColor;
            }
            if level.has_256 {
                return Self::Indexed;
            }
            if level.has_basic {
                return Self::Basic;
            }
        }

        // Fallback: check COLORTERM
        if let Ok(colorterm) = std::env::var("COLORTERM") {
            if colorterm == "truecolor" || colorterm == "24bit" {
                return Self::TrueColor;
            }
        }

        // Fallback: check TERM
        if let Ok(term) = std::env::var("TERM") {
            if term.contains("256color") {
                return Self::Indexed;
            }
            if term.contains("color") || term.contains("xterm") {
                return Self::Basic;
            }
        }

        Self::Basic
    }
}

static COLOR_LEVEL: std::sync::LazyLock<ColorLevel> = std::sync::LazyLock::new(ColorLevel::detect);

/// Get the detected color level
#[must_use]
pub fn color_level() -> ColorLevel {
    *COLOR_LEVEL
}

/// Check if true color is available
#[must_use]
pub fn has_true_color() -> bool {
    *COLOR_LEVEL == ColorLevel::TrueColor
}

/// Perceptual distance between two colors (simplified CIEDE2000)
#[must_use]
pub fn color_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    // Simple Euclidean distance in RGB space
    // For better results, we could use LAB color space
    let dr = (f64::from(a.0) - f64::from(b.0)) * 0.30;
    let dg = (f64::from(a.1) - f64::from(b.1)) * 0.59;
    let db = (f64::from(a.2) - f64::from(b.2)) * 0.11;
    (dr * dr + dg * dg + db * db).sqrt()
}

/// Convert RGB to the best available color
#[must_use]
pub fn best_color(r: u8, g: u8, b: u8) -> Color {
    color_for_level(r, g, b, color_level())
}

/// Resolve a color for a supplied capability, including deterministic previews/tests.
#[must_use]
pub fn color_for_level(r: u8, g: u8, b: u8, level: ColorLevel) -> Color {
    match level {
        ColorLevel::TrueColor => Color::Rgb(r, g, b),
        ColorLevel::Indexed => {
            // Find closest xterm 256 color
            let target = (r, g, b);
            let (idx, _) = XTERM_COLORS
                .iter()
                .enumerate()
                .skip(16) // Skip the first 16 (theme-dependent) colors
                .min_by(|(_, a), (_, b)| {
                    color_distance(**a, target)
                        .partial_cmp(&color_distance(**b, target))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or((7, &(192, 192, 192)));
            Color::Indexed(idx as u8)
        }
        ColorLevel::Basic => {
            // Map to basic 16 colors
            let target = (r, g, b);
            let (idx, _) = BASIC_COLORS
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    color_distance(**a, target)
                        .partial_cmp(&color_distance(**b, target))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or((7, &(192, 192, 192)));
            Color::Indexed(idx as u8)
        }
        ColorLevel::None => Color::Reset,
    }
}

/// Blend two RGB colors
#[must_use]
pub fn blend(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let r = (f32::from(a.0) * (1.0 - t) + f32::from(b.0) * t) as u8;
    let g = (f32::from(a.1) * (1.0 - t) + f32::from(b.1) * t) as u8;
    let b_out = (f32::from(a.2) * (1.0 - t) + f32::from(b.2) * t) as u8;
    (r, g, b_out)
}

/// Theme colors
pub mod theme {
    use super::{Color, best_color};

    // Text hierarchy
    #[must_use]
    pub fn text() -> Color {
        best_color(229, 229, 229)
    }

    #[must_use]
    pub fn muted() -> Color {
        best_color(156, 163, 175)
    }

    #[must_use]
    pub fn dim() -> Color {
        best_color(107, 114, 128)
    }

    // Semantic colors
    #[must_use]
    pub fn success() -> Color {
        best_color(34, 197, 94)
    }

    #[must_use]
    pub fn warning() -> Color {
        best_color(234, 179, 8)
    }

    #[must_use]
    pub fn danger() -> Color {
        best_color(239, 68, 68)
    }

    #[must_use]
    pub fn info() -> Color {
        best_color(59, 130, 246)
    }

    // Accents
    #[must_use]
    pub fn accent_cool() -> Color {
        best_color(139, 92, 246)
    }

    #[must_use]
    pub fn accent_warm() -> Color {
        best_color(249, 115, 22)
    }

    // Structural
    #[must_use]
    pub fn border() -> Color {
        best_color(55, 65, 81)
    }

    #[must_use]
    pub fn separator() -> Color {
        best_color(75, 85, 99)
    }

    // Syntax highlighting
    #[must_use]
    pub fn syntax_keyword() -> Color {
        best_color(198, 120, 221)
    }

    #[must_use]
    pub fn syntax_string() -> Color {
        best_color(152, 195, 121)
    }

    #[must_use]
    pub fn syntax_number() -> Color {
        best_color(209, 154, 102)
    }

    #[must_use]
    pub fn syntax_comment() -> Color {
        best_color(92, 99, 112)
    }

    #[must_use]
    pub fn syntax_function() -> Color {
        best_color(97, 175, 239)
    }

    #[must_use]
    pub fn syntax_type() -> Color {
        best_color(229, 192, 123)
    }
}

// Basic 16 ANSI colors
const BASIC_COLORS: [(u8, u8, u8); 16] = [
    (0, 0, 0),       // 0 Black
    (128, 0, 0),     // 1 Red
    (0, 128, 0),     // 2 Green
    (128, 128, 0),   // 3 Yellow
    (0, 0, 128),     // 4 Blue
    (128, 0, 128),   // 5 Magenta
    (0, 128, 128),   // 6 Cyan
    (192, 192, 192), // 7 White
    (128, 128, 128), // 8 Bright Black
    (255, 0, 0),     // 9 Bright Red
    (0, 255, 0),     // 10 Bright Green
    (255, 255, 0),   // 11 Bright Yellow
    (0, 0, 255),     // 12 Bright Blue
    (255, 0, 255),   // 13 Bright Magenta
    (0, 255, 255),   // 14 Bright Cyan
    (255, 255, 255), // 15 Bright White
];

// Xterm 256 color palette
const XTERM_COLORS: [(u8, u8, u8); 256] = [
    // Standard colors (0-15)
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
    // 216 color cube (16-231)
    (0, 0, 0),
    (0, 0, 95),
    (0, 0, 135),
    (0, 0, 175),
    (0, 0, 215),
    (0, 0, 255),
    (0, 95, 0),
    (0, 95, 95),
    (0, 95, 135),
    (0, 95, 175),
    (0, 95, 215),
    (0, 95, 255),
    (0, 135, 0),
    (0, 135, 95),
    (0, 135, 135),
    (0, 135, 175),
    (0, 135, 215),
    (0, 135, 255),
    (0, 175, 0),
    (0, 175, 95),
    (0, 175, 135),
    (0, 175, 175),
    (0, 175, 215),
    (0, 175, 255),
    (0, 215, 0),
    (0, 215, 95),
    (0, 215, 135),
    (0, 215, 175),
    (0, 215, 215),
    (0, 215, 255),
    (0, 255, 0),
    (0, 255, 95),
    (0, 255, 135),
    (0, 255, 175),
    (0, 255, 215),
    (0, 255, 255),
    (95, 0, 0),
    (95, 0, 95),
    (95, 0, 135),
    (95, 0, 175),
    (95, 0, 215),
    (95, 0, 255),
    (95, 95, 0),
    (95, 95, 95),
    (95, 95, 135),
    (95, 95, 175),
    (95, 95, 215),
    (95, 95, 255),
    (95, 135, 0),
    (95, 135, 95),
    (95, 135, 135),
    (95, 135, 175),
    (95, 135, 215),
    (95, 135, 255),
    (95, 175, 0),
    (95, 175, 95),
    (95, 175, 135),
    (95, 175, 175),
    (95, 175, 215),
    (95, 175, 255),
    (95, 215, 0),
    (95, 215, 95),
    (95, 215, 135),
    (95, 215, 175),
    (95, 215, 215),
    (95, 215, 255),
    (95, 255, 0),
    (95, 255, 95),
    (95, 255, 135),
    (95, 255, 175),
    (95, 255, 215),
    (95, 255, 255),
    (135, 0, 0),
    (135, 0, 95),
    (135, 0, 135),
    (135, 0, 175),
    (135, 0, 215),
    (135, 0, 255),
    (135, 95, 0),
    (135, 95, 95),
    (135, 95, 135),
    (135, 95, 175),
    (135, 95, 215),
    (135, 95, 255),
    (135, 135, 0),
    (135, 135, 95),
    (135, 135, 135),
    (135, 135, 175),
    (135, 135, 215),
    (135, 135, 255),
    (135, 175, 0),
    (135, 175, 95),
    (135, 175, 135),
    (135, 175, 175),
    (135, 175, 215),
    (135, 175, 255),
    (135, 215, 0),
    (135, 215, 95),
    (135, 215, 135),
    (135, 215, 175),
    (135, 215, 215),
    (135, 215, 255),
    (135, 255, 0),
    (135, 255, 95),
    (135, 255, 135),
    (135, 255, 175),
    (135, 255, 215),
    (135, 255, 255),
    (175, 0, 0),
    (175, 0, 95),
    (175, 0, 135),
    (175, 0, 175),
    (175, 0, 215),
    (175, 0, 255),
    (175, 95, 0),
    (175, 95, 95),
    (175, 95, 135),
    (175, 95, 175),
    (175, 95, 215),
    (175, 95, 255),
    (175, 135, 0),
    (175, 135, 95),
    (175, 135, 135),
    (175, 135, 175),
    (175, 135, 215),
    (175, 135, 255),
    (175, 175, 0),
    (175, 175, 95),
    (175, 175, 135),
    (175, 175, 175),
    (175, 175, 215),
    (175, 175, 255),
    (175, 215, 0),
    (175, 215, 95),
    (175, 215, 135),
    (175, 215, 175),
    (175, 215, 215),
    (175, 215, 255),
    (175, 255, 0),
    (175, 255, 95),
    (175, 255, 135),
    (175, 255, 175),
    (175, 255, 215),
    (175, 255, 255),
    (215, 0, 0),
    (215, 0, 95),
    (215, 0, 135),
    (215, 0, 175),
    (215, 0, 215),
    (215, 0, 255),
    (215, 95, 0),
    (215, 95, 95),
    (215, 95, 135),
    (215, 95, 175),
    (215, 95, 215),
    (215, 95, 255),
    (215, 135, 0),
    (215, 135, 95),
    (215, 135, 135),
    (215, 135, 175),
    (215, 135, 215),
    (215, 135, 255),
    (215, 175, 0),
    (215, 175, 95),
    (215, 175, 135),
    (215, 175, 175),
    (215, 175, 215),
    (215, 175, 255),
    (215, 215, 0),
    (215, 215, 95),
    (215, 215, 135),
    (215, 215, 175),
    (215, 215, 215),
    (215, 215, 255),
    (215, 255, 0),
    (215, 255, 95),
    (215, 255, 135),
    (215, 255, 175),
    (215, 255, 215),
    (215, 255, 255),
    (255, 0, 0),
    (255, 0, 95),
    (255, 0, 135),
    (255, 0, 175),
    (255, 0, 215),
    (255, 0, 255),
    (255, 95, 0),
    (255, 95, 95),
    (255, 95, 135),
    (255, 95, 175),
    (255, 95, 215),
    (255, 95, 255),
    (255, 135, 0),
    (255, 135, 95),
    (255, 135, 135),
    (255, 135, 175),
    (255, 135, 215),
    (255, 135, 255),
    (255, 175, 0),
    (255, 175, 95),
    (255, 175, 135),
    (255, 175, 175),
    (255, 175, 215),
    (255, 175, 255),
    (255, 215, 0),
    (255, 215, 95),
    (255, 215, 135),
    (255, 215, 175),
    (255, 215, 215),
    (255, 215, 255),
    (255, 255, 0),
    (255, 255, 95),
    (255, 255, 135),
    (255, 255, 175),
    (255, 255, 215),
    (255, 255, 255),
    // Grayscale (232-255)
    (8, 8, 8),
    (18, 18, 18),
    (28, 28, 28),
    (38, 38, 38),
    (48, 48, 48),
    (58, 58, 58),
    (68, 68, 68),
    (78, 78, 78),
    (88, 88, 88),
    (98, 98, 98),
    (108, 108, 108),
    (118, 118, 118),
    (128, 128, 128),
    (138, 138, 138),
    (148, 148, 148),
    (158, 158, 158),
    (168, 168, 168),
    (178, 178, 178),
    (188, 188, 188),
    (198, 198, 198),
    (208, 208, 208),
    (218, 218, 218),
    (228, 228, 228),
    (238, 238, 238),
];

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn color_distance_same() {
        assert_eq!(color_distance((0, 0, 0), (0, 0, 0)), 0.0);
    }

    #[test]
    fn blend_midpoint() {
        let result = blend((0, 0, 0), (100, 100, 100), 0.5);
        assert_eq!(result, (50, 50, 50));
    }

    #[test]
    fn blend_extremes() {
        let a = (10, 20, 30);
        let b = (100, 200, 255);
        assert_eq!(blend(a, b, 0.0), a);
        assert_eq!(blend(a, b, 1.0), b);
    }
}
