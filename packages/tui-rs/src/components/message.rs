//! Message display widgets
//!
//! This module implements the chat message display system, including the main scrollable
//! message list (`ChatView`), individual message rendering (`MessageWidget`), and supporting
//! components like the status bar and input box.
//!
//! # Widget Hierarchy
//!
//! ```text
//! ChatView (main container)
//! ├── MessageWidget (per message)
//! │   ├── Role header with timestamp
//! │   ├── Thinking block (collapsible)
//! │   ├── Markdown content
//! │   └── ToolCallWidget (per tool call)
//! ├── ChatInputWidget (bottom input box)
//! └── StatusBarWidget (bottom status line)
//! ```
//!
//! # Rendering Features
//!
//! ## Markdown Parsing
//!
//! The message content supports inline markdown:
//! - **bold**: `**text**`
//! - `code`: backtick-wrapped inline code
//! - Code blocks: triple-backtick fenced blocks with syntax highlighting hints
//! - Links: `[text](url)` rendered with underline styling
//!
//! Markdown parsing uses the shared block renderer for both measurement and painting.
//!
//! ## Scrolling and Viewport Management
//!
//! `ChatView` implements a virtual scrolling system:
//! - Messages are filtered to only renderable ones (via `should_render_message`)
//! - Heights are pre-calculated for all messages (via `calculate_message_height`)
//! - The viewport is anchored from the bottom by `scroll_offset`
//! - Only messages within the viewport are rendered
//! - A scrollbar is drawn when total content height exceeds viewport height
//!
//! ## Tool Call Display
//!
//! Tool calls are rendered as collapsible sections with:
//! - Status pill: `[RUN]`, `[OK]`, `[ERR]`, `[PEND]` with color coding
//! - Shared tool-phase icons and concise outcome labels
//! - Expandable arguments (JSON pretty-printed)
//! - Output display (truncated to first 10 lines when collapsed)
//! - Click indicator: `[+]` collapsed, `[-]` expanded
//!
//! ## Thinking Blocks
//!
//! Assistant messages may include "thinking" content (Claude's internal reasoning):
//! - Rendered in a collapsible section with gutter (│)
//! - Collapsed: shows only the disclosure header
//! - Expanded: shows full thinking content with italic dim styling
//! - Toggle indicator: `[+]` / `[-]`
//!
//! # Widget Trait Implementation
//!
//! Each widget implements `ratatui::widgets::Widget`:
//!
//! ```rust,ignore
//! impl Widget for MessageWidget<'_> {
//!     fn render(self, area: Rect, buf: &mut Buffer) {
//!         // Write styled text to buffer cells
//!         buf.set_string(x, y, "text", style);
//!         // Or use higher-level Paragraph widget
//!         Paragraph::new(content).render(area, buf);
//!     }
//! }
//! ```
//!
//! ## Stateless Widget Pattern
//!
//! `MessageWidget`, `ToolCallWidget`, `StatusBarWidget` are stateless widgets:
//! - Take references to data (`&'a Message`, `&'a str`)
//! - Consume `self` in `render()` (builder pattern allows chaining)
//! - Do not maintain state across renders
//!
//! ## Stateful Widget Pattern
//!
//! `ChatView` references stateful data (`&'a AppState`) which contains:
//! - Message list
//! - Scroll position
//! - Expanded tool call set
//! - Input state and cursor position
//!
//! # Cursor Positioning
//!
//! `ChatInputWidget` calculates terminal cursor position using:
//! - Unicode width calculation (not byte length)
//! - Text wrapping logic to determine row/column
//! - Clamping to visible area boundaries
//!
//! The cursor position is calculated in `cursor_pos()` and set by the app's render loop.
//!
//! # Keyboard Event Handling
//!
//! This module does NOT handle keyboard events directly. Event handling is performed in
//! the main app loop (src/app.rs), which updates `AppState`. The widgets re-render based
//! on the updated state.
//!
//! # Design Inspiration
//!
//! Visual design inspired by:
//! - TypeScript Maestro TUI
//! - `OpenAI` Codex TUI
//!
//! Features:
//! - Bordered panels and status badges
//! - Tool-specific icons
//! - Shimmer animations for "Working" text
//! - Elapsed time display
//! - Timestamps
//! - Collapsible thinking blocks

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::components::textarea::TextArea;
use crate::effects::shimmer_spans;
use crate::runtime_badges::{RuntimeBadgeParams, build_runtime_badges};
use crate::session::ThinkingLevel;
use crate::shimmer::{DEIXIC_ACCENT, DEIXIC_BORDER, DEIXIC_MUTED, DEIXIC_TEXT};
use crate::state::{
    ApprovalMode, InteractionMode, Message, MessageKind, MessageRole, QueueMode, ToolCallStatus,
};
use crate::tool_output::{clamp_tool_output, format_tool_output_truncation, tool_output_limits};
use crate::tool_summary::summarize_tool_use;
use crate::wrapping::{RtOptions, word_wrap_lines};
#[cfg(test)]
use maestro_presentation::components::tool_result::preview_lines as tool_preview_lines;
use maestro_presentation::components::{
    composer::Composer,
    tool_result::{ToolPhase, ToolResult},
};
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::SystemTime;
use unicode_width::UnicodeWidthStr;

fn conversation_theme() -> maestro_ui::UiTheme {
    conversation_theme_for(&crate::themes::current_theme())
}

fn conversation_theme_for(theme: &crate::themes::Theme) -> maestro_ui::UiTheme {
    theme.ui_theme()
}

fn semantic_color(key: &str, fallback: Color) -> Color {
    semantic_color_for_theme(&crate::themes::current_theme(), key, fallback)
}

fn semantic_color_for_theme(theme: &crate::themes::Theme, key: &str, fallback: Color) -> Color {
    theme.get_color(key).unwrap_or(fallback)
}

fn themed_chrome(key: &str, fallback: (u8, u8, u8)) -> Color {
    semantic_color(key, brand_color(fallback))
}

fn brand_color(rgb: (u8, u8, u8)) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

fn brand_violet() -> Color {
    themed_chrome("accent", DEIXIC_ACCENT)
}

fn brand_border() -> Color {
    themed_chrome("border", DEIXIC_BORDER)
}

fn brand_muted() -> Color {
    themed_chrome("muted", DEIXIC_MUTED)
}

fn brand_text() -> Color {
    themed_chrome("text", DEIXIC_TEXT)
}

/// Shared block layout for both message measurement and painting.
fn parse_markdown_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    crate::markdown::render_markdown_with_width(text, Some(width.saturating_sub(2).max(1))).lines
}

/// Format a timestamp for display (HH:MM)
fn format_timestamp(time: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();
    // Convert to local time (simplified - just use UTC offset approximation)
    // For proper timezone support, would need chrono crate
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    format!("{hours:02}:{minutes:02}")
}

fn tool_phase(status: ToolCallStatus) -> ToolPhase {
    match status {
        ToolCallStatus::Pending => ToolPhase::Pending,
        ToolCallStatus::Running => ToolPhase::Running,
        ToolCallStatus::Completed => ToolPhase::Completed,
        ToolCallStatus::Failed => ToolPhase::Failed,
        ToolCallStatus::Cancelled => ToolPhase::Cancelled,
        ToolCallStatus::Blocked => ToolPhase::Blocked,
    }
}

fn tool_result_lines(
    tc: &crate::state::ToolCallState,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let summary = if tc.status == ToolCallStatus::Completed {
        summarize_tool_use(&tc.tool, &tc.args)
    } else {
        crate::tool_summary::summarize_tool_intent(&tc.tool, &tc.args)
    };
    let arguments = get_tool_args_preview(&tc.tool, &tc.args, width.saturating_sub(20) as usize);
    let clamp = clamp_tool_output(&tc.output, tool_output_limits());
    let banner = format_tool_output_truncation(&clamp);
    ToolResult {
        phase: tool_phase(tc.status),
        summary: &summary,
        arguments: &arguments,
        output: &clamp.text,
        expanded,
        detail: &format!("· {} #{}", tc.tool, tc.call_id),
        truncation: banner.as_deref(),
        theme: conversation_theme(),
    }
    .lines(width)
}

fn focus_turn_is_collapsed(
    message: &Message,
    focus_view: bool,
    expanded_focus_turns: &HashSet<String>,
) -> bool {
    focus_view
        && !message.tool_calls.is_empty()
        && !expanded_focus_turns.contains(message.id.as_str())
}

fn focus_turn_summary(message: &Message, selected: bool) -> Line<'static> {
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut running = 0usize;
    let mut pending = 0usize;
    let mut cancelled = 0usize;
    let mut blocked = 0usize;

    for tool_call in &message.tool_calls {
        match tool_call.status {
            ToolCallStatus::Completed => completed += 1,
            ToolCallStatus::Failed => failed += 1,
            ToolCallStatus::Running => running += 1,
            ToolCallStatus::Pending => pending += 1,
            ToolCallStatus::Cancelled => cancelled += 1,
            ToolCallStatus::Blocked => blocked += 1,
        }
    }

    let (bullet, color) = if failed > 0 {
        ("●", semantic_color("error", Color::Red))
    } else if blocked > 0 {
        ("●", semantic_color("accent", Color::Magenta))
    } else if running > 0 {
        ("●", semantic_color("accent", Color::Cyan))
    } else if pending > 0 || cancelled > 0 {
        ("○", semantic_color("warning", Color::Yellow))
    } else {
        ("●", semantic_color("success", Color::Green))
    };

    let mut parts = vec![maestro_ui::localization::format(
        if message.tool_calls.len() == 1 {
            "{0} tool"
        } else {
            "{0} tools"
        },
        &[message.tool_calls.len().to_string()],
    )];
    for (count, label) in [
        (completed, maestro_ui::localization::tr("completed")),
        (failed, maestro_ui::localization::tr("failed")),
        (running, maestro_ui::localization::tr("running")),
        (pending, maestro_ui::localization::tr("pending")),
        (cancelled, maestro_ui::localization::tr("cancelled")),
        (blocked, maestro_ui::localization::tr("blocked")),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }

    let mut spans = vec![
        Span::styled(
            if selected { "› " } else { "  " },
            Style::default().fg(semantic_color("accent", Color::Cyan)),
        ),
        Span::styled(
            bullet,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            parts.join(" · "),
            Style::default()
                .fg(semantic_color("text", Color::White))
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(tool_call) = message
        .tool_calls
        .iter()
        .rev()
        .find(|tool_call| tool_call.status == ToolCallStatus::Running)
    {
        spans.push(Span::styled(
            maestro_ui::localization::tr(" · Live: "),
            Style::default().fg(semantic_color("muted", Color::DarkGray)),
        ));
        spans.push(Span::styled(
            summarize_tool_use(&tool_call.tool, &tool_call.args),
            Style::default().fg(semantic_color("accent", Color::Cyan)),
        ));
    }
    spans.push(Span::styled(
        "  [+]",
        Style::default().fg(semantic_color("muted", Color::DarkGray)),
    ));
    let line = Line::from(spans);
    if selected {
        line.style(Style::default().bg(semantic_color("user_message_bg", Color::DarkGray)))
    } else {
        line
    }
}

/// Check if a message should be rendered
/// Skip empty assistant messages (no content AND no tool calls)
pub fn should_render_message(message: &Message) -> bool {
    if message.is_compaction_boundary() {
        return true;
    }

    // User messages always render
    if message.role == MessageRole::User {
        return true;
    }

    // Assistant messages: render if they have content, thinking, tool calls, OR are streaming
    !message.content.is_empty()
        || !message.thinking.is_empty()
        || !message.tool_calls.is_empty()
        || message.streaming
}

/// Calculate the height needed to render a message
pub fn calculate_message_height(
    message: &Message,
    width: u16,
    expanded_tools: &HashSet<String>,
    compact_tool_outputs: bool,
    focus_view: bool,
    expanded_focus_turns: &HashSet<String>,
) -> u16 {
    if !should_render_message(message) {
        return 0;
    }

    let mut height: u16 = 0;
    let content_width = width.saturating_sub(4).max(1) as usize;

    // Empty line before message (separator)
    height += 1;

    if message.is_compaction_boundary() {
        height += 1;
        if !message.content.is_empty() {
            let md_lines = parse_markdown_lines(&message.content, content_width);
            let wrap_opts = RtOptions::new(content_width)
                .initial_indent(Line::from("  "))
                .subsequent_indent(Line::from("  "));
            let wrapped_lines = word_wrap_lines(&md_lines, wrap_opts);
            height += wrapped_lines.len() as u16;
        }
        return height;
    }

    // Header line with role and timestamp
    height += 1;

    // Thinking previews occupy one terminal row per source line.
    if !message.thinking.is_empty() {
        let count = message.thinking.lines().count();
        height += 1 + if message.thinking_expanded { count } else { 0 } as u16;
    }

    // Content lines (with word wrapping)
    if !message.content.is_empty() {
        let md_lines = parse_markdown_lines(&message.content, content_width);
        let wrap_opts = RtOptions::new(content_width)
            .initial_indent(Line::from("  "))
            .subsequent_indent(Line::from("  "));
        let wrapped_lines = word_wrap_lines(&md_lines, wrap_opts);
        height += wrapped_lines.len() as u16;
    }

    if focus_turn_is_collapsed(message, focus_view, expanded_focus_turns) {
        return height + 1;
    }

    // Tool calls
    for (tool_index, tc) in message.tool_calls.iter().enumerate() {
        let expanded = if compact_tool_outputs {
            expanded_tools.contains(&tc.call_id)
        } else {
            !expanded_tools.contains(&tc.call_id)
        };
        height += tool_result_lines(tc, expanded, width)
            .len()
            .min(u16::MAX as usize) as u16;

        // Separate tools, but let the next message supply the turn spacing.
        if tool_index + 1 < message.tool_calls.len() {
            height += 1;
        }
    }

    height
}

/// A stateless widget for rendering a single chat message.
///
/// Renders a complete message including:
/// - Role header with timestamp (User/Assistant)
/// - Optional thinking block (collapsible)
/// - Message content with markdown parsing
/// - Tool calls (collapsible) with status indicators
///
/// # Widget Trait
///
/// Implements `ratatui::widgets::Widget` to render directly to a buffer. The widget
/// consumes itself (builder pattern) to allow method chaining.
///
/// # Usage
///
/// ```rust,ignore
/// let widget = MessageWidget::new(&message)
///     .with_expanded_tools(&expanded_set);
/// frame.render_widget(widget, area);
/// ```
///
/// The widget will skip rendering entirely if `should_render_message()` returns false
/// (e.g., empty assistant messages with no tool calls).
pub struct MessageWidget<'a> {
    message: &'a Message,
    expanded_tools: Option<&'a HashSet<String>>,
    compact_tool_outputs: bool,
    focus_view: bool,
    expanded_focus_turns: Option<&'a HashSet<String>>,
    selected_focus_turn: Option<&'a str>,
    continuation: bool,
    timestamps: bool,
}

impl<'a> MessageWidget<'a> {
    #[must_use]
    pub fn new(message: &'a Message) -> Self {
        Self {
            message,
            expanded_tools: None,
            compact_tool_outputs: false,
            focus_view: false,
            expanded_focus_turns: None,
            selected_focus_turn: None,
            continuation: false,
            timestamps: false,
        }
    }

    #[must_use]
    pub fn with_continuation(mut self, continuation: bool) -> Self {
        self.continuation = continuation;
        self
    }

    #[must_use]
    pub fn with_timestamps(mut self, timestamps: bool) -> Self {
        self.timestamps = timestamps;
        self
    }

    #[must_use]
    pub fn with_expanded_tools(mut self, expanded: &'a HashSet<String>) -> Self {
        self.expanded_tools = Some(expanded);
        self
    }

    #[must_use]
    pub fn with_compact_tool_outputs(mut self, compact: bool) -> Self {
        self.compact_tool_outputs = compact;
        self
    }

    #[must_use]
    pub fn with_focus_view(mut self, enabled: bool, expanded_turns: &'a HashSet<String>) -> Self {
        self.focus_view = enabled;
        self.expanded_focus_turns = Some(expanded_turns);
        self
    }

    #[must_use]
    pub fn with_selected_focus_turn(mut self, selected: Option<&'a str>) -> Self {
        self.selected_focus_turn = selected;
        self
    }
}

impl Widget for MessageWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        if !should_render_message(self.message) {
            return;
        }

        let mut y = area.y;
        let max_y = area.y + area.height;

        // Blank line separator before message
        if y < max_y {
            y += 1;
        }

        if self.message.is_compaction_boundary() {
            let timestamp = format_timestamp(self.message.timestamp);
            let boundary = Line::from(vec![
                Span::styled(
                    "  ✻ ",
                    Style::default()
                        .fg(semantic_color("muted", Color::DarkGray))
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    maestro_ui::localization::tr("Conversation compacted"),
                    Style::default()
                        .fg(semantic_color("muted", Color::DarkGray))
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  {timestamp}"),
                    Style::default()
                        .fg(semantic_color("muted", Color::DarkGray))
                        .add_modifier(Modifier::DIM),
                ),
            ]);
            Paragraph::new(boundary).render(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                buf,
            );
            y += 1;

            if y < max_y && !self.message.content.is_empty() {
                let content_width = area.width.saturating_sub(4).max(1) as usize;
                let md_lines = parse_markdown_lines(&self.message.content, content_width);
                let wrap_opts = RtOptions::new(area.width.saturating_sub(4).max(1) as usize)
                    .initial_indent(Line::from("  "))
                    .subsequent_indent(Line::from("  "));
                let wrapped_lines = word_wrap_lines(&md_lines, wrap_opts);
                let available = max_y.saturating_sub(y) as usize;
                Paragraph::new(
                    wrapped_lines
                        .into_iter()
                        .take(available)
                        .collect::<Vec<_>>(),
                )
                .wrap(Wrap { trim: false })
                .render(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: max_y.saturating_sub(y),
                    },
                    buf,
                );
            }
            return;
        }

        // Continuations share the turn heading.
        if y < max_y && !self.continuation {
            let mut header_spans: Vec<Span<'static>> = Vec::new();

            match self.message.role {
                MessageRole::User => {
                    let (label, color) = if self.message.kind == MessageKind::SideQuestion {
                        ("BTW", brand_muted())
                    } else {
                        (maestro_ui::localization::tr("You"), brand_text())
                    };
                    header_spans.push(Span::styled(
                        "› ",
                        Style::default()
                            .fg(color)
                            .add_modifier(Modifier::BOLD | Modifier::DIM),
                    ));
                    header_spans.push(Span::styled(
                        label,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ));
                }
                MessageRole::Assistant => {
                    let (prefix, label, color) = match self.message.kind {
                        MessageKind::System => (
                            "• ",
                            maestro_ui::localization::tr("System"),
                            semantic_color("warning", Color::Yellow),
                        ),
                        MessageKind::SideAnswer => ("• ", "Dex (side)", brand_muted()),
                        _ => ("• ", "Dex", brand_violet()),
                    };
                    header_spans.push(Span::styled(
                        prefix,
                        Style::default().fg(color).add_modifier(Modifier::DIM),
                    ));
                    header_spans.push(Span::styled(
                        label,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ));
                }
            }

            if self.timestamps {
                header_spans.push(Span::styled(
                    format!("  {}", format_timestamp(self.message.timestamp)),
                    Style::default().fg(brand_muted()),
                ));
            }

            let header = Line::from(header_spans);
            let header_para = Paragraph::new(header);
            header_para.render(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                buf,
            );
            y += 1;
        }

        // Render thinking content (collapsible)
        if y < max_y && !self.message.thinking.is_empty() {
            let expanded = self.message.thinking_expanded;
            let toggle_hint = if expanded { "[-]" } else { "[+]" };

            // Thinking header with collapse/expand indicator
            let thinking_header = Line::from(vec![
                Span::styled(
                    "  │ ",
                    Style::default().fg(semantic_color("muted", Color::DarkGray)),
                ),
                Span::styled("◆ ", Style::default().fg(brand_violet())),
                Span::styled(
                    maestro_ui::localization::tr("Thinking"),
                    Style::default().fg(brand_violet()),
                ),
                Span::raw(" "),
                Span::styled(
                    toggle_hint,
                    Style::default()
                        .fg(semantic_color("muted", Color::DarkGray))
                        .add_modifier(Modifier::DIM),
                ),
            ]);
            Paragraph::new(thinking_header).render(
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
            y += 1;

            if expanded {
                // Show all thinking content with gutter
                for line in self.message.thinking.lines() {
                    if y >= max_y {
                        break;
                    }
                    let max_len = area.width.saturating_sub(6) as usize;
                    let truncated = truncate_location(line, max_len);
                    let content = Line::from(vec![
                        Span::styled(
                            "  │ ",
                            Style::default().fg(semantic_color("muted", Color::DarkGray)),
                        ),
                        Span::styled(
                            truncated,
                            Style::default()
                                .fg(semantic_color("muted", Color::DarkGray))
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]);
                    Paragraph::new(content).render(
                        Rect {
                            x: area.x,
                            y,
                            width: area.width,
                            height: 1,
                        },
                        buf,
                    );
                    y += 1;
                }
            }
        }

        // Render content with markdown styling and proper word wrapping
        if y < max_y && !self.message.content.is_empty() {
            let content_width = area.width.saturating_sub(4).max(1) as usize;

            // Parse markdown into styled lines
            let md_lines = parse_markdown_lines(&self.message.content, content_width);

            // Word wrap all lines with indent
            let wrap_opts = RtOptions::new(content_width)
                .initial_indent(Line::from("  "))
                .subsequent_indent(Line::from("  "));

            let wrapped_lines = word_wrap_lines(&md_lines, wrap_opts);

            // Render each wrapped line
            for line in wrapped_lines {
                if y >= max_y {
                    break;
                }
                Paragraph::new(line).render(
                    Rect {
                        x: area.x,
                        y,
                        width: area.width,
                        height: 1,
                    },
                    buf,
                );
                y += 1;
            }
        }

        let empty_focus_turns = HashSet::new();
        let expanded_focus_turns = self.expanded_focus_turns.unwrap_or(&empty_focus_turns);
        if y < max_y && focus_turn_is_collapsed(self.message, self.focus_view, expanded_focus_turns)
        {
            Paragraph::new(focus_turn_summary(
                self.message,
                self.selected_focus_turn == Some(self.message.id.as_str()),
            ))
            .render(
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
            return;
        }

        // Render tool calls in Codex style
        for (tool_index, tool_call) in self.message.tool_calls.iter().enumerate() {
            if y >= max_y {
                break;
            }

            let expanded = self
                .expanded_tools
                .is_some_and(|s| s.contains(&tool_call.call_id));
            let expanded = if self.compact_tool_outputs {
                expanded
            } else {
                !expanded
            };

            let lines = tool_result_lines(tool_call, expanded, area.width);
            let height = (lines.len().min(u16::MAX as usize) as u16).min(max_y - y);
            Paragraph::new(lines).render(Rect { y, height, ..area }, buf);
            y += height;

            // Only separate tools within this message.
            if y < max_y && tool_index + 1 < self.message.tool_calls.len() {
                y += 1;
            }
        }
    }
}

/// Get a preview of tool arguments based on tool type
fn get_tool_args_preview(tool: &str, args: &serde_json::Value, max_len: usize) -> String {
    let preview = match tool.to_lowercase().as_str() {
        "bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "read" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "write" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "edit" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "grep" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "task" => args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "webfetch" | "websearch" => args
            .get("url")
            .or_else(|| args.get("query"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => {
            // Generic: show first string value
            args.as_object()
                .and_then(|obj| {
                    obj.values()
                        .find_map(|v| v.as_str())
                        .map(std::string::ToString::to_string)
                })
                .unwrap_or_default()
        }
    };

    if preview.width() <= max_len {
        return preview;
    }
    let suffix = ".".repeat(max_len.min(3));
    let budget = max_len.saturating_sub(suffix.len());
    let mut clipped = String::new();
    let mut width = 0;
    let span = Span::raw(preview);
    for grapheme in span.styled_graphemes(Style::default()) {
        let next = grapheme.symbol.width();
        if width + next > budget {
            break;
        }
        clipped.push_str(grapheme.symbol);
        width += next;
    }
    clipped + &suffix
}

/// A stateless widget for rendering a single tool call.
///
/// Displays tool execution details with status indicator, tool name, and optional
/// output. Supports expand/collapse for detailed view.
///
/// # Status Indicators
///
/// - `?` yellow: Pending
/// - `*` blue: Running
/// - `+` green: Completed
/// - `!` red: Failed
/// - `x` yellow: Cancelled
///
/// # Usage
///
/// ```rust,ignore
/// let widget = ToolCallWidget::new("bash", ToolCallStatus::Completed, output, true);
/// frame.render_widget(widget, area);
/// ```
pub struct ToolCallWidget<'a> {
    tool: &'a str,
    status: ToolCallStatus,
    output: &'a str,
    expanded: bool,
}

impl<'a> ToolCallWidget<'a> {
    #[must_use]
    pub fn new(tool: &'a str, status: ToolCallStatus, output: &'a str, expanded: bool) -> Self {
        Self {
            tool,
            status,
            output,
            expanded,
        }
    }
}

impl Widget for ToolCallWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        ToolResult {
            phase: tool_phase(self.status),
            summary: self.tool,
            arguments: "",
            output: self.output,
            expanded: self.expanded,
            detail: "",
            truncation: None,
            theme: conversation_theme(),
        }
        .render(area, buf);
    }
}

/// Format elapsed seconds into compact form (like Codex TUI)
fn fmt_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return maestro_ui::localization::format(
            "{0}m {1}s",
            &[(minutes).to_string(), format!("{:02}", seconds)],
        );
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    maestro_ui::localization::format(
        "{0}h {1}m {2}s",
        &[
            (hours).to_string(),
            format!("{:02}", minutes),
            format!("{:02}", seconds),
        ],
    )
}

/// A stateless widget for rendering the chat input box.
///
/// Displays a bordered input area with:
/// - In-box `>` prompt (no placeholder copy)
/// - Text wrapping for multi-line input
/// - Busy indicator with shimmer animation
/// - Elapsed time display during agent processing
/// - Optional thinking header from agent
///
/// # Cursor Position
///
/// The widget provides `cursor_pos()` to calculate where the terminal cursor should
/// be positioned. This accounts for:
/// - Text wrapping within the input area
/// - Unicode display width (not byte length)
/// - Border offset
///
/// # Usage
///
/// ```rust,ignore
/// let widget = ChatInputWidget::new(
///     &state.textarea,
///     ChatInputWidgetOptions {
///         busy,
///         pending_input_preview,
///         ghost_text: None,
///     },
/// );
/// frame.render_widget(widget, area);
///
/// if let Some((x, y)) = widget.cursor_pos(area) {
///     frame.set_cursor_position((x, y));
/// }
/// ```
pub struct ChatInputWidget<'a> {
    textarea: &'a TextArea,
    busy: bool,
    pending_input_preview: Option<PendingInputPreview>,
    ghost_text: Option<String>,
    runtime_footer: Option<String>,
    interaction_mode: Option<InteractionMode>,
}

#[derive(Debug)]
pub struct ChatInputWidgetOptions {
    pub busy: bool,
    pub pending_input_preview: Option<PendingInputPreview>,
    /// Ghost-text suffix shown dimmed after the cursor (slash-command
    /// inline completion). Only pass it when the cursor is at end of input.
    pub ghost_text: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct QueueSummary {
    pub total: usize,
}

impl QueueSummary {
    pub fn new(total: usize) -> Self {
        Self { total }
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

#[derive(Debug, Clone, Default)]
pub struct PendingInputPreview {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub follow_up_edit_binding_label: String,
}

const PREVIEW_LINE_LIMIT: usize = 3;
const INTERRUPT_STEERING_DESCRIPTION: &str = "Ctrl+C interrupt and apply now";
const EDIT_LAST_QUEUED_FOLLOW_UP_DESCRIPTION: &str = "edit queued follow-ups";

pub(crate) use maestro_presentation::components::composer::PROMPT_WIDTH as COMPOSER_PROMPT_WIDTH;

/// Usable editor width inside the composer (borders + in-box prompt).
#[must_use]
pub(crate) fn composer_editor_width(area_width: u16) -> u16 {
    area_width
        .saturating_sub(2)
        .saturating_sub(COMPOSER_PROMPT_WIDTH)
        .max(1)
}

/// Short label for composer/status chrome.
///
/// Uses the catalog `name` for the current model (`openai-codex/gpt-5.6` →
/// `GPT-5.6`). Unknown ids fall back to the last path segment so the footer
/// is never `openai-codex/gpt-5.6 via openai-codex`.
#[must_use]
fn chrome_model_label(model: &str) -> String {
    let label = if let Some(info) = crate::model_catalog::find_model(model) {
        if !info.name.is_empty() {
            info.name
        } else {
            model.to_string()
        }
    } else {
        match model.rsplit_once('/') {
            Some((_, rest)) if !rest.is_empty() => rest.to_string(),
            _ => model.to_string(),
        }
    };
    if crate::local_models::is_local_model_route(model) {
        format!("{label} · Local")
    } else {
        label
    }
}

impl PendingInputPreview {
    #[must_use]
    pub fn from_state(state: &crate::state::AppState) -> Option<Self> {
        let preview = Self {
            steering: state.queued_steering_preview.clone(),
            follow_up: state.queued_follow_up_preview.clone(),
            steering_mode: state.steering_mode,
            follow_up_mode: state.follow_up_mode,
            follow_up_edit_binding_label: state.queued_follow_up_edit_binding_label.clone(),
        };
        (!preview.is_empty()).then_some(preview)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steering.is_empty() && self.follow_up.is_empty()
    }

    #[must_use]
    pub fn desired_height(&self, width: u16) -> u16 {
        self.build_lines(width).len() as u16
    }

    fn build_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.is_empty() || width < 4 {
            return Vec::new();
        }

        let dim_style = Style::default()
            .fg(semantic_color("muted", Color::DarkGray))
            .add_modifier(Modifier::DIM);
        let mut lines = Vec::new();

        if !self.steering.is_empty() {
            Self::push_section(
                &mut lines,
                width,
                &Self::section_title(
                    maestro_ui::localization::tr("Queued steering after next tool boundary"),
                    self.steering.len(),
                    self.steering_mode,
                ),
                &self.steering,
                false,
            );
            lines.extend(word_wrap_lines(
                &[Line::from(Span::styled(
                    format!(
                        "  {}",
                        maestro_ui::localization::tr(INTERRUPT_STEERING_DESCRIPTION)
                    ),
                    dim_style,
                ))],
                RtOptions::new(width as usize)
                    .subsequent_indent(Line::from(Span::styled("    ", dim_style))),
            ));
        }

        if !self.follow_up.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            Self::push_section(
                &mut lines,
                width,
                &Self::section_title(
                    maestro_ui::localization::tr("Queued follow-ups after turn end"),
                    self.follow_up.len(),
                    self.follow_up_mode,
                ),
                &self.follow_up,
                true,
            );
            lines.extend(word_wrap_lines(
                &[Line::from(Span::styled(
                    format!(
                        "  {} {}",
                        self.follow_up_edit_binding_label,
                        maestro_ui::localization::tr(EDIT_LAST_QUEUED_FOLLOW_UP_DESCRIPTION)
                    ),
                    dim_style,
                ))],
                RtOptions::new(width as usize)
                    .subsequent_indent(Line::from(Span::styled("    ", dim_style))),
            ));
        }

        lines
    }

    fn section_title(base: &str, count: usize, mode: QueueMode) -> String {
        if count <= 1 {
            return base.to_string();
        }
        let note = match mode {
            QueueMode::All => {
                maestro_ui::localization::format("next batch: all {0}", &[(count).to_string()])
            }
            QueueMode::One => {
                maestro_ui::localization::format("next batch: 1 of {0}", &[(count).to_string()])
            }
        };
        format!("{base} ({note})")
    }

    fn push_section(
        lines: &mut Vec<Line<'static>>,
        width: u16,
        title: &str,
        entries: &[String],
        italic: bool,
    ) {
        let dim_style = Style::default()
            .fg(semantic_color("muted", Color::DarkGray))
            .add_modifier(Modifier::DIM);
        let header = Line::from(vec![
            Span::styled("• ", dim_style),
            Span::styled(title.to_string(), dim_style),
        ]);
        lines.extend(word_wrap_lines(
            &[header],
            RtOptions::new(width as usize)
                .subsequent_indent(Line::from(Span::styled("  ", dim_style))),
        ));

        for entry in entries {
            let entry_style = if italic {
                dim_style.add_modifier(Modifier::ITALIC)
            } else {
                dim_style
            };
            let source_lines: Vec<Line<'static>> = entry
                .lines()
                .map(|line| Line::from(Span::styled(line.to_string(), entry_style)))
                .collect();
            let wrapped = word_wrap_lines(
                &source_lines,
                RtOptions::new(width as usize)
                    .initial_indent(Line::from(Span::styled("  ↳ ", dim_style)))
                    .subsequent_indent(Line::from(Span::styled("    ", dim_style))),
            );
            let wrapped_len = wrapped.len();
            lines.extend(wrapped.into_iter().take(PREVIEW_LINE_LIMIT));
            if wrapped_len > PREVIEW_LINE_LIMIT {
                lines.push(Line::from(Span::styled("    …", entry_style)));
            }
        }
    }

    #[cfg(test)]
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let lines = self.build_lines(area.width);
        if lines.is_empty() {
            return;
        }
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
}

impl<'a> ChatInputWidget<'a> {
    pub fn new(textarea: &'a TextArea, options: ChatInputWidgetOptions) -> Self {
        Self {
            textarea,
            busy: options.busy,
            pending_input_preview: options.pending_input_preview,
            ghost_text: options.ghost_text,
            runtime_footer: None,
            interaction_mode: None,
        }
    }

    /// Attach model context and the active interaction mode to the composer.
    #[must_use]
    pub fn with_runtime_footer(
        mut self,
        model: Option<&str>,
        thinking_level: ThinkingLevel,
        interaction_mode: InteractionMode,
    ) -> Self {
        let mut context = model
            .map(chrome_model_label)
            .unwrap_or_else(|| super::deixic_logo::PRODUCT_TITLE.to_owned());
        if thinking_level != ThinkingLevel::Off {
            context.push_str(&format!(
                " ({})",
                maestro_ui::localization::tr(&thinking_level.label().to_ascii_lowercase())
            ));
        }
        self.runtime_footer = Some(context);
        self.interaction_mode = Some(interaction_mode);
        self
    }

    fn footer_for_width(&self, width: u16) -> Option<String> {
        let mode = self.interaction_mode?;
        let mut footer = maestro_ui::localization::format(
            "Mode: {0}",
            &[(match mode {
                InteractionMode::Normal => maestro_ui::localization::tr("Act"),
                InteractionMode::AlwaysApprove => maestro_ui::localization::tr("Auto-approve"),
            })
            .to_string()],
        );
        let available = usize::from(width.saturating_sub(2));
        if let Some(model) = &self.runtime_footer {
            let candidate = format!("{footer} · {model}");
            if candidate.width() <= available {
                footer = candidate;
            }
        }
        Some(footer)
    }

    /// Calculate the on-screen cursor position within the input area.
    ///
    /// Returns `(x, y)` coordinates where the terminal cursor should be placed,
    /// accounting for:
    /// - Border offset (1 cell on each side)
    /// - Text wrapping
    /// - Unicode display width
    ///
    /// Returns `None` if:
    /// - Area is too small to render
    /// - Cursor is outside visible area (scrolled out of view)
    #[must_use]
    pub fn cursor_pos(&self, input_area: Rect) -> Option<(u16, u16)> {
        let queued = self
            .pending_input_preview
            .as_ref()
            .map_or_else(Vec::new, |preview| {
                preview.build_lines(input_area.width.saturating_sub(2))
            });
        let footer = self.footer_for_width(input_area.width);
        self.shared(&queued, footer.as_deref())
            .cursor_pos(input_area)
    }

    fn shared<'b>(&'b self, queued: &'b [Line<'static>], footer: Option<&'b str>) -> Composer<'b> {
        Composer {
            editor: self.textarea,
            queued,
            busy: self.busy,
            footer,
            completion: self.ghost_text.as_deref(),
            theme: conversation_theme(),
        }
    }
}

impl Widget for ChatInputWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let queued = self
            .pending_input_preview
            .as_ref()
            .map_or_else(Vec::new, |preview| {
                preview.build_lines(area.width.saturating_sub(2))
            });
        let footer = self.footer_for_width(area.width);
        self.shared(&queued, footer.as_deref()).render(area, buf);
    }
}

const MIN_TOTAL_INPUT_HEIGHT: u16 = 4;
const MAX_VISIBLE_INPUT_LINES: u16 = 6;
const MIN_MESSAGES_HEIGHT: u16 = 3;

/// Calculate dynamic chat input height based on wrapped lines.
///
/// The height includes borders. It grows with content up to a cap, and
/// always leaves at least a small message viewport.
pub(crate) fn calculate_input_height(state: &crate::state::AppState, area: Rect) -> u16 {
    let status_height = u16::from(!state.zen_mode);

    // If space is tight, fall back to minimum.
    let available_after_status = area.height.saturating_sub(status_height);
    if available_after_status <= MIN_TOTAL_INPUT_HEIGHT {
        return available_after_status.max(1);
    }

    let inner_width = area.width.saturating_sub(2).max(1);
    let editor_width = composer_editor_width(area.width);
    let preview_height = PendingInputPreview::from_state(state)
        .map_or(0, |preview| preview.desired_height(inner_width));
    let desired_inner_lines = state
        .textarea
        .desired_height(editor_width)
        .max(1)
        .saturating_add(preview_height);

    let max_total_for_input = available_after_status
        .saturating_sub(MIN_MESSAGES_HEIGHT)
        .max(MIN_TOTAL_INPUT_HEIGHT);
    let max_inner_for_input = max_total_for_input.saturating_sub(3).max(1);

    let visible_inner = desired_inner_lines
        .min(MAX_VISIBLE_INPUT_LINES)
        .min(max_inner_for_input)
        .max(1);

    visible_inner
        .saturating_add(3)
        .max(MIN_TOTAL_INPUT_HEIGHT)
        .min(available_after_status)
}

/// Token usage summary for display
#[derive(Default, Clone, Copy)]
pub struct UsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl UsageSummary {
    /// Format tokens for display (e.g., "1.2k" for 1200)
    fn format_tokens(count: u64) -> String {
        if count >= 1000 {
            format!("{:.1}k", count as f64 / 1000.0)
        } else {
            count.to_string()
        }
    }
}

/// Grok-inspired one-line status for the active turn.
///
/// Keeps transient execution state out of the composer border so activity,
/// elapsed time, queue pressure, token usage, and controls scan as one row.
pub struct TurnStatusWidget<'a> {
    activity: Option<&'a str>,
    elapsed_secs: u64,
    queue: QueueSummary,
    tokens: Option<u64>,
    can_queue_follow_up: bool,
    animations: bool,
}

impl<'a> TurnStatusWidget<'a> {
    #[must_use]
    pub fn new(
        activity: Option<&'a str>,
        elapsed_secs: u64,
        queue: QueueSummary,
        tokens: Option<u64>,
        can_queue_follow_up: bool,
    ) -> Self {
        Self {
            activity,
            elapsed_secs,
            queue,
            tokens,
            can_queue_follow_up,
            animations: true,
        }
    }

    #[must_use]
    pub fn animations(mut self, animations: bool) -> Self {
        self.animations = animations;
        self
    }
}

impl Widget for TurnStatusWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let activity = self
            .activity
            .unwrap_or(maestro_ui::localization::tr("Working"));
        let activity = if activity.chars().count() > 30 {
            format!("{}…", activity.chars().take(29).collect::<String>())
        } else {
            activity.to_owned()
        };
        let dim = Style::default().fg(semantic_color("muted", Color::DarkGray));
        let mut spans = vec![Span::styled(
            "◐ ",
            Style::default().fg(Color::Rgb(
                crate::shimmer::DEIXIC_VIOLET.0,
                crate::shimmer::DEIXIC_VIOLET.1,
                crate::shimmer::DEIXIC_VIOLET.2,
            )),
        )];
        if self.animations {
            spans.extend(shimmer_spans(&activity));
        } else {
            spans.push(Span::styled(
                activity,
                Style::default().fg(crate::themes::current_ui_theme().text),
            ));
        }
        spans.push(Span::styled(
            format!("  ·  {}", fmt_elapsed_compact(self.elapsed_secs)),
            dim,
        ));

        if area.width >= 48 && !self.queue.is_empty() {
            spans.push(Span::styled(
                maestro_ui::localization::format(
                    "  ·  {0} queued",
                    &[(self.queue.total).to_string()],
                ),
                Style::default().fg(semantic_color("warning", Color::Yellow)),
            ));
        }
        if area.width >= 64 {
            if let Some(tokens) = self.tokens.filter(|tokens| *tokens > 0) {
                spans.push(Span::styled(
                    maestro_ui::localization::format(
                        "  ·  {0} tok",
                        &[UsageSummary::format_tokens(tokens)],
                    ),
                    dim,
                ));
            }
        }
        if area.width >= 84 && self.can_queue_follow_up {
            spans.push(Span::styled(
                maestro_ui::localization::tr("  ·  Tab queue"),
                dim,
            ));
        }
        // Cancellation hints live in the contextual footer.

        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}

pub use maestro_presentation::components::session_header::SessionHeaderWidget;
#[cfg(test)]
use maestro_presentation::components::session_header::format_context_usage;
use maestro_presentation::components::session_header::{
    format_session_location, truncate_location,
};

/// A stateless widget for rendering the bottom status bar.
///
/// Displays:
/// - Left side: Model name, provider, working directory, git branch
/// - Right side: Token usage (input/output), terminal size
///
/// Hidden in zen mode.
///
/// # Usage
///
/// ```rust,ignore
/// let widget = StatusBarWidget::new(
///     Some("claude-opus-4"),
///     Some("anthropic"),
///     Some("/path/to/project"),
///     Some("main"),
/// ).with_usage(usage_summary);
/// frame.render_widget(widget, area);
/// ```
pub struct StatusBarWidget<'a> {
    model: Option<&'a str>,
    provider: Option<&'a str>,
    cwd: Option<&'a str>,
    git_branch: Option<&'a str>,
    usage: UsageSummary,
    /// Number of active hooks (None = hooks disabled)
    hook_count: Option<usize>,
    queue_badge: Option<&'a str>,
    approval_mode: Option<ApprovalMode>,
    thinking_level: Option<ThinkingLevel>,
    boost_status: crate::model_dynamics::BoostStatus,
    mcp_connected: usize,
    mcp_tool_count: usize,
    mcp_failed: usize,
    alert_count: usize,
    sandbox_policy: Option<&'a str>,
    workspace_trusted: bool,
    pending_approvals: usize,
    shortcut_hints: bool,
    input_busy: bool,
    input_has_text: bool,
    paste_note: Option<&'a str>,
    goal_badge: Option<&'a str>,
    worker_badge: Option<&'a str>,
    footer_style: crate::commands::FooterStyle,
    /// Pending `/attach` paths for the next prompt.
    attach_count: usize,
}

impl<'a> StatusBarWidget<'a> {
    #[must_use]
    pub fn new(
        model: Option<&'a str>,
        provider: Option<&'a str>,
        cwd: Option<&'a str>,
        git_branch: Option<&'a str>,
    ) -> Self {
        Self {
            model,
            provider,
            cwd,
            git_branch,
            usage: UsageSummary::default(),
            hook_count: None,
            queue_badge: None,
            approval_mode: None,
            thinking_level: None,
            boost_status: crate::model_dynamics::BoostStatus::Idle,
            mcp_connected: 0,
            mcp_tool_count: 0,
            mcp_failed: 0,
            alert_count: 0,
            sandbox_policy: None,
            workspace_trusted: false,
            pending_approvals: 0,
            shortcut_hints: false,
            input_busy: false,
            input_has_text: false,
            paste_note: None,
            goal_badge: None,
            worker_badge: None,
            footer_style: crate::commands::FooterStyle::default(),
            attach_count: 0,
        }
    }

    #[must_use]
    pub fn with_boost_status(mut self, status: crate::model_dynamics::BoostStatus) -> Self {
        self.boost_status = status;
        self
    }

    #[must_use]
    pub fn with_worker_badge(mut self, badge: Option<&'a str>) -> Self {
        self.worker_badge = badge;
        self
    }

    #[must_use]
    pub fn with_goal_badge(mut self, badge: Option<&'a str>) -> Self {
        self.goal_badge = badge;
        self
    }

    #[must_use]
    pub fn with_footer_style(mut self, style: crate::commands::FooterStyle) -> Self {
        self.footer_style = style;
        self
    }

    #[must_use]
    pub fn with_attach_count(mut self, count: usize) -> Self {
        self.attach_count = count;
        self
    }

    #[must_use]
    pub fn with_usage(mut self, usage: UsageSummary) -> Self {
        self.usage = usage;
        self
    }

    /// Set hook count. The status bar shows it only when at least one hook is loaded.
    #[must_use]
    pub fn with_hooks(mut self, count: Option<usize>) -> Self {
        self.hook_count = count;
        self
    }

    #[must_use]
    pub fn with_queue_badge(mut self, badge: Option<&'a str>) -> Self {
        self.queue_badge = badge;
        self
    }

    #[must_use]
    pub fn with_approval_mode(mut self, approval_mode: ApprovalMode) -> Self {
        self.approval_mode = Some(approval_mode);
        self
    }

    #[must_use]
    pub fn with_thinking_level(mut self, thinking_level: ThinkingLevel) -> Self {
        self.thinking_level = Some(thinking_level);
        self
    }

    #[must_use]
    pub fn with_mcp_status(mut self, connected: usize, tool_count: usize, failed: usize) -> Self {
        self.mcp_connected = connected;
        self.mcp_tool_count = tool_count;
        self.mcp_failed = failed;
        self
    }

    #[must_use]
    pub fn with_alert_count(mut self, alert_count: usize) -> Self {
        self.alert_count = alert_count;
        self
    }

    #[must_use]
    pub fn with_sandbox_policy(mut self, sandbox_policy: Option<&'a str>) -> Self {
        self.sandbox_policy = sandbox_policy;
        self
    }

    #[must_use]
    pub fn with_workspace_trusted(mut self, workspace_trusted: bool) -> Self {
        self.workspace_trusted = workspace_trusted;
        self
    }

    #[must_use]
    pub fn with_pending_approvals(mut self, pending_approvals: usize) -> Self {
        self.pending_approvals = pending_approvals;
        self
    }

    #[must_use]
    pub fn with_shortcut_hints(mut self) -> Self {
        self.shortcut_hints = true;
        self
    }

    #[must_use]
    pub fn with_input_context(mut self, busy: bool, has_text: bool) -> Self {
        self.input_busy = busy;
        self.input_has_text = has_text;
        self
    }

    /// Note about folded pasted content, e.g. "pasted 42 lines (folded)".
    #[must_use]
    pub fn with_paste_note(mut self, note: Option<&'a str>) -> Self {
        self.paste_note = note;
        self
    }
}

impl Widget for StatusBarWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.render_with_theme(area, buf, crate::themes::current_ui_theme());
    }
}

impl StatusBarWidget<'_> {
    fn render_with_theme(self, area: Rect, buf: &mut Buffer, theme: maestro_ui::UiTheme) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        use crate::commands::FooterStyle;

        // `/footer clear` leaves the status strip empty (zen still hides the row).
        if matches!(self.footer_style, FooterStyle::Clear) {
            return;
        }

        buf.set_style(area, theme.text_style());

        let show_chrome = matches!(self.footer_style, FooterStyle::Rich);
        let show_location = matches!(self.footer_style, FooterStyle::Rich) && !self.shortcut_hints;
        let show_shortcuts = self.shortcut_hints && matches!(self.footer_style, FooterStyle::Rich);
        // History: only alerts / pending approvals. Solo: model + goal + key badges.
        let history_only = matches!(self.footer_style, FooterStyle::History);
        let solo = matches!(self.footer_style, FooterStyle::Solo);

        let mut spans = Vec::new();

        let boost_label = match self.boost_status {
            crate::model_dynamics::BoostStatus::Idle => None,
            crate::model_dynamics::BoostStatus::Suggested => Some("✦ /boost"),
            crate::model_dynamics::BoostStatus::Pending => Some("✦ queued"),
            crate::model_dynamics::BoostStatus::Active => Some("✦"),
        };
        if let Some(label) = boost_label {
            spans.push(Span::styled(label, Style::default().fg(theme.focus)));
        }
        if show_shortcuts {
            if boost_label.is_some() {
                spans.push(Span::styled(" · ", Style::default().fg(theme.muted)));
            }
            let hints = if self.pending_approvals > 0 {
                maestro_ui::localization::tr("Respond to the approval above")
            } else if self.input_busy {
                if self.input_has_text {
                    maestro_ui::localization::tr("Enter steer · Alt+Enter queue · Ctrl+C cancel")
                } else {
                    maestro_ui::localization::tr("Ctrl+C cancel")
                }
            } else if self.input_has_text {
                if area.width >= 60 {
                    maestro_ui::localization::tr("Enter send · Shift+Enter newline")
                } else {
                    maestro_ui::localization::tr("Enter send")
                }
            } else {
                maestro_ui::localization::tr("? search · / commands")
            };
            spans.push(Span::styled(hints, Style::default().fg(theme.muted)));
        }

        // Model info (rich + solo)
        if !history_only {
            if let Some(model) = self.model {
                if !spans.is_empty() {
                    spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
                }
                spans.push(Span::styled(
                    chrome_model_label(model),
                    Style::default().fg(theme.muted),
                ));
                if let Some(provider) = self.provider {
                    if show_chrome {
                        spans.push(Span::raw(maestro_ui::localization::tr(" via ")));
                        spans.push(Span::styled(provider, Style::default().fg(theme.muted)));
                    }
                }
            }
        }

        // Goal badge (rich + solo)
        if !history_only {
            if let Some(goal) = self.goal_badge {
                if !spans.is_empty() {
                    spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
                }
                spans.push(Span::styled(
                    goal.to_string(),
                    Style::default().fg(theme.attention),
                ));
            }
        }

        if !history_only {
            if let Some(worker) = self.worker_badge {
                if !spans.is_empty() {
                    spans.push(Span::raw("  ·  "));
                }
                spans.push(Span::styled(
                    worker.to_owned(),
                    Style::default().fg(theme.focus),
                ));
            }
        }

        // Pending attachments (rich + solo)
        if !history_only && self.attach_count > 0 {
            if !spans.is_empty() {
                spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
            }
            spans.push(Span::styled(
                maestro_ui::localization::format("attach:{0}", &[(self.attach_count).to_string()]),
                Style::default().fg(theme.focus),
            ));
        }

        // Working directory + git (rich only)
        if show_location {
            if !spans.is_empty() && self.cwd.is_some() {
                spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
            }
            if let Some(cwd) = self.cwd {
                let short_cwd = cwd.rsplit('/').next().unwrap_or(cwd);
                spans.push(Span::styled(short_cwd, Style::default().fg(theme.muted)));

                if let Some(branch) = self.git_branch {
                    spans.push(Span::styled("  ", Style::default().fg(theme.border)));
                    spans.push(Span::styled(branch, Style::default().fg(theme.focus)));
                }
            }
        }

        // Hook status (rich only)
        if show_chrome {
            if let Some(count) = self.hook_count.filter(|count| *count > 0) {
                if !spans.is_empty() {
                    spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
                }
                spans.push(Span::styled(
                    maestro_ui::localization::format("hooks:{0}", &[(count).to_string()]),
                    Style::default().fg(theme.muted),
                ));
            }
        }

        // Folded-paste note (rich + solo)
        if !history_only {
            if let Some(note) = self.paste_note {
                if !spans.is_empty() {
                    spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
                }
                spans.push(Span::styled(
                    note.to_string(),
                    Style::default().fg(theme.muted),
                ));
            }
        }

        // History mode: surface only alert / approval urgency on the left.
        if history_only {
            if self.pending_approvals > 0 {
                spans.push(Span::styled(
                    maestro_ui::localization::format(
                        "approvals:{0}",
                        &[(self.pending_approvals).to_string()],
                    ),
                    Style::default().fg(theme.attention),
                ));
            }
            if self.alert_count > 0 {
                if !spans.is_empty() {
                    spans.push(Span::styled("  ·  ", Style::default().fg(theme.border)));
                }
                spans.push(Span::styled(
                    maestro_ui::localization::format(
                        "alerts:{0}",
                        &[(self.alert_count).to_string()],
                    ),
                    Style::default().fg(theme.error),
                ));
            }
        }

        let line = Line::from(spans);
        let left_width = line.width() as u16;
        let para = Paragraph::new(line).style(Style::default().fg(theme.muted));
        para.render(area, buf);

        // Build right-side info (usage + terminal size)
        let mut usage_text: Option<String> = None;

        // Token usage (rich only; solo keeps model/goal focus)
        let total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        if show_chrome && total_tokens > 0 {
            usage_text = Some(format!(
                "↑{} ↓{}",
                UsageSummary::format_tokens(self.usage.input_tokens),
                UsageSummary::format_tokens(self.usage.output_tokens)
            ));
        }

        let badges = if history_only || self.shortcut_hints {
            None
        } else {
            self.approval_mode.map(|mode| {
                build_runtime_badges(RuntimeBadgeParams {
                    approval_mode: mode,
                    thinking_level: self.thinking_level.unwrap_or(ThinkingLevel::Off),
                    mcp_connected: self.mcp_connected,
                    mcp_tool_count: self.mcp_tool_count,
                    mcp_failed: self.mcp_failed,
                    alert_count: self.alert_count,
                    sandbox_policy: self.sandbox_policy.map(str::to_owned),
                    workspace_trusted: self.workspace_trusted,
                    pending_approvals: self.pending_approvals,
                })
            })
        };
        let core_badges = if self.shortcut_hints {
            // Keep actionable conditions visible; environment diagnostics remain
            // available through /about and the detailed footer surfaces.
            let mut notices = Vec::new();
            if self.approval_mode == Some(ApprovalMode::Yolo) {
                notices.push(maestro_ui::localization::tr("Always approve").to_string());
            }
            if self.pending_approvals > 0 {
                notices.push(maestro_ui::localization::format(
                    "{0} pending",
                    &[(self.pending_approvals).to_string()],
                ));
            }
            if self.alert_count > 0 {
                notices.push(maestro_ui::localization::format(
                    "{0} alerts",
                    &[(self.alert_count).to_string()],
                ));
            }
            if self.mcp_failed > 0 {
                notices.push(maestro_ui::localization::format(
                    "{0} connections failed",
                    &[(self.mcp_failed).to_string()],
                ));
            }
            (!notices.is_empty()).then(|| notices.join(" · "))
        } else {
            badges
                .as_ref()
                .and_then(|b| (!b.core.is_empty()).then(|| b.core.join(" ")))
        };
        // Solo: core badges only. Rich: core + env.
        let env_badges = if solo {
            None
        } else {
            badges
                .as_ref()
                .and_then(|b| (!b.env.is_empty()).then(|| b.env.join(" ")))
        };

        let queue_text = if show_chrome {
            self.queue_badge.map(|badge| badge.to_string())
        } else {
            None
        };

        let term_text = if show_chrome && !self.shortcut_hints && area.width >= 120 {
            crate::terminal::size()
                .ok()
                .map(|(cols, rows)| format!("{cols}x{rows}"))
        } else {
            None
        };

        let available_width = area.width.saturating_sub(left_width + 1);

        // Drop right-side segments until the right column fits. Never paint
        // over the left content (bugbash: "gpt-4queue" / "…fiqueue").
        let mut right = RightStatusParts {
            usage: usage_text.as_deref(),
            core: core_badges.as_deref(),
            env: env_badges.as_deref(),
            queue: queue_text.as_deref(),
            term: term_text.as_deref(),
        };
        let mut right_text = right.render();

        // Prefer keeping queue mode labels over env/core/usage when tight.
        for drop in [
            DropRight::Env,
            DropRight::Core,
            DropRight::Usage,
            DropRight::Term,
            DropRight::Queue,
        ] {
            if right_text.is_empty()
                || UnicodeWidthStr::width(right_text.as_str()) <= available_width as usize
            {
                break;
            }
            right.omit(drop);
            right_text = right.render();
        }

        // Render right-side info only when it fits without overlapping left.
        if !right_text.is_empty()
            && UnicodeWidthStr::width(right_text.as_str()) <= available_width as usize
        {
            let right_line = Line::from(Span::styled(right_text, Style::default().fg(theme.muted)));
            let right_width = right_line.width() as u16;
            let right_x = area.right().saturating_sub(right_width);
            buf.set_line(right_x, area.y, &right_line, right_width);
        }
    }
}

#[derive(Clone, Copy)]
enum DropRight {
    Env,
    Core,
    Usage,
    Term,
    Queue,
}

#[derive(Clone, Copy)]
struct RightStatusParts<'a> {
    usage: Option<&'a str>,
    core: Option<&'a str>,
    env: Option<&'a str>,
    queue: Option<&'a str>,
    term: Option<&'a str>,
}

impl RightStatusParts<'_> {
    fn omit(&mut self, which: DropRight) {
        match which {
            DropRight::Env => self.env = None,
            DropRight::Core => self.core = None,
            DropRight::Usage => self.usage = None,
            DropRight::Term => self.term = None,
            DropRight::Queue => self.queue = None,
        }
    }

    fn render(self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(usage) = self.usage {
            parts.push(usage);
        }
        if let Some(core) = self.core.filter(|c| !c.is_empty()) {
            parts.push(core);
        }
        if let Some(env) = self.env.filter(|e| !e.is_empty()) {
            parts.push(env);
        }
        if let Some(queue) = self.queue {
            parts.push(queue);
        }
        if let Some(term) = self.term {
            parts.push(term);
        }
        parts.join(" ")
    }
}

/// The main chat view widget containing messages, input, and status bar.
///
/// This is the top-level widget for the chat interface. It implements a virtual
/// scrolling system to efficiently render large message histories.
///
/// # Layout
///
/// ```text
/// ┌─────────────────────────┐
/// │  Messages (scrollable)  │
/// │                         │
/// │  • Maestro              │
/// │  I can help with that   │
/// │                         │
/// │  › You                  │
/// │  Please do              │
/// │                         │
/// ├─────────────────────────┤
/// │ > Type message_         │ <- Input box (auto-growing)
/// ├─────────────────────────┤
/// │ opus-4 | project (main) │ <- Status bar (1 row, hidden in zen mode)
/// └─────────────────────────┘
/// ```
///
/// # Virtual Scrolling
///
/// The message list uses virtual scrolling:
/// 1. Pre-calculate heights for all messages
/// 2. Determine which messages are visible based on scroll offset
/// 3. Render only visible messages
/// 4. Draw scrollbar if content exceeds viewport
///
/// This allows smooth scrolling through thousands of messages.
///
/// # Usage
///
/// ```rust,ignore
/// let view = ChatView::new(&app_state);
/// frame.render_widget(view, frame.area());
/// ```
pub struct ChatView<'a> {
    tool_toggle_binding: Option<crate::key_hints::KeyBinding>,
    timestamps: bool,
    state: &'a crate::state::AppState,
    sandbox_policy: Option<&'a str>,
    workspace_trusted: bool,
    pending_approvals: usize,
    footer_style: crate::commands::FooterStyle,
    goal_badge: Option<&'a str>,
    worker_badge: Option<&'a str>,
    attach_count: usize,
    dex_state: Option<super::dex_companion::DexCompanionState>,
    dex_frame: u64,
    dex_look: crate::dex_delight::DexLook,
    dex_notice: Option<&'a str>,
    dex_suggestion: Option<&'a str>,
    dex_tip: Option<&'a str>,
    dex_personality: super::dex_companion::DexPersonality,
    animations: bool,
}

impl<'a> ChatView<'a> {
    pub fn new(state: &'a crate::state::AppState) -> Self {
        Self {
            tool_toggle_binding: None,
            timestamps: false,
            state,
            sandbox_policy: None,
            workspace_trusted: false,
            pending_approvals: 0,
            footer_style: crate::commands::FooterStyle::default(),
            goal_badge: None,
            worker_badge: None,
            attach_count: 0,
            dex_state: None,
            dex_frame: 0,
            dex_look: Default::default(),
            dex_notice: None,
            dex_suggestion: None,
            dex_tip: None,
            dex_personality: super::dex_companion::DexPersonality::Standard,
            animations: false,
        }
    }

    pub fn with_dex_delight(
        mut self,
        look: crate::dex_delight::DexLook,
        notice: Option<&'a str>,
        suggestion: Option<&'a str>,
        tip: Option<&'a str>,
    ) -> Self {
        self.dex_look = look;
        self.dex_notice = notice;
        self.dex_suggestion = suggestion;
        self.dex_tip = tip;
        self
    }

    /// Presentation preferences do not affect execution or activity classification.
    #[must_use]
    pub fn with_timestamps(mut self, timestamps: bool) -> Self {
        self.timestamps = timestamps;
        self
    }

    #[must_use]
    pub fn with_dex_presentation(
        mut self,
        personality: super::dex_companion::DexPersonality,
        animations: bool,
    ) -> Self {
        self.dex_personality = personality;
        self.animations = animations;
        self
    }

    #[must_use]
    pub fn with_dex_frame(mut self, frame: u64) -> Self {
        self.dex_frame = frame;
        self
    }

    #[must_use]
    pub fn with_tool_toggle_binding(mut self, binding: crate::key_hints::KeyBinding) -> Self {
        self.tool_toggle_binding = Some(binding);
        self
    }

    fn tool_shortcut_hint(&self, width: usize) -> Option<String> {
        if self.state.busy
            || !self.state.input().is_empty()
            || self.pending_approvals > 0
            || self.state.focus_view
            || self.dex_personality == super::dex_companion::DexPersonality::Quiet
        {
            return None;
        }
        let binding = self.tool_toggle_binding?;
        // Match the existing toggle action, which targets the latest tool call.
        let call = self
            .state
            .messages
            .iter()
            .rev()
            .find_map(|message| message.tool_calls.last())?;
        let action = if self.state.is_tool_call_expanded(&call.call_id) {
            maestro_ui::localization::tr("collapse")
        } else {
            maestro_ui::localization::tr("expand")
        };
        let toggle = format!("{} {action}", binding.display());
        let details = crate::key_hints::ctrl(crossterm::event::KeyCode::Char('e')).display();
        let detailed = format!(
            "{toggle} · {details} {}",
            maestro_ui::localization::tr("details")
        );
        [detailed, toggle]
            .into_iter()
            .find(|hint| hint.width() <= width)
    }

    /// Use the application's observed lifecycle; never infer completion from prose.
    #[must_use]
    pub fn with_dex_state(mut self, state: super::dex_companion::DexCompanionState) -> Self {
        self.dex_state = Some(state);
        self
    }

    #[must_use]
    pub fn with_runtime_status(
        mut self,
        sandbox_policy: Option<&'a str>,
        workspace_trusted: bool,
        pending_approvals: usize,
    ) -> Self {
        self.sandbox_policy = sandbox_policy;
        self.workspace_trusted = workspace_trusted;
        self.pending_approvals = pending_approvals;
        self
    }

    #[must_use]
    pub fn with_footer_style(mut self, style: crate::commands::FooterStyle) -> Self {
        self.footer_style = style;
        self
    }

    #[must_use]
    pub fn with_worker_badge(mut self, badge: Option<&'a str>) -> Self {
        self.worker_badge = badge;
        self
    }

    #[must_use]
    pub fn with_goal_badge(mut self, badge: Option<&'a str>) -> Self {
        self.goal_badge = badge;
        self
    }

    #[must_use]
    pub fn with_attach_count(mut self, count: usize) -> Self {
        self.attach_count = count;
        self
    }
}

impl Widget for ChatView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let theme = crate::themes::current_theme();
        buf.set_style(area, theme.canvas_style());
        if area.height < 5 || area.width < 10 {
            return;
        }

        let status_height = u16::from(!self.state.zen_mode);
        let header_height = 2 * u16::from(
            !self.state.zen_mode && self.state.messages.iter().any(should_render_message),
        );
        let show_face = self.dex_state.is_some()
            && self.dex_personality != super::dex_companion::DexPersonality::Quiet
            && area.width >= 48
            && area.height >= 16;
        let turn_status_height = u16::from(
            self.state.busy || (self.dex_state.is_some() && !self.state.messages.is_empty()),
        ) * if show_face || self.dex_notice.is_some() {
            2
        } else {
            1
        };
        let input_height = calculate_input_height(self.state, area);
        let chunks = Layout::vertical([
            Constraint::Length(header_height), // Session location + context
            Constraint::Min(0),                // Messages
            Constraint::Length(turn_status_height), // Active turn status
            Constraint::Length(input_height),  // Input (auto-grow)
            Constraint::Length(status_height), // Status (hidden in zen mode)
        ])
        .split(area);

        let context_used = self
            .state
            .messages
            .iter()
            .rev()
            .find_map(|message| message.usage.as_ref())
            .map(|usage| {
                usage
                    .input_tokens
                    .saturating_add(usage.output_tokens)
                    .saturating_add(usage.cache_read_tokens)
            });

        if header_height > 0 {
            SessionHeaderWidget::new(self.state.cwd.as_deref(), self.state.git_branch.as_deref())
                .with_context(context_used, self.state.context_window)
                .theme(theme.canvas_style().bg.map(|_| theme.ui_theme()))
                .render(chunks[0], buf);
        }

        // Render messages
        self.render_messages(chunks[1], buf, theme.canvas_style());

        let mut activity_area = chunks[2];
        if show_face && activity_area.height > 0 {
            if let Some(state) = self.dex_state {
                super::dex_companion::DexCompanion::new(state)
                    .theme(theme.canvas_style().bg.map(|_| theme.ui_theme()))
                    .personality(self.dex_personality)
                    .animations(self.animations)
                    .frame(self.dex_frame)
                    .look(self.dex_look)
                    .render_face(
                        Rect::new(activity_area.x, activity_area.y, 6, activity_area.height),
                        buf,
                    );
                activity_area = Rect::new(
                    activity_area.x + 7,
                    activity_area.bottom() - 1,
                    activity_area.width.saturating_sub(7),
                    1,
                );
            }
        }
        let suggestion_tip = if self.dex_tip.is_some() && self.dex_suggestion.is_some() {
            Some(maestro_ui::localization::tr(
                "→ accept suggestion · /dex suggestions-off",
            ))
        } else {
            None
        };
        if let Some(notice) = self.dex_notice.or(suggestion_tip) {
            if chunks[2].height >= 2 {
                Paragraph::new(notice)
                    .style(Style::default().fg(crate::themes::current_ui_theme().muted))
                    .render(
                        Rect::new(
                            chunks[2].x + 7,
                            chunks[2].y,
                            chunks[2].width.saturating_sub(7),
                            1,
                        ),
                        buf,
                    );
            }
        }
        if self.state.busy && self.pending_approvals == 0 {
            let activity = super::activity::active_tool_label(self.state);
            let activity = match (self.dex_state, activity.as_deref()) {
                (Some(state), Some(tool)) => format!("Dex {} · {tool}", state.label()),
                (Some(state), None) => {
                    maestro_ui::localization::format("Dex {0}", &[(state.label()).to_string()])
                }
                (None, tool) => tool
                    .unwrap_or(maestro_ui::localization::tr("Working"))
                    .to_owned(),
            };
            let activity =
                if self.dex_personality == super::dex_companion::DexPersonality::Expressive {
                    format!("{activity} · {}", self.dex_look.activity.phrase())
                } else {
                    activity
                };
            TurnStatusWidget::new(
                Some(&activity),
                self.state.elapsed_busy_secs(),
                QueueSummary::new(self.state.queued_prompt_count),
                context_used,
                self.state.can_queue_follow_up_shortcut(),
            )
            .animations(
                self.animations
                    && self.dex_personality != super::dex_companion::DexPersonality::Quiet,
            )
            .render(activity_area, buf);
        } else if let Some(state) = self.dex_state {
            let mut line = super::dex_companion::DexCompanion::new(state)
                .theme(theme.canvas_style().bg.map(|_| theme.ui_theme()))
                .personality(self.dex_personality)
                .status_line();
            let available = usize::from(activity_area.width).saturating_sub(line.width() + 3);
            if let Some(hint) = self.tool_shortcut_hint(available) {
                line.spans.push(Span::styled(
                    format!(" · {hint}"),
                    Style::default().fg(crate::themes::current_ui_theme().muted),
                ));
            }
            Paragraph::new(line).render(activity_area, buf);
        }

        // Render input
        let mut input_widget = ChatInputWidget::new(
            &self.state.textarea,
            ChatInputWidgetOptions {
                busy: self.state.busy,
                pending_input_preview: PendingInputPreview::from_state(self.state),
                ghost_text: if self.state.cursor() == self.state.input().len() {
                    self.state
                        .ghost_completion
                        .clone()
                        .or_else(|| self.dex_suggestion.map(str::to_owned))
                } else {
                    None
                },
            },
        );
        let startup_summary_visible = chunks[1].width >= 44
            && chunks[1].height >= 5
            && !self.state.messages.iter().any(should_render_message);
        input_widget = input_widget.with_runtime_footer(
            self.state.model.as_deref(),
            self.state.thinking_level,
            self.state.interaction_mode,
        );
        if startup_summary_visible {
            input_widget.runtime_footer = None;
        }

        input_widget.render(chunks[3], buf);

        // Render status bar (unless zen mode)
        if !self.state.zen_mode {
            let queue_badge = {
                if self.state.queued_prompt_count > 0 {
                    Some(format!(
                        "queue:{} · f={} s={}",
                        self.state.queued_prompt_count,
                        self.state.follow_up_mode.short_label(),
                        self.state.steering_mode.short_label()
                    ))
                } else {
                    None
                }
            };

            let alert_count = self.state.unseen_alerts;

            let paste_note = self.state.textarea.folded_paste_lines().map(|lines| {
                maestro_ui::localization::format(
                    "pasted {0} lines (folded)",
                    &[(lines).to_string()],
                )
            });

            // Model + mode already sit on the composer border. Passing them
            // here reprints `GPT-5.6 via openai-codex` on the next row.
            let status_widget = StatusBarWidget::new(
                None,
                None,
                self.state.cwd.as_deref(),
                self.state.git_branch.as_deref(),
            )
            .with_queue_badge(queue_badge.as_deref())
            .with_approval_mode(self.state.approval_mode)
            .with_thinking_level(self.state.thinking_level)
            .with_boost_status(self.state.boost_status)
            .with_mcp_status(
                self.state.mcp_connected,
                self.state.mcp_tool_count,
                self.state.mcp_failed,
            )
            .with_alert_count(alert_count)
            .with_sandbox_policy(self.sandbox_policy)
            .with_workspace_trusted(self.workspace_trusted)
            .with_pending_approvals(self.pending_approvals)
            .with_paste_note(paste_note.as_deref())
            .with_goal_badge(self.goal_badge)
            .with_worker_badge(self.worker_badge)
            .with_footer_style(self.footer_style)
            .with_attach_count(self.attach_count)
            .with_shortcut_hints()
            .with_input_context(self.state.busy, !self.state.input().is_empty());
            status_widget.render(chunks[4], buf);
        }
    }
}

fn continues_turn(previous: Option<&&Message>, message: &Message) -> bool {
    previous.is_some_and(|previous| {
        previous.role == MessageRole::Assistant
            && message.role == MessageRole::Assistant
            && previous.kind == MessageKind::Regular
            && message.kind == MessageKind::Regular
    })
}

impl ChatView<'_> {
    fn message_layout_settings_key(&self) -> u64 {
        let mut key = u64::from(self.state.compact_tool_outputs);
        for call_id in &self.state.expanded_tool_calls {
            let mut hasher = DefaultHasher::new();
            call_id.hash(&mut hasher);
            key ^= hasher.finish().rotate_left(1);
        }
        key ^= (self.state.expanded_tool_calls.len() as u64).rotate_left(32);
        key ^= u64::from(self.state.focus_view).rotate_left(3);
        for message_id in &self.state.expanded_focus_turns {
            let mut hasher = DefaultHasher::new();
            message_id.hash(&mut hasher);
            key ^= hasher.finish().rotate_left(7);
        }
        key ^ (self.state.expanded_focus_turns.len() as u64).rotate_left(40)
    }

    fn render_messages(&self, area: Rect, buf: &mut Buffer, canvas: Style) {
        // Filter to only renderable messages
        let renderable_messages: Vec<&Message> = self
            .state
            .messages
            .iter()
            .filter(|m| should_render_message(m))
            .collect();

        if area.height == 0 || renderable_messages.is_empty() {
            let runtime = self
                .state
                .model
                .as_deref()
                .map(chrome_model_label)
                .unwrap_or_else(|| {
                    maestro_ui::localization::tr("Sign in to choose a model").to_string()
                });
            let location = format_session_location(
                self.state.cwd.as_deref(),
                self.state.git_branch.as_deref(),
            );
            if self.dex_personality == super::dex_companion::DexPersonality::Quiet {
                if !self.state.busy {
                    super::welcome::WelcomeScreen::new()
                        .personality(self.dex_personality)
                        .animations(false)
                        .with_session(self.state.session_id.clone())
                        .with_summary(runtime, location)
                        .render(area, buf);
                }
            } else {
                crate::components::deixic_logo::render_welcome_with_theme(
                    area,
                    buf,
                    self.animations,
                    self.state.session_id.as_deref(),
                    !self.state.busy,
                    Some((&runtime, &location)),
                    crate::themes::current_theme()
                        .canvas_style()
                        .bg
                        .map(|_| crate::themes::current_ui_theme()),
                );
            }
            if self.dex_personality != super::dex_companion::DexPersonality::Quiet {
                maestro_presentation::components::dex_companion::render_welcome_portrait_with_theme(
                    area,
                    buf,
                    self.dex_look,
                    self.dex_state
                        .unwrap_or(super::dex_companion::DexCompanionState::Ready),
                    self.animations,
                    crate::themes::current_theme()
                        .canvas_style()
                        .bg
                        .map(|_| crate::themes::current_ui_theme()),
                );
            }
            if area.height >= 7
                && (self.dex_personality == super::dex_companion::DexPersonality::Quiet
                    || super::deixic_logo::welcome_prompt_row(area) + 1 < area.bottom())
            {
                if let Some(text) = self.dex_notice.or(self.dex_tip) {
                    maestro_ui::Notice::new(text)
                        .style(Style::default().fg(crate::themes::current_ui_theme().muted))
                        .render(
                            Rect::new(
                                area.x + 3,
                                if self.dex_personality
                                    == super::dex_companion::DexPersonality::Quiet
                                {
                                    area.bottom().saturating_sub(1)
                                } else {
                                    super::deixic_logo::welcome_prompt_row(area) + 1
                                },
                                area.width.saturating_sub(3),
                                1,
                            ),
                            buf,
                        );
                }
            }
            return;
        }

        let content_area = Rect {
            width: area.width.saturating_sub(1),
            ..area
        };
        let layout = self.state.prepare_message_layout(
            content_area.width,
            self.message_layout_settings_key(),
            &renderable_messages,
            |index| {
                usize::from(calculate_message_height(
                    renderable_messages[index],
                    content_area.width,
                    &self.state.expanded_tool_calls,
                    self.state.compact_tool_outputs,
                    self.state.focus_view,
                    &self.state.expanded_focus_turns,
                ))
                .saturating_sub(usize::from(continues_turn(
                    index
                        .checked_sub(1)
                        .and_then(|i| renderable_messages.get(i)),
                    renderable_messages[index],
                )))
            },
        );
        let total_height = layout.total_height();

        // Clamp scroll_offset to available content
        let max_offset = total_height.saturating_sub(usize::from(area.height));
        let clamped_offset = self.state.scroll_offset.min(max_offset);

        // Window anchored from bottom by scroll_offset
        let window_bottom = total_height.saturating_sub(clamped_offset);
        let window_top = window_bottom.saturating_sub(usize::from(area.height));

        // Find the first message whose bottom exceeds window_top
        let start_idx = layout.first_visible(window_top);

        // Render messages from start_idx forward
        let mut y = area.y;
        let max_y = area.y + area.height;

        let mut message_top = layout.heights()[..start_idx].iter().sum::<usize>();
        for (i, message) in renderable_messages.iter().enumerate().skip(start_idx) {
            if y >= max_y {
                break;
            }

            let skip = window_top.saturating_sub(message_top);
            message_top += layout.heights()[i];
            let full_height = layout.heights()[i].min(usize::from(u16::MAX)) as u16;
            let msg_height = usize::from(full_height)
                .saturating_sub(skip)
                .min(usize::from(max_y.saturating_sub(y))) as u16;
            let msg_area = Rect::new(0, 0, content_area.width, full_height);
            let mut message_buffer = Buffer::empty(msg_area);
            message_buffer.set_style(msg_area, canvas);

            let widget = MessageWidget::new(message)
                .with_continuation(continues_turn(
                    i.checked_sub(1).and_then(|i| renderable_messages.get(i)),
                    message,
                ))
                .with_timestamps(self.timestamps)
                .with_expanded_tools(&self.state.expanded_tool_calls)
                .with_compact_tool_outputs(self.state.compact_tool_outputs)
                .with_focus_view(self.state.focus_view, &self.state.expanded_focus_turns)
                .with_selected_focus_turn(self.state.focus_selected_turn.as_deref());
            widget.render(msg_area, &mut message_buffer);
            for row in 0..msg_height {
                for col in 0..content_area.width {
                    if let (Some(source), Some(target)) = (
                        message_buffer.cell((col, skip as u16 + row)),
                        buf.cell_mut((area.x + col, y + row)),
                    ) {
                        *target = source.clone();
                    }
                }
            }
            y += msg_height;
        }

        // Draw a simple scrollbar on the right
        if total_height > usize::from(area.height) {
            let bar_x = area.x + area.width.saturating_sub(1);
            let view_ratio = f32::from(area.height) / total_height as f32;
            let thumb_height = (f32::from(area.height) * view_ratio)
                .round()
                .clamp(1.0, f32::from(area.height)) as u16;
            let scroll_ratio = window_top as f32 / max_offset.max(1) as f32;
            let thumb_start = (scroll_ratio * f32::from(area.height - thumb_height)).round() as u16;
            for i in 0..area.height {
                let in_thumb = i >= thumb_start && i < thumb_start + thumb_height;
                if let Some(cell) = buf.cell_mut((bar_x, area.y + i)) {
                    cell.set_symbol("│");
                    cell.set_style(Style::default().fg(if in_thumb {
                        brand_muted()
                    } else {
                        brand_border()
                    }));
                }
            }
        }

        // Jump-to-latest indicator
        if self.state.scroll_offset > 0 {
            let hint = maestro_ui::localization::tr("Jump to latest (G)");
            let hx = area
                .x
                .saturating_add(area.width.saturating_sub(hint.len() as u16 + 2));
            buf.set_string(
                hx,
                area.y,
                hint,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn clear_footer_preserves_canvas_with_shared_theme() {
        for theme in crate::components::theme_test::palettes() {
            let area = Rect::new(0, 0, 120, 1);
            let mut buffer = Buffer::empty(area);
            buffer.set_style(area, Style::default().fg(Color::Gray).bg(Color::Black));
            let original = buffer.clone();
            StatusBarWidget::new(None, None, None, None)
                .with_footer_style(crate::commands::FooterStyle::Clear)
                .render_with_theme(area, &mut buffer, theme);
            assert_eq!(buffer, original);
        }
    }

    #[test]
    fn status_bar_uses_shared_theme() {
        for theme in crate::components::theme_test::palettes() {
            for footer in [
                crate::commands::FooterStyle::Rich,
                crate::commands::FooterStyle::Solo,
                crate::commands::FooterStyle::History,
            ] {
                let area = Rect::new(0, 0, 240, 1);
                let mut buffer = Buffer::empty(area);
                StatusBarWidget::new(
                    Some("gpt-4o"),
                    Some("openai"),
                    Some("/workspace"),
                    Some("main"),
                )
                .with_boost_status(crate::model_dynamics::BoostStatus::Suggested)
                .with_footer_style(footer)
                .with_goal_badge(Some("goal"))
                .with_worker_badge(Some("worker"))
                .with_paste_note(Some("pasted 42 lines"))
                .with_hooks(Some(2))
                .with_attach_count(1)
                .with_pending_approvals(1)
                .with_alert_count(1)
                .render_with_theme(area, &mut buffer, theme);
                crate::components::theme_test::assert_palette(&buffer, theme);
            }
        }
    }

    use super::*;

    #[test]
    fn system_message_header_uses_the_theme_warning_color() {
        let mut message = polish_message("system", "Setup needs attention");
        message.kind = MessageKind::System;
        let area = Rect::new(0, 0, 60, 6);
        let mut buffer = Buffer::empty(area);
        MessageWidget::new(&message).render(area, &mut buffer);
        let label = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "S")
            .unwrap();
        assert_eq!(label.fg, crate::themes::current_ui_theme().attention);
    }

    fn polish_message(id: &str, content: &str) -> Message {
        Message {
            id: id.into(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: content.into(),
            thinking: String::new(),
            streaming: false,
            tool_calls: vec![],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        }
    }

    #[test]
    fn tool_shortcut_uses_resolved_binding_and_current_expansion_state() {
        let mut state = crate::state::AppState::new();
        let binding = crate::key_hints::shift(crossterm::event::KeyCode::Left);
        assert!(
            ChatView::new(&state)
                .with_tool_toggle_binding(binding)
                .tool_shortcut_hint(100)
                .is_none()
        );
        let mut message = polish_message("tool", "Done.");
        message.tool_calls.push(crate::state::ToolCallState {
            call_id: "read-1".into(),
            tool: "read".into(),
            args: serde_json::json!({"path":"README.md"}),
            status: ToolCallStatus::Completed,
            output: "Release checklist".into(),
        });
        state.messages.push(message);
        let hint = |state: &crate::state::AppState, width| {
            ChatView::new(state)
                .with_tool_toggle_binding(binding)
                .tool_shortcut_hint(width)
        };
        assert_eq!(
            hint(&state, 100),
            Some(format!(
                "{} expand · {} details",
                binding.display(),
                crate::key_hints::ctrl(crossterm::event::KeyCode::Char('e')).display()
            ))
        );
        let compact = format!("{} expand", binding.display());
        assert_eq!(hint(&state, compact.width()), Some(compact));
        assert!(hint(&state, 3).is_none());
        state.toggle_tool_call("read-1");
        assert!(hint(&state, 100).unwrap().contains("collapse"));
        state.compact_tool_outputs = false;
        assert!(hint(&state, 100).unwrap().contains("expand"));
        state.busy = true;
        assert!(hint(&state, 100).is_none());
        state.busy = false;
        state.focus_view = true;
        assert!(hint(&state, 100).is_none());
        state.focus_view = false;
        assert!(
            ChatView::new(&state)
                .with_tool_toggle_binding(binding)
                .with_runtime_status(None, true, 1)
                .tool_shortcut_hint(100)
                .is_none()
        );
        assert!(
            ChatView::new(&state)
                .with_tool_toggle_binding(binding)
                .with_dex_presentation(super::super::dex_companion::DexPersonality::Quiet, false)
                .tool_shortcut_hint(100)
                .is_none()
        );
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        ChatView::new(&state)
            .with_tool_toggle_binding(binding)
            .with_dex_state(super::super::dex_companion::DexCompanionState::Finished)
            .render(area, &mut buf);
        assert!(
            buffer_lines(&buf, 100, 30)
                .join("\n")
                .contains(&format!("{} expand", binding.display()))
        );
        state.set_input("draft");
        assert!(hint(&state, 100).is_none());
    }

    #[test]
    fn compact_session_header_keeps_a_gap_above_scrolled_transcript() {
        let mut state = crate::state::AppState::new();
        state.cwd = Some("/tmp/release-planner".into());
        state.messages = vec![polish_message(
            "long",
            &"A line of conversation.\n".repeat(40),
        )];
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        ChatView::new(&state).render(area, &mut buf);
        let lines = buffer_lines(&buf, 60, 20);
        assert!(lines[0].contains("release-planner"));
        assert!(lines[1].trim().is_empty());
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("100%") || line.contains('░'))
        );
    }

    #[test]
    fn measured_height_matches_wrapped_content_and_clipped_unicode_previews() {
        for expanded in [false, true] {
            let mut message = polish_message(
                "unicode",
                "A long answer repeated across narrow terminal columns without losing the last line.",
            );
            message.thinking = "界".repeat(60);
            message.thinking_expanded = expanded;
            message.tool_calls.push(crate::state::ToolCallState {
                call_id: "read-1".into(),
                tool: "read".into(),
                args: serde_json::json!({"path":"README.md"}),
                status: ToolCallStatus::Completed,
                output: "x".repeat(180),
            });
            let height = calculate_message_height(
                &message,
                40,
                &HashSet::new(),
                true,
                false,
                &HashSet::new(),
            );
            let area = Rect::new(0, 0, 40, height + 3);
            let mut buf = Buffer::empty(area);
            MessageWidget::new(&message)
                .with_compact_tool_outputs(true)
                .render(area, &mut buf);
            let lines = buffer_lines(&buf, 40, height + 3);
            let occupied = lines
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .unwrap()
                + 1;
            assert_eq!(occupied, usize::from(height), "{}", lines.join("\n"));
        }
    }

    #[test]
    fn message_copy_preserves_canvas_including_blank_cells() {
        let mut state = crate::state::AppState::new();
        state.messages = vec![polish_message("canvas", "Readable message")];
        let area = Rect::new(0, 0, 80, 20);
        let canvas = Style::default()
            .bg(Color::Rgb(238, 232, 224))
            .fg(Color::Rgb(81, 71, 84));
        let mut buf = Buffer::empty(area);
        buf.set_style(area, canvas);
        ChatView::new(&state).render_messages(area, &mut buf, canvas);
        assert!(
            buffer_lines(&buf, 80, 20)
                .join("\n")
                .contains("Readable message")
        );
        for cell in &buf.content {
            assert_eq!(cell.bg, Color::Rgb(238, 232, 224));
        }
        let body_row = buffer_lines(&buf, 80, 20)
            .iter()
            .position(|line| line.contains("Readable message"))
            .unwrap();
        assert_eq!(buf[(0, body_row as u16)].fg, Color::Rgb(81, 71, 84));
    }

    #[test]
    fn adjacent_dex_messages_share_heading_but_not_system_or_side_turns() {
        let mut state = crate::state::AppState::new();
        state.messages = vec![
            polish_message("a", "Read complete."),
            polish_message("b", "Here is the answer."),
        ];
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        ChatView::new(&state).render_messages(area, &mut buf, Style::default());
        let text = buffer_lines(&buf, 80, 20).join("\n");
        assert_eq!(text.matches("Dex").count(), 1);
        assert!(text.contains("Read complete."));
        assert!(text.contains("Here is the answer."));
        // Changing the predecessor must invalidate the following cached height.
        state.messages[0].kind = MessageKind::System;
        let mut buf = Buffer::empty(area);
        ChatView::new(&state).render_messages(area, &mut buf, Style::default());
        let text = buffer_lines(&buf, 80, 20).join("\n");
        assert!(text.contains("System"));
        assert!(text.contains("Dex"));
        state.messages[0].kind = MessageKind::SideAnswer;
        assert!(!continues_turn(
            Some(&&state.messages[0]),
            &state.messages[1]
        ));
    }

    #[test]
    fn long_message_scrolls_to_its_tail_and_back_after_resize() {
        let mut state = crate::state::AppState::new();
        use std::fmt::Write as _;
        let mut content = String::new();
        for i in 1..=40 {
            writeln!(content, "Line {i:02}  ").unwrap();
        }
        state.messages = vec![polish_message("long", &content)];
        for (width, height) in [(80, 10), (40, 11), (100, 13)] {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            ChatView::new(&state).render_messages(area, &mut buf, Style::default());
            let text = buffer_lines(&buf, width, height).join("\n");
            assert!(text.contains("Line 40"), "{text}");
            assert!(!text.contains("Line 01"));
            assert_eq!(buf[(width - 1, height - 1)].fg, brand_muted());
        }
        state.scroll_offset = 20;
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);
        ChatView::new(&state).render_messages(area, &mut buf, Style::default());
        let text = buffer_lines(&buf, 80, 10).join("\n");
        assert!(text.contains("Line 20"), "{text}");
        assert!(!text.contains("Line 40"));
    }

    #[test]
    fn tool_identifiers_and_timestamps_are_details_only() {
        let mut message = polish_message("tool", "");
        message.tool_calls.push(crate::state::ToolCallState {
            call_id: "read-private-id".into(),
            tool: "read".into(),
            args: serde_json::json!({"path":"README.md"}),
            status: ToolCallStatus::Completed,
            output: "Useful preview".into(),
        });
        let area = Rect::new(0, 0, 100, 10);
        for expanded in [false, true] {
            let mut buf = Buffer::empty(area);
            MessageWidget::new(&message)
                .with_compact_tool_outputs(!expanded)
                .with_timestamps(expanded)
                .render(area, &mut buf);
            let text = buffer_lines(&buf, 100, 10).join("\n");
            assert!(text.contains("Read README.md"));
            assert_eq!(text.contains("Useful preview"), expanded);
            assert_eq!(text.contains("read-private-id"), expanded);
            assert_eq!(
                text.contains(&format_timestamp(message.timestamp)),
                expanded
            );
        }
    }

    #[test]
    fn footer_hints_follow_input_and_execution_state() {
        for (busy, typing, approvals, expected) in [
            (false, false, 0, "? search"),
            (false, true, 0, "Shift+Enter newline"),
            (true, false, 0, "Ctrl+C cancel"),
            (true, true, 0, "Enter steer"),
            (true, false, 1, "Respond to the approval"),
        ] {
            let area = Rect::new(0, 0, 80, 1);
            let mut buf = Buffer::empty(area);
            StatusBarWidget::new(None, None, None, None)
                .with_shortcut_hints()
                .with_input_context(busy, typing)
                .with_pending_approvals(approvals)
                .render(area, &mut buf);
            let text = buffer_lines(&buf, 80, 1).join("\n");
            assert!(text.contains(expected), "{text}");
            assert_eq!(text.contains("Enter send"), typing && !busy);
        }
    }

    #[test]
    fn compact_tool_preview_keeps_content_and_number_spacing() {
        let output = "```text\n1\t# Release checklist\n2\t\n3\tRun checks\n```";
        assert_eq!(
            tool_preview_lines(output, false),
            vec!["1  # Release checklist", "3  Run checks"]
        );
        assert_eq!(tool_preview_lines(output, true).len(), 5);
    }

    fn buffer_lines(buf: &Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn queued_preview_keeps_the_editor_visible_in_a_short_composer() {
        let mut textarea = TextArea::new();
        textarea.set_text("Keep this draft");
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: true,
                pending_input_preview: Some(PendingInputPreview {
                    follow_up: vec!["Next task".into(); 5],
                    ..Default::default()
                }),
                ghost_text: None,
            },
        );
        let area = Rect::new(0, 0, 40, 5);
        assert!(
            widget.cursor_pos(area).is_some(),
            "queued previews must leave an editor row"
        );
        let mut buffer = Buffer::empty(area);
        widget.render(area, &mut buffer);
        assert!(
            buffer_lines(&buffer, 40, 5)
                .join("\n")
                .contains("Keep this draft")
        );
    }

    #[test]
    fn tool_argument_preview_does_not_split_unicode() {
        let args = serde_json::json!({"file_path": "界".repeat(30)});
        let preview = get_tool_args_preview("read", &args, 20);
        assert!(preview.width() <= 20);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn pending_input_preview_is_empty_without_items() {
        let preview = PendingInputPreview::default();
        assert_eq!(preview.desired_height(48), 0);
    }

    #[test]
    fn pending_input_preview_renders_sections() {
        let preview = PendingInputPreview {
            steering: vec!["steer first".to_string()],
            follow_up: vec!["follow later".to_string()],
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            follow_up_edit_binding_label: "Shift+Left".to_string(),
        };
        let width = 96;
        let height = preview.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        preview.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("Queued steering after next"));
        assert!(rendered.contains("Queued follow-ups after turn end"));
        assert!(rendered.contains("steer first"));
        assert!(rendered.contains("Ctrl+C interrupt and apply now"));
        assert!(rendered.contains("follow later"));
        assert!(rendered.contains("Shift+Left edit queued follow-ups"));
    }

    #[test]
    fn pending_input_preview_describes_next_batch_for_multiple_items() {
        let preview = PendingInputPreview {
            steering: vec!["steer first".to_string(), "steer second".to_string()],
            follow_up: vec![
                "follow first".to_string(),
                "follow second".to_string(),
                "follow third".to_string(),
            ],
            steering_mode: QueueMode::One,
            follow_up_mode: QueueMode::All,
            follow_up_edit_binding_label: "Alt+Up".to_string(),
        };
        let width = 128;
        let height = preview.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        preview.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("Queued steering after next tool boundary (next batch: 1 of 2)"));
        assert!(rendered.contains("Queued follow-ups after turn end (next batch: all 3)"));
    }

    #[test]
    fn turn_status_shows_active_turn_details_at_wide_widths() {
        let widget = TurnStatusWidget::new(
            Some("Indexing workspace"),
            12,
            QueueSummary::new(2),
            Some(1_200),
            true,
        );
        let width = 120;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));

        widget.render(Rect::new(0, 0, width, 1), &mut buf);

        let rendered = buffer_lines(&buf, width, 1).join("\n");
        assert!(rendered.contains("Indexing workspace"));
        assert!(rendered.contains("12s"));
        assert!(rendered.contains("2 queued"));
        assert!(rendered.contains("1.2k tok"));
        assert!(rendered.contains("Tab queue"));
        assert!(!rendered.contains("Esc interrupt"));
    }

    #[test]
    fn turn_status_keeps_essential_state_at_narrow_widths() {
        let widget =
            TurnStatusWidget::new(Some("Working"), 12, QueueSummary::new(2), Some(1_200), true);
        let width = 46;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));

        widget.render(Rect::new(0, 0, width, 1), &mut buf);

        let rendered = buffer_lines(&buf, width, 1).join("\n");
        assert!(rendered.contains("Working"));
        assert!(rendered.contains("12s"));
        assert!(!rendered.contains("Esc interrupt"));
        assert!(!rendered.contains("queued"));
        assert!(!rendered.contains("tok"));
        assert!(!rendered.contains("Tab queue"));
    }

    #[test]
    fn input_footer_keeps_model_effort_and_mode_next_to_the_composer() {
        let textarea = TextArea::new();
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: false,
                pending_input_preview: None,
                ghost_text: None,
            },
        )
        .with_runtime_footer(
            Some("gpt-5.6"),
            ThinkingLevel::High,
            InteractionMode::AlwaysApprove,
        );
        let width = 96;
        let height = 4;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        widget.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("Mode: Auto-approve · GPT-5.6 (high)"));
    }

    #[test]
    fn input_footer_uses_catalog_name_for_qualified_model_id() {
        let textarea = TextArea::new();
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: false,
                pending_input_preview: None,
                ghost_text: None,
            },
        )
        .with_runtime_footer(
            Some("openai-codex/gpt-5.6"),
            ThinkingLevel::Off,
            InteractionMode::Normal,
        );
        let width = 96;
        let height = 4;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        widget.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("Mode: Act · GPT-5.6"));
        assert!(!rendered.contains("openai-codex/gpt-5.6"));
    }

    #[test]
    fn chat_view_shows_model_once_in_startup_summary() {
        let mut state = crate::state::AppState::default();
        state.model = Some("openai-codex/gpt-5.6".to_string());
        state.provider = Some("openai-codex".to_string());
        let width = 100;
        let height = 16;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        ChatView::new(&state).render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        let catalog_hits = rendered.matches("GPT-5.6").count();
        assert_eq!(catalog_hits, 1, "model must appear once:\n{rendered}");
        assert!(rendered.contains("Mode: Act"));
        assert!(!rendered.contains("via openai-codex"));
        assert!(!rendered.contains("openai-codex/gpt-5.6"));
        assert!(!rendered.contains("Describe what you want to build..."));
    }

    #[test]
    fn composer_mode_remains_visible_at_startup_and_in_conversation() {
        for (mode, label) in [
            (InteractionMode::Normal, "Mode: Act"),
            (InteractionMode::AlwaysApprove, "Mode: Auto-approve"),
        ] {
            for width in [24, 40, 100] {
                for has_messages in [false, true] {
                    let mut state = crate::state::AppState::default();
                    state.model = Some("provider/a-very-long-custom-model-name".into());
                    state.thinking_level = ThinkingLevel::High;
                    state.interaction_mode = mode;
                    if has_messages {
                        state
                            .messages
                            .push(polish_message("user", "Inspect the files"));
                    }
                    let area = Rect::new(0, 0, width, 16);
                    let mut buffer = Buffer::empty(area);
                    ChatView::new(&state).render(area, &mut buffer);
                    let lines = buffer_lines(&buffer, width, 16);
                    assert!(
                        lines.iter().any(|line| line.contains(label)),
                        "mode missing: {lines:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn chrome_model_label_inherits_catalog_name() {
        assert_eq!(chrome_model_label("openai-codex/gpt-5.6"), "GPT-5.6");
        assert_eq!(chrome_model_label("gpt-5.6"), "GPT-5.6");
        assert_eq!(chrome_model_label("ollama/qwen3"), "qwen3 · Local");
        assert_eq!(
            chrome_model_label("openrouter/openai/gpt-4o-mini"),
            "OpenAI: GPT-4o-mini"
        );
        assert_eq!(
            chrome_model_label("unknown-provider/custom-id"),
            "custom-id"
        );
    }

    #[test]
    fn composer_prompt_sits_between_rules_with_no_placeholder() {
        let textarea = TextArea::new();
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: false,
                pending_input_preview: None,
                ghost_text: None,
            },
        );
        let width = 48;
        let height = 4;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        widget.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.starts_with('╭'));
        assert!(rendered.contains('╮'));
        assert!(rendered.contains('╰'));
        assert!(rendered.contains('╯'));
        assert!(!rendered.contains("Describe what you want to build..."));
        let lines = buffer_lines(&buf, width, height);
        assert!(
            lines[0].starts_with('╭'),
            "prompt must not sit on the top border: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("> "),
            "prompt must sit below the separator: {}",
            lines[1]
        );
    }

    #[test]
    fn empty_composer_cursor_sits_on_prompt_space() {
        let textarea = TextArea::new();
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: false,
                pending_input_preview: None,
                ghost_text: None,
            },
        );
        let area = Rect::new(0, 0, 48, 4);
        let pos = widget.cursor_pos(area).expect("cursor");
        // inner.x = 1, prompt "> " occupies cols 1..3, cursor on the space (col 2)
        assert_eq!(pos, (2, 1));
    }

    #[test]
    fn input_renders_ghost_text_completion_after_cursor() {
        let mut textarea = TextArea::new();
        textarea.set_text("/qui");
        textarea.set_cursor(4);
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: false,
                pending_input_preview: None,
                ghost_text: Some("t".to_string()),
            },
        );
        let width = 40;
        let height = 3;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        widget.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("/quit"));
    }

    #[test]
    fn input_without_ghost_text_leaves_trailing_cells_blank() {
        let mut textarea = TextArea::new();
        textarea.set_text("/qui");
        textarea.set_cursor(4);
        let widget = ChatInputWidget::new(
            &textarea,
            ChatInputWidgetOptions {
                busy: false,
                pending_input_preview: None,
                ghost_text: None,
            },
        );
        let width = 40;
        let height = 3;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        widget.render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(!rendered.contains("/quit"));
    }

    #[test]
    fn dex_code_header_stays_readable_in_narrow_terminals() {
        for width in [8, 16, 19, 20, 21, 24, 26, 27, 60, 100] {
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            SessionHeaderWidget::new(Some("/workspace/release-planner"), Some("main"))
                .with_context(Some(9_500), Some(500_000))
                .render(area, &mut buf);
            let row = &buffer_lines(&buf, width, 1)[0];
            assert!(row.starts_with("Dex Code"));
            assert_eq!(row.contains("9.5K / 500K"), width >= 21);
            if (21..27).contains(&width) {
                assert!(!row.contains("│"));
            }
        }
    }

    #[test]
    fn session_header_pairs_location_with_context_pressure() {
        let width = 80;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));

        SessionHeaderWidget::new(Some("/workspace/maestro"), Some("main"))
            .with_context(Some(9_500), Some(500_000))
            .render(Rect::new(0, 0, width, 1), &mut buf);

        let rendered = buffer_lines(&buf, width, 1).join("\n");
        assert!(rendered.contains("Dex"));
        assert!(rendered.contains("maestro  ·  main"));
        assert!(!rendered.contains("/workspace/"));
        assert!(rendered.contains("9.5K / 500K"));
    }

    #[test]
    fn session_header_degrades_without_a_known_context_limit() {
        assert_eq!(
            format_context_usage(Some(12_000), None).as_deref(),
            Some("12K context")
        );
        assert_eq!(format_context_usage(None, Some(500_000)), None);
    }

    #[test]
    fn tool_calls_render_concise_summary_labels() {
        let message = Message {
            id: "msg-1".to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: String::new(),
            thinking: String::new(),
            streaming: false,
            tool_calls: vec![crate::state::ToolCallState {
                call_id: "call-12345678".to_string(),
                tool: "read".to_string(),
                args: serde_json::json!({
                    "file_path": "/Users/jonathanhaas/Documents/Projects/maestro/package.json"
                }),
                status: ToolCallStatus::Completed,
                output: String::new(),
            }],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        };

        let width = 100;
        for compact in [true, false] {
            let expanded = HashSet::new();
            let height = calculate_message_height(
                &message,
                width,
                &expanded,
                compact,
                false,
                &HashSet::new(),
            );
            let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
            MessageWidget::new(&message)
                .with_expanded_tools(&expanded)
                .with_compact_tool_outputs(compact)
                .render(buf.area, &mut buf);
            let rendered = buffer_lines(&buf, width, height).join("\n");
            assert!(rendered.contains("Read package.json"));
            assert_eq!(rendered.contains("· read"), !compact);
            assert_eq!(
                rendered.contains("/Users/jonathanhaas/Documents/Projects/maestro/package.json"),
                !compact
            );
        }
    }

    fn focus_view_message() -> Message {
        Message {
            id: "focus-turn".to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: "Working through the checks.".to_string(),
            thinking: String::new(),
            streaming: true,
            tool_calls: vec![
                crate::state::ToolCallState {
                    call_id: "call-complete".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({ "command": "cargo test" }),
                    status: ToolCallStatus::Completed,
                    output: "finished".to_string(),
                },
                crate::state::ToolCallState {
                    call_id: "call-failed".to_string(),
                    tool: "grep".to_string(),
                    args: serde_json::json!({ "pattern": "needle" }),
                    status: ToolCallStatus::Failed,
                    output: "not found".to_string(),
                },
                crate::state::ToolCallState {
                    call_id: "call-running".to_string(),
                    tool: "read".to_string(),
                    args: serde_json::json!({ "file_path": "/tmp/live.rs" }),
                    status: ToolCallStatus::Running,
                    output: "partial output".to_string(),
                },
            ],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        }
    }

    #[test]
    fn focus_view_collapses_a_tool_bearing_turn_to_one_summary_line() {
        let message = focus_view_message();
        let expanded_turns = HashSet::new();
        let width = 120;
        let height = calculate_message_height(
            &message,
            width,
            &HashSet::new(),
            true,
            true,
            &expanded_turns,
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        MessageWidget::new(&message)
            .with_expanded_tools(&HashSet::new())
            .with_compact_tool_outputs(true)
            .with_focus_view(true, &expanded_turns)
            .render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("3 tools · 1 completed · 1 failed · 1 running"));
        assert!(rendered.contains("Live: Read live.rs"));
        assert!(!rendered.contains("cargo test"));
        assert!(!rendered.contains("finished"));
        assert_eq!(height, 4, "separator, header, content, and one summary row");
    }

    #[test]
    fn expanding_a_focus_turn_restores_existing_tool_rendering() {
        let message = focus_view_message();
        let expanded_turns = HashSet::from([message.id.clone()]);
        let width = 120;
        let height = calculate_message_height(
            &message,
            width,
            &HashSet::new(),
            true,
            true,
            &expanded_turns,
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        MessageWidget::new(&message)
            .with_expanded_tools(&HashSet::new())
            .with_compact_tool_outputs(true)
            .with_focus_view(true, &expanded_turns)
            .render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("Ran cargo test"));
        assert!(!rendered.contains("finished"));
        assert!(rendered.contains("not found"));
        assert!(rendered.contains("Failed · Search for \"needle\""));
    }

    #[test]
    fn assistant_messages_render_deixic_code_header() {
        let message = Message {
            id: "msg-2".to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: "Hello".to_string(),
            thinking: String::new(),
            streaming: false,
            tool_calls: vec![],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        };

        let width = 80;
        let height = calculate_message_height(
            &message,
            width,
            &HashSet::new(),
            true,
            false,
            &HashSet::new(),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        MessageWidget::new(&message).render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("• Dex"));
        assert!(!rendered.contains("Deixic Code"));
        assert!(!rendered.contains("• Composer"));
    }

    #[test]
    fn empty_chat_view_uses_deixic_code_welcome_copy() {
        let state = crate::state::AppState::default();
        let width = 100;
        let height = 20;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        ChatView::new(&state).render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains(crate::components::deixic_logo::PRODUCT_TITLE));
        assert!(rendered.contains("Sign in to choose a model"));
        assert!(!rendered.contains("session 01"));
        assert!(!rendered.contains("Welcome to Deixic Code!"));
        assert!(!rendered.contains("Welcome to Composer! Type a message to get started."));
    }

    #[test]
    fn empty_chat_view_uses_live_runtime_summary() {
        let mut state = crate::state::AppState::default();
        state.session_id = Some("restored-42".to_string());
        state.model = Some("openai-codex/gpt-5.6".to_string());
        state.cwd = Some("/projects/release-checklist".to_string());
        let width = 100;
        let height = 20;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));

        ChatView::new(&state).render(Rect::new(0, 0, width, height), &mut buf);

        let rendered = buffer_lines(&buf, width, height).join("\n");
        assert!(rendered.contains("GPT-5.6"));
        assert!(rendered.contains("release-checklist"));
        assert!(!rendered.contains("session restored-42"));
    }

    fn transcript_layout_message(id: &str, content: &str) -> Message {
        Message {
            id: id.to_string(),
            role: MessageRole::Assistant,
            kind: MessageKind::Regular,
            content: content.to_string(),
            thinking: String::new(),
            streaming: false,
            tool_calls: vec![],
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
            thinking_expanded: false,
        }
    }

    #[test]
    fn transcript_layout_reuses_stable_heights_and_remeasures_streaming_tail() {
        let mut state = crate::state::AppState::default();
        state.messages = vec![
            transcript_layout_message("one", "# First\n\nA stable message."),
            transcript_layout_message("two", "# Second\n\nA streaming message."),
        ];
        let area = Rect::new(0, 0, 80, 24);

        ChatView::new(&state).render(area, &mut Buffer::empty(area));
        let cold_measurements = state.transcript_layout_measurements();
        assert_eq!(cold_measurements, 2);

        ChatView::new(&state).render(area, &mut Buffer::empty(area));
        assert_eq!(state.transcript_layout_measurements(), cold_measurements);

        state.messages[1].content.push_str(" More streamed text.");
        ChatView::new(&state).render(area, &mut Buffer::empty(area));
        assert_eq!(
            state.transcript_layout_measurements(),
            cold_measurements + 1
        );
    }

    #[test]
    fn transcript_layout_width_change_remeasures_every_message() {
        let mut state = crate::state::AppState::default();
        state.messages = vec![
            transcript_layout_message("one", "A stable message."),
            transcript_layout_message("two", "Another stable message."),
        ];

        let wide = Rect::new(0, 0, 80, 24);
        ChatView::new(&state).render(wide, &mut Buffer::empty(wide));
        let cold_measurements = state.transcript_layout_measurements();

        let narrow = Rect::new(0, 0, 60, 24);
        ChatView::new(&state).render(narrow, &mut Buffer::empty(narrow));
        assert_eq!(
            state.transcript_layout_measurements(),
            cold_measurements + 2
        );
    }

    #[test]
    fn transcript_layout_remeasures_when_focus_projection_changes() {
        let mut state = crate::state::AppState::default();
        state.messages = vec![focus_view_message()];
        let area = Rect::new(0, 0, 120, 24);

        ChatView::new(&state).render(area, &mut Buffer::empty(area));
        let initial_measurements = state.transcript_layout_measurements();

        state.focus_view = true;
        ChatView::new(&state).render(area, &mut Buffer::empty(area));
        assert_eq!(
            state.transcript_layout_measurements(),
            initial_measurements + 1
        );

        state.expanded_focus_turns.insert("focus-turn".to_string());
        ChatView::new(&state).render(area, &mut Buffer::empty(area));
        assert_eq!(
            state.transcript_layout_measurements(),
            initial_measurements + 2
        );
    }
}

#[cfg(test)]
mod boost_footer_tests {
    use super::*;
    use crate::model_dynamics::BoostStatus;
    #[test]
    fn boost_footer_stays_compact_at_narrow_widths() {
        for width in [12, 40, 100] {
            for (status, expected) in [
                (BoostStatus::Suggested, "✦ /boost"),
                (BoostStatus::Pending, "✦ queued"),
                (BoostStatus::Active, "✦"),
                (BoostStatus::Idle, ""),
            ] {
                let area = Rect::new(0, 0, width, 1);
                let mut buffer = Buffer::empty(area);
                StatusBarWidget::new(None, None, None, None)
                    .with_boost_status(status)
                    .render(area, &mut buffer);
                let rendered = (0..width)
                    .map(|x| buffer[(x, 0)].symbol())
                    .collect::<String>();
                assert!(rendered.starts_with(expected), "{rendered:?}");
                if status == BoostStatus::Idle {
                    assert!(!rendered.contains('✦'));
                }
            }
        }
    }
}

#[cfg(test)]
mod dex_notice_layout_tests {
    use super::*;

    #[test]
    fn quiet_notice_does_not_overwrite_welcome_identity() {
        let state = crate::state::AppState::new();
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);
        ChatView::new(&state)
            .with_dex_presentation(
                crate::components::dex_companion::DexPersonality::Quiet,
                false,
            )
            .with_dex_delight(
                Default::default(),
                Some("Welcome back. Your answer is needed"),
                None,
                None,
            )
            .render(area, &mut buffer);
        let lines: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let notice = lines
            .iter()
            .position(|line| line.contains("Welcome back. Your answer is needed"))
            .unwrap();
        let status = lines
            .iter()
            .position(|line| line.contains(super::super::deixic_logo::PRODUCT_TITLE))
            .unwrap();
        assert_ne!(notice, status);
    }
}

#[cfg(test)]
mod worker_badge_tests {
    use super::*;
    #[test]
    fn worker_activity_is_visible_in_the_footer() {
        let area = Rect::new(0, 0, 100, 1);
        let mut buffer = Buffer::empty(area);
        StatusBarWidget::new(None, None, None, None)
            .with_worker_badge(Some("↗ 2 running · 1 need input /workers"))
            .render(area, &mut buffer);
        let text: String = (0..100).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(text.contains("↗ 2 running"), "{text}");
        assert!(text.contains("/workers"), "{text}");
    }
}

#[cfg(test)]
mod transparent_theme_regression {
    use super::*;
    #[test]
    fn transparent_high_contrast_uses_selected_palette() {
        let theme = crate::themes::high_contrast_theme();
        assert!(theme.canvas_style().bg.is_none());
        let actual = conversation_theme_for(&theme);
        let expected = theme.ui_theme();
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.muted, expected.muted);
        assert_eq!(actual.focus, expected.focus);
        assert_eq!(
            semantic_color_for_theme(&theme, "md_link", Color::Blue),
            theme.get_color("md_link").unwrap()
        );
        assert_eq!(
            conversation_theme_for(&crate::themes::dark_theme()).text,
            crate::themes::dark_theme().ui_theme().text
        );
    }
}
