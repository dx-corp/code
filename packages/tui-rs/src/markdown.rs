//! Markdown rendering for terminal display
//!
//! This module converts markdown text to styled ratatui `Text` using the pulldown-cmark
//! parser. It handles all standard markdown features including headings, code blocks,
//! lists, emphasis, links, and blockquotes, applying appropriate terminal styling.
//!
//! # Rendering Pipeline
//!
//! The rendering process follows these steps:
//!
//! 1. Parse markdown using `pulldown-cmark::Parser` with extended features
//! 2. Process events through `MarkdownRenderer` which maintains style state
//! 3. Syntax highlight code blocks using the `syntax` module
//! 4. Convert to ratatui `Text` with styled `Line` and `Span` elements
//!
//! # Supported Features
//!
//! - **Headings** (H1-H6): Bold semantic headings without source markers
//! - **Code blocks**: Fenced code with language-specific syntax highlighting
//! - **Inline code**: Backtick-delimited code with distinct styling
//! - **Lists**: Both ordered and unordered, with proper indentation
//! - **Emphasis**: Italic (*text*), bold (**text**), strikethrough (~~text~~)
//! - **Links**: Displayed as styled text with URL appended in parentheses
//! - **Blockquotes**: Rendered with vertical bar prefix
//! - **Tables**: Width-aware columns with stacked fields on narrow terminals
//! - **Horizontal rules**: Rendered as separator lines
//!
//! # External Crates
//!
//! - `pulldown-cmark`: CommonMark-compliant markdown parser that provides an
//!   event-based streaming API for efficient parsing
//! - `ratatui`: Terminal UI framework for styled text rendering
//!
//! # Example
//!
//! ```
//! use maestro_tui::markdown::render_markdown;
//!
//! let markdown = "# Hello\n\nThis is **bold** and *italic*.";
//! let text = render_markdown(markdown);
//! // `text` is now a ratatui::text::Text ready for rendering
//! ```

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::hyperlink;
use crate::syntax;
use maestro_ui::wrapping::{RtOptions, word_wrap_lines};

/// Style configuration for markdown elements.
///
/// This struct defines the visual appearance of different markdown elements when
/// rendered in the terminal. Each field corresponds to a specific markdown construct
/// and contains a ratatui `Style` with foreground/background colors and modifiers.
///
/// The default styles are designed for readability in dark terminal themes.
#[derive(Clone)]
pub struct MarkdownStyles {
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub h4: Style,
    pub h5: Style,
    pub h6: Style,
    pub code: Style,
    pub code_block: Style,
    pub emphasis: Style,
    pub strong: Style,
    pub strikethrough: Style,
    pub link: Style,
    pub blockquote: Style,
    pub list_marker: Style,
}

impl Default for MarkdownStyles {
    fn default() -> Self {
        Self::for_theme(&crate::themes::current_theme())
    }
}

impl MarkdownStyles {
    fn for_theme(theme: &crate::themes::Theme) -> Self {
        let foreground = |name| Style::default().fg(theme.get_color(name).unwrap_or(Color::Reset));
        let heading = foreground("md_heading").add_modifier(Modifier::BOLD);
        Self {
            h1: heading,
            h2: heading,
            h3: heading,
            h4: heading,
            h5: heading,
            h6: heading,
            code: foreground("md_code"),
            code_block: foreground("text"),
            emphasis: Style::default().add_modifier(Modifier::ITALIC),
            strong: Style::default().add_modifier(Modifier::BOLD),
            strikethrough: Style::default().add_modifier(Modifier::CROSSED_OUT),
            link: foreground("md_link").add_modifier(Modifier::UNDERLINED),
            blockquote: foreground("muted"),
            list_marker: foreground("muted"),
        }
    }
}

/// Render markdown text to ratatui Text.
///
/// This is the main entry point for converting markdown strings to styled terminal
/// output. It parses the markdown and returns a `Text` instance ready for rendering
/// with ratatui.
///
/// # Arguments
///
/// - `input`: The markdown source text
///
/// # Returns
///
/// A ratatui `Text<'static>` with styled lines and spans
///
/// # Example
///
/// ```
/// use maestro_tui::markdown::render_markdown;
///
/// let text = render_markdown("**Bold** and *italic*");
/// ```
#[must_use]
pub fn render_markdown(input: &str) -> Text<'static> {
    render_markdown_with_width(input, None)
}

/// Render markdown with optional width limit for wrapping.
///
/// This function provides the same functionality as `render_markdown` but with
/// an optional cell width for table layout. The caller owns outer paragraph wrapping.
#[must_use]
pub fn render_markdown_with_width(input: &str, width: Option<usize>) -> Text<'static> {
    render_with_styles(input, width, MarkdownStyles::default())
}

fn render_with_styles(input: &str, width: Option<usize>, styles: MarkdownStyles) -> Text<'static> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, options);
    let mut renderer = MarkdownRenderer::new(styles, width.unwrap_or(80).max(1));
    renderer.render(parser);
    renderer.into_text()
}

/// Table cells keep their styled text and fit the same width used by the transcript.
#[derive(Default)]
struct TableLayout {
    rows: Vec<Vec<Line<'static>>>,
    row: Vec<Line<'static>>,
}

impl TableLayout {
    fn render(self, width: usize, muted: Style) -> Vec<Line<'static>> {
        let columns = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return Vec::new();
        }
        let mut widths = vec![1; columns];
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.width());
            }
        }
        let gaps = columns.saturating_sub(1) * 2;
        // At small widths, stacked labeled fields retain every cell instead of clipping.
        if width < columns * 8 + gaps {
            let mut lines = Vec::new();
            let headers = &self.rows[0];
            if self.rows.len() == 1 {
                return headers
                    .iter()
                    .flat_map(|cell| {
                        word_wrap_lines(std::slice::from_ref(cell), RtOptions::new(width))
                    })
                    .collect();
            }
            for row in self.rows.iter().skip(1) {
                for (i, cell) in row.iter().enumerate() {
                    let mut spans = headers.get(i).map_or_else(Vec::new, |h| h.spans.clone());
                    spans.push(Span::styled(": ", muted));
                    spans.extend(cell.spans.clone());
                    lines.extend(word_wrap_lines(&[Line::from(spans)], RtOptions::new(width)));
                }
                lines.push(Line::from(""));
            }
            return lines;
        }
        while widths.iter().sum::<usize>() + gaps > width {
            let Some((i, longest)) = widths.iter().enumerate().max_by_key(|(_, n)| *n) else {
                break;
            };
            if *longest <= 1 {
                break;
            }
            widths[i] -= 1;
        }
        let mut lines = Vec::new();
        for (row_index, row) in self.rows.into_iter().enumerate() {
            let cells: Vec<Vec<Line<'static>>> = (0..columns)
                .map(|i| {
                    let cell = row.get(i).cloned().unwrap_or_default();
                    word_wrap_lines(&[cell], RtOptions::new(widths[i]))
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for y in 0..height {
                let mut spans = Vec::new();
                for (i, cell) in cells.iter().enumerate() {
                    let line = cell.get(y).cloned().unwrap_or_default();
                    let used = line.width();
                    spans.extend(line.spans);
                    if i + 1 < columns {
                        spans.push(Span::raw(" ".repeat(widths[i].saturating_sub(used) + 2)));
                    }
                }
                lines.push(Line::from(spans));
            }
            if row_index == 0 {
                lines.push(Line::styled(
                    widths
                        .iter()
                        .map(|w| "─".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("  "),
                    muted,
                ));
            }
        }
        lines
    }
}

/// Internal renderer state for processing markdown events.
///
/// This struct maintains rendering state as it processes the stream of events from
/// pulldown-cmark. It uses a stack-based approach for nested styles and list tracking
/// to properly handle nested markdown constructs.
///
/// # Style Stack
///
/// Styles are composed using a stack, where each nested construct (emphasis, strong,
/// link) pushes a new combined style onto the stack. This allows proper handling of
/// nested emphasis like ***bold italic***.
///
/// # List State
///
/// Lists are tracked with a stack of `Option<u64>` where:
/// - `None` indicates an unordered list (bullet points)
/// - `Some(n)` indicates an ordered list starting at number `n`
///
/// This allows proper rendering of nested lists with correct indentation and markers.
struct LinkState {
    url: String,
    label: String,
    spans: Vec<Span<'static>>,
    has_rendered_segment: bool,
}

struct MarkdownRenderer {
    styles: MarkdownStyles,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>, // None = unordered, Some(n) = ordered starting at n
    in_code_block: bool,
    code_block_content: String,
    code_block_lang: Option<String>,
    blockquote_depth: usize,
    /// Current link target and visible label while parsing `[label](url)`.
    current_link: Option<LinkState>,
    width: usize,
    table: Option<TableLayout>,
}

impl MarkdownRenderer {
    fn new(styles: MarkdownStyles, width: usize) -> Self {
        Self {
            styles,
            lines: Vec::new(),
            current_spans: Vec::new(),
            style_stack: vec![Style::default()],
            list_stack: Vec::new(),
            in_code_block: false,
            code_block_content: String::new(),
            code_block_lang: None,
            blockquote_depth: 0,
            current_link: None,
            width,
            table: None,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, style: Style) {
        let combined = self.current_style().patch(style);
        self.style_stack.push(combined);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        if !self.current_spans.is_empty() {
            let mut spans = Vec::new();

            // Add blockquote prefix if needed
            for _ in 0..self.blockquote_depth {
                spans.push(Span::styled("│ ", self.styles.blockquote));
            }

            spans.append(&mut self.current_spans);
            self.lines.push(Line::from(spans));
        }
        self.current_spans = Vec::new();
    }

    fn add_text(&mut self, text: &str) {
        let style = self.current_style();
        if self.in_code_block {
            self.code_block_content.push_str(text);
        } else if let Some(link) = self.current_link.as_mut() {
            link.label.push_str(text);
            link.spans.push(Span::styled(text.to_string(), style));
        } else {
            self.current_spans
                .push(Span::styled(text.to_string(), style));
        }
    }

    fn add_inline_code(&mut self, code: &str) {
        if let Some(link) = self.current_link.as_mut() {
            link.label.push_str(code);
            link.spans
                .push(Span::styled(code.to_owned(), self.styles.code));
            return;
        }
        self.current_spans
            .push(Span::styled(code.to_owned(), self.styles.code));
    }

    fn add_soft_break(&mut self) {
        let style = self.current_style();
        if let Some(link) = self.current_link.as_mut() {
            link.label.push(' ');
            link.spans.push(Span::styled(" ", style));
            return;
        }
        self.current_spans.push(Span::raw(" "));
    }

    fn append_link_spans(&mut self, url: &str, label: &str, spans: Vec<Span<'static>>) {
        if url.starts_with("file://") {
            if spans.is_empty() {
                self.current_spans
                    .push(hyperlink::link_span(url, label, self.styles.link));
            } else {
                for span in spans {
                    self.current_spans.push(Span::styled(
                        hyperlink::wrap_in_link(url, span.content.as_ref()),
                        span.style,
                    ));
                }
            }
            return;
        }
        if spans.is_empty() {
            self.current_spans
                .push(Span::styled(label.to_string(), self.styles.link));
        } else {
            self.current_spans.extend(spans);
        }
    }

    fn add_hard_break(&mut self) {
        let pending_link_segment = if let Some(link) = self.current_link.as_mut() {
            if link.label.is_empty() && link.spans.is_empty() {
                None
            } else {
                let segment_spans = std::mem::take(&mut link.spans);
                let segment_label = std::mem::take(&mut link.label);
                let url = link.url.clone();
                Some((url, segment_label, segment_spans))
            }
        } else {
            None
        };
        if let Some((url, label, spans)) = pending_link_segment {
            self.append_link_spans(&url, &label, spans);
            if let Some(link) = self.current_link.as_mut() {
                link.has_rendered_segment = true;
            }
        }
        self.flush_line();
    }

    fn render(&mut self, parser: Parser<'_>) {
        for event in parser {
            match event {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(text) => self.add_text(&text),
                Event::Code(code) => self.add_inline_code(&code),
                Event::SoftBreak => self.add_soft_break(),
                Event::HardBreak => self.add_hard_break(),
                Event::Rule => {
                    self.flush_line();
                    self.lines.push(Line::from(Span::styled(
                        "─".repeat(self.width.min(40)),
                        self.styles.blockquote,
                    )));
                }
                _ => {}
            }
        }
        self.flush_line();
    }

    fn start_tag(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_line();
                let style = match level {
                    HeadingLevel::H1 => self.styles.h1,
                    HeadingLevel::H2 => self.styles.h2,
                    HeadingLevel::H3 => self.styles.h3,
                    HeadingLevel::H4 => self.styles.h4,
                    HeadingLevel::H5 => self.styles.h5,
                    HeadingLevel::H6 => self.styles.h6,
                };
                self.push_style(style);
            }
            Tag::Paragraph if self.list_stack.is_empty() => {
                self.flush_line();
            }
            Tag::Table(_) => {
                self.flush_line();
                self.table = Some(TableLayout::default());
            }
            Tag::TableHead => self.push_style(self.styles.strong),
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.code_block_content.clear();
                self.code_block_lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_line();
                // Add list marker
                let marker = if let Some(Some(n)) = self.list_stack.last_mut() {
                    let marker = format!("{n}. ");
                    *n += 1;
                    marker
                } else {
                    "• ".to_string()
                };
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                self.current_spans.push(Span::styled(
                    format!("{indent}{marker}"),
                    self.styles.list_marker,
                ));
            }
            Tag::Emphasis => {
                self.push_style(self.styles.emphasis);
            }
            Tag::Strong => {
                self.push_style(self.styles.strong);
            }
            Tag::Strikethrough => {
                self.push_style(self.styles.strikethrough);
            }
            Tag::Link { dest_url, .. } => {
                self.push_style(self.styles.link);
                self.current_link = Some(LinkState {
                    url: dest_url.to_string(),
                    label: String::new(),
                    spans: Vec::new(),
                    has_rendered_segment: false,
                });
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush_line();
                self.lines.push(Line::from("")); // blank line after heading
            }
            TagEnd::Paragraph => {
                self.flush_line();
                if self.list_stack.is_empty() {
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table
                        .row
                        .push(Line::from(std::mem::take(&mut self.current_spans)));
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if matches!(tag, TagEnd::TableHead) {
                    self.pop_style();
                }
                if let Some(table) = &mut self.table {
                    table.rows.push(std::mem::take(&mut table.row));
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.lines
                        .extend(table.render(self.width, self.styles.blockquote));
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                let lang = self.code_block_lang.as_deref();
                if let Some(label) = lang {
                    self.lines
                        .push(Line::styled(label.to_owned(), self.styles.blockquote));
                }
                self.lines
                    .extend(syntax::highlight_code(&self.code_block_content, lang));
                self.lines.push(Line::from(""));

                self.code_block_content.clear();
                self.code_block_lang = None;
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.flush_line();
                    self.lines.push(Line::from("")); // blank line after list
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Emphasis => {
                self.pop_style();
            }
            TagEnd::Strong => {
                self.pop_style();
            }
            TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style();
                if let Some(link) = self.current_link.take() {
                    if link.label.is_empty() && link.spans.is_empty() && link.has_rendered_segment {
                        if !link.url.starts_with("file://") {
                            self.current_spans.push(Span::styled(
                                format!(" ({})", link.url),
                                self.styles.blockquote,
                            ));
                        }
                        return;
                    }
                    let label = if link.label.is_empty() {
                        link.url.as_str()
                    } else {
                        link.label.as_str()
                    };
                    self.append_link_spans(&link.url, label, link.spans);
                    if !link.url.starts_with("file://") {
                        self.current_spans.push(Span::styled(
                            format!(" ({})", link.url),
                            self.styles.blockquote,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    fn into_text(mut self) -> Text<'static> {
        // Remove trailing empty lines
        while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_markdown_line_with_theme(text: &str, theme: &crate::themes::Theme) -> Line<'static> {
        render_with_styles(text, None, MarkdownStyles::for_theme(theme))
            .lines
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    #[test]
    fn inline_code_uses_readable_theme_ink_without_terminal_dimming() {
        for name in [
            "light",
            "green",
            "pink",
            "blue",
            "green-dark",
            "pink-dark",
            "blue-dark",
        ] {
            let theme = crate::themes::load_theme(name).unwrap();
            let line = parse_markdown_line_with_theme("Run `cargo test`.", &theme);
            let code = line
                .spans
                .iter()
                .find(|span| span.content.trim_matches('`') == "cargo test")
                .unwrap();
            assert_eq!(code.style.fg, theme.get_color("md_code"));
            assert!(!code.style.add_modifier.contains(Modifier::DIM));
        }
        let dark = crate::themes::dark_theme();
        let code = parse_markdown_line_with_theme("`cargo test`", &dark);
        assert_eq!(code.spans[0].style.fg, dark.get_color("md_code"));
        assert!(!code.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn markdown_links_respect_distinct_theme_link_colors() {
        let mut custom = crate::themes::light_theme();
        custom.colors.md_heading = "#ff0000".into();
        custom.colors.md_link = "#00ff00".into();
        assert_ne!(custom.get_color("md_link"), custom.get_color("md_heading"));
        for theme in [crate::themes::light_theme(), custom] {
            let line = parse_markdown_line_with_theme("See [guide](https://example.com).", &theme);
            let link = line
                .spans
                .iter()
                .find(|span| span.content == "guide")
                .unwrap();
            assert_eq!(link.style.fg, theme.get_color("md_link"));
            assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        }
        let line = parse_markdown_line_with_theme(
            "[guide](https://example.com)",
            &crate::themes::dark_theme(),
        );
        assert_eq!(
            line.spans[0].style.fg,
            crate::themes::dark_theme().get_color("md_link")
        );
    }

    fn visible(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn headings_and_code_use_visual_hierarchy_without_source_fences() {
        let text = render_markdown("## Result\n\n```rust\nfn main() {}\n```");
        let lines = visible(&text);
        assert_eq!(lines[0], "Result");
        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(lines.iter().any(|line| line == "rust"));
        assert!(lines.iter().any(|line| line == "fn main() {}"));
        assert!(!lines.iter().any(|line| line.contains("```")));
    }

    #[test]
    fn tables_preserve_cells_at_wide_and_narrow_widths() {
        let input = "| Item | Status |\n| --- | --- |\n| 世界 | Ready |\n| Build | Passed |";
        for width in [12, 24, 60, 100] {
            let text = render_markdown_with_width(input, Some(width));
            assert!(text.lines.iter().all(|line| line.width() <= width));
            let rendered = visible(&text).join("\n");
            for cell in ["Item", "Status", "世界", "Ready", "Build", "Passed"] {
                assert!(
                    rendered.contains(cell),
                    "missing {cell} at {width}: {rendered}"
                );
            }
        }
    }

    #[test]
    fn nested_and_loose_lists_keep_their_markers() {
        let text = render_markdown("- First\n\n- Second\n  - Nested");
        let lines = visible(&text);
        assert!(lines.iter().any(|line| line == "• First"));
        assert!(lines.iter().any(|line| line == "• Second"));
        assert!(lines.iter().any(|line| line == "  • Nested"));
    }

    #[test]
    fn incomplete_streamed_code_remains_visible() {
        let text = render_markdown("```rust\nlet answer = 42;");
        assert!(visible(&text).iter().any(|line| line == "let answer = 42;"));
    }

    #[test]
    fn inline_code_removes_only_markdown_delimiters() {
        let text = render_markdown("Run `cargo test`, then `` echo `date` ``.");
        assert_eq!(visible(&text), ["Run cargo test, then echo `date`."]);
        let link = render_markdown("[`src/main.rs`](file:///tmp/src/main.rs)");
        assert_eq!(
            crate::hyperlink::strip_hyperlinks(&visible(&link).join("\n")),
            "src/main.rs"
        );
    }

    #[test]
    fn horizontal_rules_fit_the_viewport_and_use_theme_ink() {
        let styles = MarkdownStyles::for_theme(&crate::themes::light_theme());
        for width in [1, 12, 24, 60] {
            let text = render_with_styles("---", Some(width), styles.clone());
            assert_eq!(text.lines.len(), 1);
            assert_eq!(text.lines[0].width(), width.min(40));
            assert_eq!(text.lines[0].spans[0].style, styles.blockquote);
        }
    }

    #[test]
    fn renders_plain_text() {
        let text = render_markdown("Hello, world!");
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn renders_heading() {
        let text = render_markdown("# Heading 1");
        assert!(
            text.lines
                .iter()
                .any(|l| { l.spans.iter().any(|s| s.content.contains("Heading")) })
        );
    }

    #[test]
    fn renders_code_block() {
        let text = render_markdown("```rust\nfn main() {}\n```");
        // With syntax highlighting, tokens may be split across spans
        // Check that the code content is present by concatenating all spans
        let all_content: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all_content.contains("fn") && all_content.contains("main"));
    }

    #[test]
    fn renders_list() {
        let text = render_markdown("* Item 1\n* Item 2");
        assert!(text.lines.len() >= 2);
    }

    #[test]
    fn renders_emphasis() {
        let text = render_markdown("This is *italic* text");
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn renders_file_uri_links_as_terminal_hyperlinks() {
        let text =
            render_markdown("See [src/main.ts](file:///Users/alice/work/maestro/src/main.ts#L42).");
        let rendered: String = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();

        assert!(crate::hyperlink::contains_hyperlink(&rendered));
        assert_eq!(
            crate::hyperlink::extract_urls(&rendered),
            vec!["file:///Users/alice/work/maestro/src/main.ts#L42"]
        );
        assert_eq!(
            crate::hyperlink::strip_hyperlinks(&rendered),
            "See src/main.ts."
        );
    }

    #[test]
    fn keeps_soft_breaks_inside_file_uri_link_labels() {
        let text =
            render_markdown("See [src\nmain.ts](file:///Users/alice/work/maestro/src/main.ts).");
        let rendered: String = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();

        assert!(crate::hyperlink::contains_hyperlink(&rendered));
        assert_eq!(
            crate::hyperlink::strip_hyperlinks(&rendered),
            "See src main.ts."
        );
    }

    #[test]
    fn preserves_hard_breaks_inside_file_uri_link_labels() {
        let text =
            render_markdown("See [src  \nmain.ts](file:///Users/alice/work/maestro/src/main.ts).");
        let rendered_lines: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let rendered = rendered_lines.join("\n");

        assert!(crate::hyperlink::contains_hyperlink(&rendered));
        assert_eq!(
            rendered_lines
                .iter()
                .map(|line| crate::hyperlink::strip_hyperlinks(line))
                .collect::<Vec<_>>(),
            vec!["See src".to_string(), "main.ts.".to_string()]
        );
    }

    #[test]
    fn skips_empty_hard_break_file_uri_link_segments() {
        let text =
            render_markdown("See [\\\nmain.ts](file:///Users/alice/work/maestro/src/main.ts).");
        let rendered_lines: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let visible_lines: Vec<String> = rendered_lines
            .iter()
            .map(|line| crate::hyperlink::strip_hyperlinks(line))
            .collect();

        assert!(crate::hyperlink::contains_hyperlink(
            &rendered_lines.join("\n")
        ));
        assert!(
            visible_lines
                .iter()
                .all(|line| !line.contains("file:///Users/alice/work/maestro/src/main.ts"))
        );
        assert_eq!(
            visible_lines,
            vec!["See ".to_string(), "main.ts.".to_string()]
        );
    }

    #[test]
    fn skips_consecutive_empty_hard_break_file_uri_link_segments() {
        let text = render_markdown(
            "See [src\\\n\\\nmain.ts](file:///Users/alice/work/maestro/src/main.ts).",
        );
        let rendered_lines: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let visible_lines: Vec<String> = rendered_lines
            .iter()
            .map(|line| crate::hyperlink::strip_hyperlinks(line))
            .collect();

        assert!(crate::hyperlink::contains_hyperlink(
            &rendered_lines.join("\n")
        ));
        assert!(
            !visible_lines
                .iter()
                .any(|line| line.contains("file:///Users/alice/work/maestro/src/main.ts"))
        );
        assert_eq!(
            visible_lines,
            vec!["See src".to_string(), "main.ts.".to_string()]
        );
    }

    #[test]
    fn skips_empty_link_end_after_hard_break_file_uri_segment() {
        let text = render_markdown("See [src\\\n](file:///Users/alice/work/maestro/src/main.ts)");
        let rendered_lines: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let visible_lines: Vec<String> = rendered_lines
            .iter()
            .map(|line| crate::hyperlink::strip_hyperlinks(line))
            .collect();

        assert!(crate::hyperlink::contains_hyperlink(
            &rendered_lines.join("\n")
        ));
        assert!(
            visible_lines
                .iter()
                .all(|line| !line.contains("file:///Users/alice/work/maestro/src/main.ts"))
        );
        assert_eq!(visible_lines, vec!["See src".to_string()]);
    }

    #[test]
    fn keeps_non_file_url_fallback_after_empty_hard_break_link_end() {
        let text = render_markdown("See [the docs\\\n](https://example.com/docs)");
        let rendered_lines: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            rendered_lines,
            vec![
                "See the docs".to_string(),
                " (https://example.com/docs)".to_string()
            ]
        );
    }

    #[test]
    fn keeps_non_file_links_visible_for_terminal_fallback() {
        let text = render_markdown("Read [the docs](https://example.com/docs).");
        let rendered: String = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();

        assert!(!crate::hyperlink::contains_hyperlink(&rendered));
        assert_eq!(rendered, "Read the docs (https://example.com/docs).");
    }

    #[test]
    fn preserves_nested_styles_in_non_file_links() {
        let text = render_markdown("Read [the **bold** docs](https://example.com/docs).");
        let rendered: String = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        let bold_span = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "bold")
            .expect("bold link label span should be preserved");

        assert_eq!(rendered, "Read the bold docs (https://example.com/docs).");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold_span.style.add_modifier.contains(Modifier::UNDERLINED));
    }
}
