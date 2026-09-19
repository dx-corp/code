//! Syntax highlighting for code blocks
//!
//! This module provides syntax highlighting for code blocks using the `syntect` library,
//! which is based on Sublime Text's syntax definitions. It converts highlighted code into
//! ratatui-compatible styled lines for terminal rendering.
//!
//! # External Crates
//!
//! - **syntect**: Provides syntax highlighting using Sublime Text's .sublime-syntax files.
//!   Includes built-in support for 100+ languages and themes.
//! - **once_cell**: Used for lazy static initialization of syntax and theme sets, avoiding
//!   repeated loading overhead.
//!
//! # Language Support
//!
//! Supports all languages included in syntect's default syntax set, including:
//! - Programming languages: Rust, Python, JavaScript, TypeScript, Java, C/C++, Go, Ruby, etc.
//! - Markup: HTML, XML, Markdown, YAML, JSON
//! - Shell scripts: Bash, Zsh, PowerShell
//! - Configuration: TOML, YAML, INI
//!
//! Language detection works via:
//! 1. Exact token match (e.g., "rust", "python")
//! 2. File extension match (e.g., "rs", "py")
//! 3. Common aliases (e.g., "js" -> "javascript", "ts" -> "typescript")
//!
//! # Theme
//!
//! Selects a light or dark syntax palette from the current canvas and adapts
//! foreground colors to the terminal capabilities, including NO_COLOR.
//!
//! # Example
//!
//! ```
//! use maestro_tui::syntax::highlight_code;
//!
//! let code = "fn main() {\n    println!(\"Hello\");\n}";
//! let lines = highlight_code(code, Some("rust"));
//! // `lines` contains styled ratatui Line instances ready for rendering
//! ```

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Global syntax set loaded lazily on first use.
///
/// Uses syntect's `load_defaults_newlines()` which includes syntax definitions for
/// 100+ languages. Loaded only once and shared across all highlighting operations.
///
/// Lazy initialization avoids the ~10-20ms startup cost if syntax highlighting is
/// never used (e.g., when rendering plain text conversations).
static SYNTAX_SET: std::sync::LazyLock<SyntaxSet> =
    std::sync::LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Global theme set loaded lazily on first use.
///
/// Contains syntect's default themes including the "base16-eighties.dark" theme
/// used by this module. Lazy initialization keeps it out of the hot path.
static THEME_SET: std::sync::LazyLock<ThemeSet> = std::sync::LazyLock::new(ThemeSet::load_defaults);

/// Convert syntect `FontStyle` bitflags to ratatui Modifier.
///
/// Syntect uses bitflags for text styling (bold, italic, underline), while ratatui
/// uses its own `Modifier` type. This function bridges between the two representations.
fn font_style_to_modifier(font_style: FontStyle) -> Modifier {
    let mut modifier = Modifier::empty();
    if font_style.contains(FontStyle::BOLD) {
        modifier |= Modifier::BOLD;
    }
    if font_style.contains(FontStyle::ITALIC) {
        modifier |= Modifier::ITALIC;
    }
    if font_style.contains(FontStyle::UNDERLINE) {
        modifier |= Modifier::UNDERLINED;
    }
    modifier
}

/// Highlight a code block and return styled lines
///
/// # Arguments
/// * `code` - The code to highlight
/// * `language` - The language hint (e.g., "rust", "python", "javascript")
///
/// # Returns
/// A vector of styled lines suitable for ratatui
#[must_use]
pub fn highlight_code(code: &str, language: Option<&str>) -> Vec<Line<'static>> {
    // Try to find syntax for the language
    let syntax = language
        .and_then(|lang| {
            // Try exact match first
            SYNTAX_SET
                .find_syntax_by_token(lang)
                .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))
                .or_else(|| {
                    // Try common aliases
                    let alias = match lang.to_lowercase().as_str() {
                        "js" => "javascript",
                        "ts" => "typescript",
                        "tsx" => "typescript",
                        "jsx" => "javascript",
                        "py" => "python",
                        "rb" => "ruby",
                        "rs" => "rust",
                        "sh" | "bash" | "zsh" => "shell",
                        "yml" => "yaml",
                        "md" => "markdown",
                        "c++" | "cpp" | "cxx" => "c++",
                        "dockerfile" => "dockerfile",
                        _ => lang,
                    };
                    SYNTAX_SET
                        .find_syntax_by_token(alias)
                        .or_else(|| SYNTAX_SET.find_syntax_by_extension(alias))
                })
        })
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());

    let ui_theme = crate::themes::current_theme();
    let theme = &THEME_SET.themes[syntax_theme_name(&ui_theme)];
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut result = Vec::new();

    for line in code.lines() {
        let ranges = highlighter.highlight_line(line, &SYNTAX_SET);

        match ranges {
            Ok(ranges) => {
                let spans: Vec<Span<'static>> = ranges
                    .iter()
                    .map(|(style, text)| {
                        let fg = crate::palette::best_color(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        );
                        let modifier = font_style_to_modifier(style.font_style);
                        Span::styled(
                            (*text).to_string(),
                            Style::default().fg(fg).add_modifier(modifier),
                        )
                    })
                    .collect();
                result.push(Line::from(spans));
            }
            Err(_) => {
                // Fallback to unstyled text on error
                result.push(Line::from(Span::raw(line.to_string())));
            }
        }
    }

    result
}

// Choose contrast from the source palette, before terminal color quantization.
fn syntax_theme_name(theme: &crate::themes::Theme) -> &'static str {
    let rgb = theme.colors.assistant_message_bg.trim_start_matches('#');
    let light = rgb
        .get(..6)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .is_some_and(|rgb| {
            crate::color_utils::is_light(((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
        });
    if light {
        "InspiredGitHub"
    } else {
        "base16-eighties.dark"
    }
}

/// Check if syntax highlighting is available for a language.
///
/// Returns `true` if syntect has a syntax definition for the given language identifier.
/// The identifier can be a language name (e.g., "rust") or file extension (e.g., "rs").
///
/// # Example
///
/// ```
/// use maestro_tui::syntax::has_syntax;
///
/// assert!(has_syntax("rust"));
/// assert!(has_syntax("python"));
/// assert!(!has_syntax("nonexistent_language"));
/// ```
pub fn has_syntax(language: &str) -> bool {
    SYNTAX_SET.find_syntax_by_token(language).is_some()
        || SYNTAX_SET.find_syntax_by_extension(language).is_some()
}

/// Get a list of all supported language names.
///
/// Returns the canonical names of all languages with syntax definitions in the
/// default syntect syntax set. Useful for autocomplete or language selection UIs.
///
/// Note: The returned references have static lifetime because they point into
/// the lazy-loaded `SYNTAX_SET`.
pub fn supported_languages() -> Vec<&'static str> {
    SYNTAX_SET
        .syntaxes()
        .iter()
        .map(|s| s.name.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_contrast_follows_palette_before_terminal_quantization() {
        assert_eq!(
            syntax_theme_name(&crate::themes::light_theme()),
            "InspiredGitHub"
        );
        assert_eq!(
            syntax_theme_name(&crate::themes::dark_theme()),
            "base16-eighties.dark"
        );
    }

    #[test]
    fn test_highlight_rust() {
        let code = "fn main() {\n    println!(\"Hello, world!\");\n}";
        let lines = highlight_code(code, Some("rust"));
        assert_eq!(lines.len(), 3);
        // Should have multiple spans per line (syntax highlighting)
        assert!(!lines[0].spans.is_empty());
    }

    #[test]
    fn test_highlight_python() {
        let code = "def hello():\n    print(\"Hello\")";
        let lines = highlight_code(code, Some("python"));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_highlight_javascript() {
        let code = "const x = 42;\nconsole.log(x);";
        let lines = highlight_code(code, Some("js"));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_highlight_unknown_language() {
        let code = "some random text\nmore text";
        let lines = highlight_code(code, Some("unknown_lang_xyz"));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_highlight_no_language() {
        let code = "plain text";
        let lines = highlight_code(code, None);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_has_syntax() {
        assert!(has_syntax("rust"));
        assert!(has_syntax("python"));
        assert!(has_syntax("javascript"));
        assert!(!has_syntax("completely_fake_language_xyz"));
    }

    #[test]
    fn test_supported_languages() {
        let languages = supported_languages();
        assert!(!languages.is_empty());
        // Should include common languages
        assert!(languages.iter().any(|l| l.to_lowercase().contains("rust")));
    }

    #[test]
    fn test_language_aliases() {
        // Test that aliases work
        let js_code = "const x = 1;";
        let lines_js = highlight_code(js_code, Some("js"));
        let lines_javascript = highlight_code(js_code, Some("javascript"));
        // Both should produce highlighted output
        assert!(!lines_js.is_empty());
        assert!(!lines_javascript.is_empty());
    }
}
