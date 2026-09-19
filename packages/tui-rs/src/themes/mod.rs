//! Theme System for the TUI
//!
//! This module provides a comprehensive theming system that supports built-in themes
//! and user-defined custom themes loaded from JSON files. Themes control all colors
//! used in the UI, from message backgrounds to syntax highlighting.
//!
//! # Built-in Themes
//!
//! These themes are included out of the box:
//!
//! - **dark** (default): Dark background with soft, eye-friendly colors
//! - **light**: Light background suitable for bright environments
//! - **green**, **pink**, **blue**: Gentle, tinted full-canvas palettes
//! - **high-contrast**: Maximum contrast for accessibility
//!
//! # Custom Themes
//!
//! Users can create custom themes by placing JSON files in:
//!
//! - Global: `~/.composer/themes/<name>.json`
//! - Project: `.composer/themes/<name>.json`
//!
//! ## Theme JSON Format
//!
//! ```json
//! {
//!   "name": "my-theme",
//!   "colors": {
//!     "accent": "#7dd3fc",
//!     "border": "#334155",
//!     "text": "#e2e8f0",
//!     "error": "#fca5a5",
//!     "success": "#86efac",
//!     "md_heading": "#60a5fa",
//!     "syntax_keyword": "#c084fc"
//!   }
//! }
//! ```
//!
//! # Color Format
//!
//! Colors are specified as hex strings:
//!
//! - `#RRGGBB` - Standard hex color (e.g., `#ff0000` for red)
//! - `#RRGGBBAA` - Hex with alpha (alpha is ignored, for compatibility)
//! - `transparent` - No color (uses terminal default)
//!
//! # Color Categories
//!
//! Themes define colors for several categories:
//!
//! - **Core**: `accent`, `border`, `success`, `error`, `warning`, `text`, `muted`, `dim`
//! - **Messages**: `user_message_bg`, `assistant_message_bg`, etc.
//! - **Tools**: `tool_pending_bg`, `tool_success_bg`, `tool_error_bg`
//! - **Markdown**: `md_heading`, `md_link`, `md_code`, `md_quote`
//! - **Syntax**: `syntax_keyword`, `syntax_function`, `syntax_string`, etc.
//! - **Thinking**: `thinking_off`, `thinking_low`, `thinking_medium`, `thinking_high`
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use maestro_tui::themes::{set_theme_by_name, current_theme, available_themes};
//!
//! // List available themes
//! for name in available_themes() {
//!     println!("Theme: {}", name);
//! }
//!
//! // Switch theme
//! set_theme_by_name("light").expect("theme should exist");
//!
//! // Use current theme for styling
//! let theme = current_theme();
//! let heading_style = theme.fg("md_heading");
//! ```
//!
//! # Thread Safety
//!
//! The current theme is stored in a `RwLock` for thread-safe access:
//!
//! - Multiple threads can read the theme simultaneously
//! - Theme changes acquire an exclusive write lock
//! - Theme changes are atomic (no partial updates visible)
//!
//! # Terminal Color Adaptation
//!
//! Colors are automatically adapted to the terminal's color capabilities:
//!
//! - **True Color** (16M colors): Full RGB colors used as-is
//! - **256 Color**: Mapped to nearest ANSI 256 color
//! - **16 Color**: Mapped to nearest basic ANSI color
//!
//! This ensures themes look reasonable even on limited terminals.
//!
//! # The `auto` Theme
//!
//! `auto` resolves to `dark` or `light` from `COLORFGBG`. With
//! `tui.theme_follow = true`, typed OSC 11 and DEC light/dark events keep it
//! synchronized with the terminal (see [`osc11`]).

use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;

use crate::palette;

pub mod osc11;

/// VS Code's bundled color assets, mapped offline into the native palette.
static VSCODE_THEMES: std::sync::LazyLock<Vec<Theme>> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("vscode/themes.json"))
        .expect("bundled VS Code theme mappings are validated by tests")
});

/// Global theme state
static CURRENT_THEME: RwLock<Option<Theme>> = RwLock::new(None);

/// A complete theme definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// Theme name
    pub name: String,
    /// Theme colors
    #[serde(default)]
    pub colors: ThemeColors,
    /// Variable definitions (for interpolation)
    #[serde(default)]
    pub vars: HashMap<String, String>,
}

/// Theme color definitions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    // Core colors
    pub accent: String,
    pub border: String,
    pub success: String,
    pub error: String,
    pub warning: String,
    pub muted: String,
    pub dim: String,
    pub text: String,

    // Message colors
    pub user_message_bg: String,
    pub user_message_text: String,
    pub assistant_message_bg: String,
    pub assistant_message_text: String,

    // Tool colors
    pub tool_pending_bg: String,
    pub tool_success_bg: String,
    pub tool_error_bg: String,

    // Markdown colors
    pub md_heading: String,
    pub md_link: String,
    pub md_code: String,
    pub md_code_block: String,
    pub md_code_block_border: String,
    pub md_quote: String,

    // Syntax colors
    pub syntax_comment: String,
    pub syntax_keyword: String,
    pub syntax_function: String,
    pub syntax_variable: String,
    pub syntax_string: String,
    pub syntax_number: String,
    pub syntax_type: String,

    // Thinking indicator colors
    pub thinking_off: String,
    pub thinking_low: String,
    pub thinking_medium: String,
    pub thinking_high: String,
}

impl Default for ThemeColors {
    fn default() -> Self {
        // Default dark theme; previews use the same semantic control colors.
        let controls = maestro_presentation::palette::default_controls();
        let hex = |color: Color| match color {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "transparent".to_string(),
        };
        Self {
            accent: hex(controls.focus),
            border: hex(controls.border),
            success: hex(controls.success),
            error: hex(controls.error),
            warning: hex(controls.attention),
            muted: hex(controls.muted),
            dim: "#6f678f".to_string(),
            text: hex(controls.text),

            user_message_bg: hex(controls.panel.unwrap_or(controls.surface)),
            user_message_text: hex(controls.text),
            assistant_message_bg: hex(controls.surface),
            assistant_message_text: hex(controls.text),

            tool_pending_bg: hex(controls.selection.unwrap_or(controls.surface)),
            tool_success_bg: "#14532d20".to_string(),
            tool_error_bg: "#7f1d1d20".to_string(),

            md_heading: hex(controls.text),
            md_link: hex(controls.focus),
            md_code: "#c2b6e8".to_string(),
            md_code_block: hex(controls.surface),
            md_code_block_border: hex(controls.border),
            md_quote: hex(controls.muted),

            syntax_comment: "#64748b".to_string(),
            syntax_keyword: "#c084fc".to_string(),
            syntax_function: "#60a5fa".to_string(),
            syntax_variable: "#fbbf24".to_string(),
            syntax_string: "#86efac".to_string(),
            syntax_number: "#fb923c".to_string(),
            syntax_type: "#f472b6".to_string(),

            thinking_off: "#64748b".to_string(),
            thinking_low: "#fbbf24".to_string(),
            thinking_medium: "#60a5fa".to_string(),
            thinking_high: "#c084fc".to_string(),
        }
    }
}

impl Theme {
    /// Create a new theme
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            colors: ThemeColors::default(),
            vars: HashMap::new(),
        }
    }

    /// Load a theme from a JSON file
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, ThemeError> {
        let content = std::fs::read_to_string(path.as_ref())
            .map_err(|e| ThemeError::IoError(e.to_string()))?;
        Self::load_from_str(&content)
    }

    /// Load a theme from a JSON string
    pub fn load_from_str(json: &str) -> Result<Self, ThemeError> {
        serde_json::from_str(json).map_err(|e| ThemeError::ParseError(e.to_string()))
    }

    /// Get a color by name
    #[must_use]
    pub fn get_color(&self, name: &str) -> Option<Color> {
        let hex = match name {
            "accent" => &self.colors.accent,
            "border" => &self.colors.border,
            "success" => &self.colors.success,
            "error" => &self.colors.error,
            "warning" => &self.colors.warning,
            "muted" => &self.colors.muted,
            "dim" => &self.colors.dim,
            "text" => &self.colors.text,
            "assistant_message_bg" => &self.colors.assistant_message_bg,
            "user_message_bg" => &self.colors.user_message_bg,
            "user_message_text" => &self.colors.user_message_text,
            "md_heading" => &self.colors.md_heading,
            "md_link" => &self.colors.md_link,
            "md_code" => &self.colors.md_code,
            "syntax_comment" => &self.colors.syntax_comment,
            "syntax_keyword" => &self.colors.syntax_keyword,
            "syntax_function" => &self.colors.syntax_function,
            "syntax_variable" => &self.colors.syntax_variable,
            "syntax_string" => &self.colors.syntax_string,
            "syntax_number" => &self.colors.syntax_number,
            "syntax_type" => &self.colors.syntax_type,
            _ => return None,
        };
        parse_color(hex)
    }

    /// Get a style with foreground color
    #[must_use]
    pub fn fg(&self, color_name: &str) -> Style {
        match self.get_color(color_name) {
            Some(color) => Style::default().fg(color),
            None => Style::default(),
        }
    }

    /// Get a style with background color
    #[must_use]
    pub fn bg(&self, color_name: &str) -> Style {
        match self.get_color(color_name) {
            Some(color) => Style::default().bg(color),
            None => Style::default(),
        }
    }
}

/// Parse a hex color string
fn parse_color(hex: &str) -> Option<Color> {
    if hex == "transparent" || hex.is_empty() {
        return None;
    }

    let hex = hex.trim_start_matches('#');

    // Handle 8-character hex with alpha (ignore alpha)
    let hex = if hex.len() == 8 { &hex[..6] } else { hex };

    if hex.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;

    Some(palette::best_color(r, g, b))
}

/// Error type for theme operations
#[derive(Debug, Clone)]
pub enum ThemeError {
    IoError(String),
    ParseError(String),
    NotFound(String),
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::IoError(msg) => write!(f, "IO error: {msg}"),
            ThemeError::ParseError(msg) => write!(f, "Parse error: {msg}"),
            ThemeError::NotFound(msg) => write!(f, "Theme not found: {msg}"),
        }
    }
}

impl std::error::Error for ThemeError {}

// =============================================================================
// Built-in themes
// =============================================================================

/// Get the dark theme (default)
#[must_use]
pub fn dark_theme() -> Theme {
    Theme::new("dark")
}

/// Get the light theme
#[must_use]
pub fn light_theme() -> Theme {
    let mut theme = Theme::new("light");
    theme.colors = ThemeColors {
        // The darker brand violet keeps small text readable on lavender selection.
        accent: "#5847e6".to_string(),
        border: "#d9d5e8".to_string(),
        success: "#38594c".to_string(),
        error: "#893747".to_string(),
        warning: "#704d2d".to_string(),
        muted: "#645d78".to_string(),
        dim: "#5d5063".to_string(),
        text: "#171624".to_string(),

        user_message_bg: "#f5f3fc".to_string(),
        user_message_text: "#171624".to_string(),
        assistant_message_bg: "#ffffff".to_string(),
        assistant_message_text: "#171624".to_string(),

        tool_pending_bg: "#f5f3fc".to_string(),
        tool_success_bg: "#dfe7dc".to_string(),
        tool_error_bg: "#eedde0".to_string(),

        md_heading: "#5847e6".to_string(),
        md_link: "#5847e6".to_string(),
        md_code: "#68492e".to_string(),
        md_code_block: "#f5f3fc".to_string(),
        md_code_block_border: "#d9d5e8".to_string(),
        md_quote: "#645d78".to_string(),

        syntax_comment: "#5d5063".to_string(),
        syntax_keyword: "#5847e6".to_string(),
        syntax_function: "#5847e6".to_string(),
        syntax_variable: "#68492e".to_string(),
        syntax_string: "#38594c".to_string(),
        syntax_number: "#7a4633".to_string(),
        syntax_type: "#734060".to_string(),

        thinking_off: "#5d5063".to_string(),
        thinking_low: "#704d2d".to_string(),
        thinking_medium: "#5847e6".to_string(),
        thinking_high: "#5847e6".to_string(),
    };
    theme.colors.tool_pending_bg = "#efebff".into();
    theme
}

/// Get the sage green theme, inspired by Everforest's soft surfaces.
#[must_use]
pub fn green_theme() -> Theme {
    tinted_light_theme(
        "green", "#e6ecdf", "#dbe3d2", "#404f43", "#4d5d49", "#3f6247", "#acbba5",
    )
}

/// Get the muted rose theme, inspired by Rosé Pine's warm pinks.
#[must_use]
pub fn pink_theme() -> Theme {
    tinted_light_theme(
        "pink", "#f0e1e6", "#e8d5dd", "#58434f", "#604a57", "#81435c", "#c6a9b8",
    )
}

/// Get the soft blue-gray theme.
#[must_use]
pub fn blue_theme() -> Theme {
    tinted_light_theme(
        "blue", "#e2e9ef", "#d5e0e9", "#414f60", "#4d596c", "#40597f", "#a9bacb",
    )
}

fn tinted_light_theme(
    name: &str,
    surface: &str,
    panel: &str,
    text: &str,
    muted: &str,
    accent: &str,
    border: &str,
) -> Theme {
    let mut theme = light_theme();
    theme.name = name.into();
    theme.colors = ThemeColors {
        accent: accent.into(),
        border: border.into(),
        text: text.into(),
        muted: muted.into(),
        user_message_bg: panel.into(),
        user_message_text: text.into(),
        assistant_message_bg: surface.into(),
        assistant_message_text: text.into(),
        tool_pending_bg: panel.into(),
        md_heading: accent.into(),
        md_link: accent.into(),
        md_code_block: panel.into(),
        md_code_block_border: border.into(),
        md_quote: muted.into(),
        syntax_keyword: accent.into(),
        syntax_function: accent.into(),
        thinking_medium: accent.into(),
        thinking_high: accent.into(),
        ..theme.colors
    };
    theme.colors.tool_pending_bg = match name {
        "green" => "#cfdac5",
        "pink" => "#dfc7d2",
        "blue" => "#c8d6e2",
        _ => panel,
    }
    .into();
    theme
}

/// Full-canvas dark counterparts to the gentle light palettes.
#[must_use]
pub fn tinted_dark_theme(name: &str) -> Option<Theme> {
    let (surface, panel, selection, text, muted, accent, border) = match name {
        "green-dark" => (
            "#222d27", "#2c3830", "#39473d", "#e0e8da", "#b1c0ac", "#b1d3a1", "#829780",
        ),
        "pink-dark" => (
            "#30252e", "#3b2e38", "#493a45", "#eee0e7", "#cbb1c0", "#efb2cd", "#a3899a",
        ),
        "blue-dark" => (
            "#242c37", "#2d3744", "#3a4655", "#e1e8ef", "#b0bfd0", "#adcbee", "#8195ae",
        ),
        _ => return None,
    };
    let mut theme = tinted_light_theme(name, surface, panel, text, muted, accent, border);
    theme.colors.tool_pending_bg = selection.into();
    theme.colors.success = "#b2d1a8".into();
    theme.colors.warning = "#e4c795".into();
    theme.colors.error = "#efb0af".into();
    theme.colors.tool_success_bg = panel.into();
    theme.colors.tool_error_bg = panel.into();
    theme.colors.dim = muted.into();
    theme.colors.md_code = "#e4c795".into();
    theme.colors.syntax_comment = muted.into();
    theme.colors.syntax_variable = text.into();
    theme.colors.syntax_string = "#b2d1a8".into();
    theme.colors.syntax_number = "#e4c795".into();
    theme.colors.syntax_type = accent.into();
    theme.colors.thinking_off = muted.into();
    theme.colors.thinking_low = "#e4c795".into();
    Some(theme)
}

/// Get the high contrast theme
#[must_use]
pub fn high_contrast_theme() -> Theme {
    let mut theme = Theme::new("high-contrast");
    theme.colors = ThemeColors {
        accent: "#00ffff".to_string(),
        border: "#ffffff".to_string(),
        success: "#00ff00".to_string(),
        error: "#ff0000".to_string(),
        warning: "#ffff00".to_string(),
        muted: "#c0c0c0".to_string(),
        dim: "#808080".to_string(),
        text: "#ffffff".to_string(),

        user_message_bg: "#000080".to_string(),
        user_message_text: "#ffffff".to_string(),
        assistant_message_bg: "transparent".to_string(),
        assistant_message_text: "#ffffff".to_string(),

        tool_pending_bg: "#000080".to_string(),
        tool_success_bg: "#004400".to_string(),
        tool_error_bg: "#440000".to_string(),

        md_heading: "#00ffff".to_string(),
        md_link: "#00ffff".to_string(),
        md_code: "#ffff00".to_string(),
        md_code_block: "#000080".to_string(),
        md_code_block_border: "#ffffff".to_string(),
        md_quote: "#c0c0c0".to_string(),

        syntax_comment: "#808080".to_string(),
        syntax_keyword: "#ff00ff".to_string(),
        syntax_function: "#00ffff".to_string(),
        syntax_variable: "#ffff00".to_string(),
        syntax_string: "#00ff00".to_string(),
        syntax_number: "#ff8000".to_string(),
        syntax_type: "#ff00ff".to_string(),

        thinking_off: "#808080".to_string(),
        thinking_low: "#ffff00".to_string(),
        thinking_medium: "#00ffff".to_string(),
        thinking_high: "#ff00ff".to_string(),
    };
    theme
}

// =============================================================================
// Theme management
// =============================================================================

/// Get all available theme names
#[must_use]
pub fn available_themes() -> Vec<String> {
    let mut themes = vec![
        "auto".to_string(),
        "dark".to_string(),
        "light".to_string(),
        "green".to_string(),
        "pink".to_string(),
        "blue".to_string(),
        "green-dark".to_string(),
        "pink-dark".to_string(),
        "blue-dark".to_string(),
        "high-contrast".to_string(),
    ];

    themes.extend(VSCODE_THEMES.iter().map(|theme| theme.name.clone()));

    // Look for user themes
    if let Some(home) = dirs::home_dir() {
        let user_themes_dir = home.join(".composer").join("themes");
        if let Ok(entries) = std::fs::read_dir(&user_themes_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.path().file_stem() {
                    if let Some(name) = name.to_str() {
                        if !themes.contains(&name.to_string()) {
                            themes.push(name.to_string());
                        }
                    }
                }
            }
        }
    }

    // Look for project themes
    let project_themes_dir = std::path::Path::new(".composer/themes");
    if let Ok(entries) = std::fs::read_dir(project_themes_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.path().file_stem() {
                if let Some(name) = name.to_str() {
                    if !themes.contains(&name.to_string()) {
                        themes.push(name.to_string());
                    }
                }
            }
        }
    }

    themes
}

/// Resolve the `auto` theme to a concrete built-in based on the terminal's
/// reported background color.
///
/// Uses the `COLORFGBG` environment variable (set by most xterm-family
/// terminals as `fg;bg`): a background in 0-6 (or 8) reads as dark, 7/15 as
/// light. Falls back to `dark` when the variable is missing or unparsable.
#[must_use]
pub fn resolve_auto_theme_name() -> &'static str {
    resolve_auto_theme_from(std::env::var("COLORFGBG").ok().as_deref())
}

fn resolve_auto_theme_from(colorfgbg: Option<&str>) -> &'static str {
    let background = colorfgbg.and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok());
    match background {
        Some(7 | 15) => "light",
        _ => "dark",
    }
}

/// Load a theme by name
pub fn load_theme(name: &str) -> Result<Theme, ThemeError> {
    // `auto` follows the terminal's background color.
    if name == "auto" {
        return load_theme(resolve_auto_theme_name());
    }

    if let Some(theme) = tinted_dark_theme(name) {
        return Ok(theme);
    }

    // Check built-in themes first
    match name {
        "dark" => return Ok(dark_theme()),
        "light" => return Ok(light_theme()),
        "green" => return Ok(green_theme()),
        "pink" => return Ok(pink_theme()),
        "blue" => return Ok(blue_theme()),
        "high-contrast" => return Ok(high_contrast_theme()),
        _ => {}
    }

    if let Some(theme) = VSCODE_THEMES.iter().find(|theme| theme.name == name) {
        return Ok(theme.clone());
    }

    // Try user themes directory
    if let Some(home) = dirs::home_dir() {
        let path = home
            .join(".composer")
            .join("themes")
            .join(format!("{name}.json"));
        if path.exists() {
            return Theme::load_from_file(&path);
        }
    }

    // Try project themes directory
    let path = std::path::Path::new(".composer/themes").join(format!("{name}.json"));
    if path.exists() {
        return Theme::load_from_file(&path);
    }

    Err(ThemeError::NotFound(name.to_string()))
}

/// Set the current theme
pub fn set_theme(theme: Theme) {
    if let Ok(mut current) = CURRENT_THEME.write() {
        *current = Some(theme);
    }
}

/// Set the current theme by name
pub fn set_theme_by_name(name: &str) -> Result<(), ThemeError> {
    let theme = load_theme(name)?;
    set_theme(theme);
    Ok(())
}

/// Get the current theme
pub fn current_theme() -> Theme {
    if let Ok(current) = CURRENT_THEME.read() {
        if let Some(ref theme) = *current {
            return theme.clone();
        }
    }
    dark_theme()
}

/// Get the current theme name
#[must_use]
pub fn current_theme_name() -> String {
    current_theme().name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_color() {
        let color = parse_color("#ff0000");
        assert!(color.is_some());
    }

    #[test]
    fn parse_transparent() {
        let color = parse_color("transparent");
        assert!(color.is_none());
    }

    #[test]
    fn parse_hex_with_alpha() {
        let color = parse_color("#ff000080");
        assert!(color.is_some());
    }

    #[test]
    fn default_theme_colors() {
        let colors = ThemeColors::default();
        assert!(!colors.accent.is_empty());
        assert!(!colors.text.is_empty());
    }

    #[test]
    fn built_in_themes_exist() {
        let themes = available_themes();
        assert!(themes.contains(&"dark".to_string()));
        assert!(themes.contains(&"light".to_string()));
        assert!(themes.contains(&"high-contrast".to_string()));
    }

    #[test]
    fn load_built_in_theme() {
        let theme = load_theme("dark").unwrap();
        assert_eq!(theme.name, "dark");
    }

    #[test]
    fn theme_get_color() {
        let theme = dark_theme();
        let color = theme.get_color("accent");
        assert!(color.is_some());
    }

    #[test]
    fn set_and_get_theme() {
        set_theme(light_theme());
        assert_eq!(current_theme_name(), "light");
        // Reset to dark
        set_theme(dark_theme());
    }
}

#[cfg(test)]
mod auto_theme_tests {
    use super::resolve_auto_theme_from;

    #[test]
    fn auto_theme_dark_backgrounds() {
        assert_eq!(resolve_auto_theme_from(Some("15;0")), "dark");
        assert_eq!(resolve_auto_theme_from(Some("7;0")), "dark");
        assert_eq!(resolve_auto_theme_from(Some("0;8")), "dark");
    }

    #[test]
    fn auto_theme_light_backgrounds() {
        assert_eq!(resolve_auto_theme_from(Some("0;15")), "light");
        assert_eq!(resolve_auto_theme_from(Some("0;7")), "light");
    }

    #[test]
    fn auto_theme_fallback_is_dark() {
        assert_eq!(resolve_auto_theme_from(None), "dark");
        assert_eq!(resolve_auto_theme_from(Some("garbage")), "dark");
    }
}

/// Translate the active theme into the shared controls' semantic palette.
/// Overlays use the existing code-block surface when message backgrounds are
/// transparent, keeping a light palette readable on a dark terminal.
#[must_use]
pub fn current_ui_theme() -> maestro_ui::UiTheme {
    current_theme().ui_theme()
}

impl Theme {
    /// An explicit theme surface also owns the surrounding chat canvas.
    #[must_use]
    pub fn canvas_style(&self) -> Style {
        parse_color(&self.colors.assistant_message_bg).map_or_else(Style::default, |surface| {
            Style::default()
                .bg(surface)
                .fg(self.get_color("text").unwrap_or(Color::Reset))
        })
    }

    /// Resolve the shared control palette without changing the active theme.
    pub fn ui_theme(&self) -> maestro_ui::UiTheme {
        let theme = self;
        maestro_ui::UiTheme {
            panel: self
                .canvas_style()
                .bg
                .and_then(|_| parse_color(&theme.colors.user_message_bg)),
            selection: self
                .canvas_style()
                .bg
                .and_then(|_| parse_color(&theme.colors.tool_pending_bg)),
            surface: parse_color(&theme.colors.assistant_message_bg)
                .or_else(|| parse_color(&theme.colors.md_code_block))
                .unwrap_or(Color::Reset),
            text: theme.get_color("text").unwrap_or(Color::Reset),
            muted: theme.get_color("muted").unwrap_or(Color::Reset),
            border: theme.get_color("border").unwrap_or(Color::Reset),
            focus: theme.get_color("accent").unwrap_or(Color::Reset),
            success: theme.get_color("success").unwrap_or(Color::Reset),
            attention: theme.get_color("warning").unwrap_or(Color::Reset),
            error: theme.get_color("error").unwrap_or(Color::Reset),
        }
    }
}

#[cfg(test)]
mod ui_theme_tests {
    use super::*;

    #[test]
    fn brand_themes_share_canvas_and_control_colors() {
        let dark = dark_theme();
        let ui = dark.ui_theme();
        let brand = maestro_presentation::palette::default_controls();
        for (actual, expected) in [
            (ui.surface, brand.surface),
            (ui.text, brand.text),
            (ui.muted, brand.muted),
            (ui.border, brand.border),
            (ui.focus, brand.focus),
            (ui.success, brand.success),
            (ui.attention, brand.attention),
            (ui.error, brand.error),
            (ui.panel.unwrap(), brand.panel.unwrap()),
            (ui.selection.unwrap(), brand.selection.unwrap()),
        ] {
            let Color::Rgb(r, g, b) = expected else {
                panic!("brand requires RGB");
            };
            assert_eq!(actual, palette::best_color(r, g, b));
        }
        for theme in [dark, light_theme()] {
            assert_eq!(theme.canvas_style().bg, Some(theme.ui_theme().surface));
            assert_eq!(theme.canvas_style().fg, Some(theme.ui_theme().text));
            assert_eq!(theme.get_color("md_link"), Some(theme.ui_theme().focus));
        }
        let light = light_theme().ui_theme();
        assert_eq!(light.focus, palette::best_color(0x58, 0x47, 0xe6));
        assert_eq!(light.selection, Some(palette::best_color(0xef, 0xeb, 0xff)));
        assert_eq!(light.text, palette::best_color(0x17, 0x16, 0x24));
    }

    #[test]
    fn built_in_control_surfaces_are_opaque_including_light_on_dark_terminals() {
        for theme in [dark_theme(), light_theme(), high_contrast_theme()] {
            let ui = theme.ui_theme();
            assert_ne!(
                ui.surface,
                Color::Reset,
                "{} must not inherit an incompatible terminal background",
                theme.name
            );
            assert_ne!(ui.surface, ui.text);
            assert_eq!(
                ui.surface,
                parse_color(&theme.colors.assistant_message_bg)
                    .or_else(|| parse_color(&theme.colors.md_code_block))
                    .unwrap()
            );
        }
    }

    #[test]
    fn gentle_light_canvas_and_controls_share_readable_opaque_colors() {
        for name in ["light", "green", "pink", "blue"] {
            assert!(available_themes().contains(&name.to_string()));
            let theme = load_theme(name).unwrap();
            assert_eq!(theme.name, name);
            let ui = theme.ui_theme();
            assert_eq!(theme.canvas_style().bg, Some(ui.surface));
            assert_eq!(theme.canvas_style().fg, Some(ui.text));
            assert_eq!(
                dark_theme().canvas_style().bg,
                Some(dark_theme().ui_theme().surface)
            );
            let luminance = |hex: &str| {
                let linear = |offset| {
                    let value =
                        f64::from(u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
                            / 255.0;
                    if value <= 0.04045 {
                        value / 12.92
                    } else {
                        ((value + 0.055) / 1.055).powf(2.4)
                    }
                };
                0.2126 * linear(1) + 0.7152 * linear(3) + 0.0722 * linear(5)
            };
            for foreground in [
                &theme.colors.text,
                &theme.colors.muted,
                &theme.colors.accent,
                &theme.colors.success,
                &theme.colors.warning,
                &theme.colors.error,
            ] {
                let ratio = (luminance(&theme.colors.assistant_message_bg) + 0.05)
                    / (luminance(foreground) + 0.05);
                assert!(
                    ratio >= 4.5,
                    "{} {foreground} has only {ratio:.2}:1 contrast",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn theme_families_keep_text_readable_on_every_surface() {
        fn luminance(hex: &str) -> f64 {
            let linear = |i| {
                let v = f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap()) / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(1) + 0.7152 * linear(3) + 0.0722 * linear(5)
        }
        for name in [
            "light",
            "green",
            "pink",
            "blue",
            "green-dark",
            "pink-dark",
            "blue-dark",
        ] {
            let theme = load_theme(name).unwrap();
            let c = &theme.colors;
            for bg in [
                &c.assistant_message_bg,
                &c.user_message_bg,
                &c.tool_pending_bg,
                &c.md_code_block,
            ] {
                for fg in [
                    &c.text,
                    &c.muted,
                    &c.accent,
                    &c.success,
                    &c.warning,
                    &c.error,
                    &c.md_code,
                    &c.syntax_comment,
                    &c.syntax_keyword,
                    &c.syntax_function,
                    &c.syntax_variable,
                    &c.syntax_string,
                    &c.syntax_number,
                    &c.syntax_type,
                ] {
                    let (a, b) = (luminance(bg), luminance(fg));
                    let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                    assert!(contrast >= 4.5, "{name} {fg} on {bg}: {contrast:.2}:1");
                }
            }
            assert!(available_themes().contains(&name.to_string()));
            assert_ne!(c.assistant_message_bg, c.user_message_bg);
            assert_ne!(c.user_message_bg, c.tool_pending_bg);
        }
    }

    #[test]
    fn limited_color_families_preserve_text_and_surface_separation() {
        for name in [
            "light",
            "green",
            "pink",
            "blue",
            "green-dark",
            "pink-dark",
            "blue-dark",
        ] {
            let theme = load_theme(name).unwrap();
            for level in [palette::ColorLevel::Basic, palette::ColorLevel::Indexed] {
                let convert = |hex: &str| {
                    palette::color_for_level(
                        u8::from_str_radix(&hex[1..3], 16).unwrap(),
                        u8::from_str_radix(&hex[3..5], 16).unwrap(),
                        u8::from_str_radix(&hex[5..7], 16).unwrap(),
                        level,
                    )
                };
                for fg in [
                    &theme.colors.text,
                    &theme.colors.muted,
                    &theme.colors.accent,
                    &theme.colors.success,
                    &theme.colors.warning,
                    &theme.colors.error,
                ] {
                    for bg in [
                        &theme.colors.assistant_message_bg,
                        &theme.colors.user_message_bg,
                        &theme.colors.tool_pending_bg,
                    ] {
                        assert_ne!(
                            convert(fg),
                            convert(bg),
                            "{name}: {fg} collapses onto {bg} at {level:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn bundled_vscode_palettes_are_selectable_and_fully_opaque() {
        let names = available_themes();
        assert!(VSCODE_THEMES.len() >= 19);
        let mut seen = std::collections::HashSet::new();
        for source in VSCODE_THEMES.iter() {
            assert!(seen.insert(&source.name));
            assert_eq!(names.iter().filter(|name| *name == &source.name).count(), 1);
            let theme = load_theme(&source.name).unwrap();
            assert_eq!(
                theme.colors.assistant_message_bg,
                source.colors.assistant_message_bg
            );
            assert_ne!(theme.colors.text, theme.colors.assistant_message_bg);
            for (_, value) in serde_json::to_value(&theme.colors)
                .unwrap()
                .as_object()
                .unwrap()
            {
                let value = value.as_str().unwrap();
                assert_eq!(value.len(), 7, "{}: {value}", theme.name);
                assert!(
                    value.starts_with('#') && value[1..].chars().all(|c| c.is_ascii_hexdigit())
                );
            }
            assert_eq!(theme.canvas_style().bg, Some(theme.ui_theme().surface));
        }
        let monokai = load_theme("vscode-monokai").unwrap();
        assert_eq!(monokai.colors.assistant_message_bg, "#272822");
    }

    #[test]
    fn explicit_message_surface_remains_the_custom_theme_authority() {
        let mut theme = light_theme();
        theme.colors.assistant_message_bg = "#ddeeff".into();
        assert_eq!(theme.ui_theme().surface, parse_color("#ddeeff").unwrap());
    }
}
