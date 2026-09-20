//! Composable terminal UI primitives for Deixic Code.
//!
//! Widgets borrow application data; the caller owns input, selection, visibility,
//! and effects. Use [`Modal`] to obtain a content rectangle, split it with
//! Ratatui's `Layout`, and render ordinary widgets such as [`SearchField`].
//! No terminal initialization, event loop, runtime, or global theme is installed.
//! [`ActionList`] renders typed `maestro_interaction::Action` catalogs; [`Notice`]
//! presents caller-owned status and hints. Use `maestro-interaction` for focus,
//! attention, navigation, reactions, and draft acceptance outside renderers.
//!
//! ```
//! use maestro_ui::{Modal, SearchField};
//! use ratatui::{backend::TestBackend, Terminal, layout::{Constraint, Layout}};
//!
//! let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
//! terminal.draw(|frame| {
//!     let inner = Modal::new(" Select item ", 50, 12).render(frame, frame.area());
//!     let areas = Layout::vertical([Constraint::Length(3), Constraint::Min(0)])
//!         .split(inner);
//!     frame.render_widget(SearchField::new("", "Type to filter..."), areas[0]);
//!     // Render the application's results into areas[1].
//! })?;
//! # Ok::<(), std::convert::Infallible>(())
//! ```

// Terminal rendering, input, and transient widget state.
pub mod ansi_text;
pub mod ansi_tracker;
pub mod ascii_animation;
pub mod borders;
pub mod confirm_dialog;
pub mod diff;
pub mod elapsed;
pub mod field_format;
pub mod focus;
pub mod key_binding;
pub mod key_hints;
pub mod keymap;
pub mod kill_ring;
pub mod layout_constraints;
pub mod live_wrap;
pub mod loader;
pub mod notification_queue;
pub mod pager;
pub mod paste_burst;
pub mod render_utils;
pub mod resize_handler;
pub mod scroll_state;
pub mod selection_list;
pub mod single_line_input;
pub mod sync_output;
pub mod text_format;
pub mod truncated_text;
pub mod undo_history;
pub mod viewport;
pub mod wrapping;

mod menu;
pub use menu::Menu;
mod action_picker;
pub use action_picker::{
    ActionPicker, PickerError, PickerHelp, PickerOptions, PickerOutcome, PickerStatus,
};
mod interaction;
mod picker;
pub use interaction::{ActionList, Notice, NoticeTone};
mod hints;
pub use hints::{KeyHint, key_hints};

/// Selection indicator shared by lists and forms.
pub const SELECTION_MARKER: &str = "› ";
mod settings;
mod theme;
pub use picker::Picker;
pub use settings::{SettingField, SettingsForm};
pub use theme::UiTheme;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Padding, Paragraph, Widget},
};

/// Shared outer dimensions, clamped to the available parent rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalSize {
    /// Short decisions and compact lists (54 × 16).
    Compact,
    /// Searchable lists and settings (72 × 22).
    Standard,
    /// Detailed previews and approvals (80 × 25).
    Wide,
}

/// A centered modal surface that clears its background and returns its content area.
///
/// The requested dimensions include the border. The default margin is two cells
/// on every side; small parents shrink the modal, including to zero. Coordinates
/// are relative to the supplied parent, so this also works inside split panes.
#[must_use]
pub struct Modal<'a> {
    width: u16,
    height: u16,
    margin: u16,
    block: Block<'a>,
}

impl<'a> Modal<'a> {
    /// Create a bordered modal with the existing selector palette.
    pub fn new(title: impl Into<Line<'a>>, width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            margin: 2,
            block: Block::default()
                .title(title)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .style(Style::default().bg(Color::Black)),
        }
    }

    /// Create a modal using a shared size. Apply `theme` for shared decoration.
    pub fn sized(title: impl Into<Line<'a>>, size: ModalSize) -> Self {
        let (width, height) = match size {
            ModalSize::Compact => (54, 16),
            ModalSize::Standard => (72, 22),
            ModalSize::Wide => (80, 25),
        };
        Self::new(title, width, height)
    }

    /// Apply application-owned semantic colors, bold title and horizontal padding.
    pub fn theme(mut self, theme: UiTheme) -> Self {
        let theme = theme.on_panel();
        self.block = self
            .block
            .border_style(Style::default().fg(theme.border))
            .title_style(Style::default().fg(theme.text).add_modifier(Modifier::BOLD))
            .padding(Padding::horizontal(1))
            .style(theme.text_style());
        self
    }

    /// Set the minimum space on each side of the modal.
    pub fn margin(mut self, margin: u16) -> Self {
        self.margin = margin;
        self
    }

    /// Set the border style without changing the title or background.
    pub fn border_style(mut self, style: Style) -> Self {
        self.block = self.block.border_style(style);
        self
    }

    /// Replace the surface decoration, including its title, borders and padding.
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = block;
        self
    }

    /// Measure the outer area without rendering or mutating application state.
    pub fn area(&self, parent: Rect) -> Rect {
        let inset = self.margin.saturating_mul(2);
        let width = self.width.min(parent.width.saturating_sub(inset));
        let height = self.height.min(parent.height.saturating_sub(inset));
        Rect::new(
            parent.x.saturating_add((parent.width - width) / 2),
            parent.y.saturating_add((parent.height - height) / 2),
            width,
            height,
        )
    }

    /// Clear and draw the surface, then return the area available for children.
    ///
    /// Pass a parent inside the frame. Children render afterward, using the
    /// returned rectangle with normal Ratatui layouts and widgets.
    pub fn render(self, frame: &mut Frame, parent: Rect) -> Rect {
        self.render_buffer(parent, frame.buffer_mut())
    }

    /// Render from a `Widget` implementation using the same bounds and decoration.
    pub fn render_buffer(self, parent: Rect, buffer: &mut Buffer) -> Rect {
        let area = self.area(parent).intersection(buffer.area);
        let inner = self.block.inner(area);
        Clear.render(area, buffer);
        self.block.render(area, buffer);
        inner
    }
}

/// A borrowed search display with an empty-query placeholder.
///
/// This is a rendering widget, not an editor: the application retains its query,
/// Unicode editing logic, focus, filtering, and cursor positioning. Text is clipped
/// by Ratatui to the provided area. A bordered field normally needs three rows.
#[must_use]
pub struct SearchField<'a> {
    query: &'a str,
    placeholder: &'a str,
    block: Block<'a>,
    text_style: Style,
    placeholder_style: Style,
    cursor: Option<usize>,
}

impl<'a> SearchField<'a> {
    /// Create a search display without copying the query or placeholder.
    pub fn new(query: &'a str, placeholder: &'a str) -> Self {
        Self {
            query,
            placeholder,
            block: Block::default()
                .title(crate::localization::tr(" Search "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
            text_style: Style::default().fg(Color::White),
            placeholder_style: Style::default().fg(Color::DarkGray),
            cursor: None,
        }
    }

    /// Apply the same semantic palette used by picker search fields.
    pub fn theme(mut self, theme: UiTheme) -> Self {
        self.block = self.block.border_style(Style::default().fg(theme.border));
        self.text_style = theme.text_style();
        self.placeholder_style = theme.muted_style();
        self
    }

    /// Replace the label, border, and padding with a normal Ratatui block.
    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = block;
        self
    }

    /// Scroll the query to keep the editor's UTF-8 byte cursor visible.
    /// The caller continues to own the cursor and editing behavior.
    pub fn cursor(mut self, cursor: usize) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Return the visible terminal cursor, using the same viewport as rendering.
    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        self.viewport(area).1
    }

    fn viewport(&self, area: Rect) -> (usize, Option<(u16, u16)>) {
        let inner = self.block.inner(area);
        let Some(mut cursor) = self.cursor else {
            return (0, None);
        };
        if inner.is_empty() {
            return (0, None);
        }
        cursor = cursor.min(self.query.len());
        while !self.query.is_char_boundary(cursor) {
            cursor -= 1;
        }
        let prefix = ratatui::text::Span::raw(&self.query[..cursor]);
        let mut remaining = prefix.width();
        let mut start = 0;
        // Advance whole graphemes, never into the second cell of a wide glyph.
        for grapheme in prefix.styled_graphemes(Style::default()) {
            if remaining < usize::from(inner.width) {
                break;
            }
            start += grapheme.symbol.len();
            remaining = remaining.saturating_sub(ratatui::text::Span::raw(grapheme.symbol).width());
        }
        (start, Some((inner.x + remaining as u16, inner.y)))
    }

    /// Set the entered-text style.
    pub fn text_style(mut self, style: Style) -> Self {
        self.text_style = style;
        self
    }

    /// Set the empty-query placeholder style.
    pub fn placeholder_style(mut self, style: Style) -> Self {
        self.placeholder_style = style;
        self
    }
}

impl Widget for SearchField<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (start, _) = self.viewport(area);
        let (text, style) = if self.query.is_empty() {
            (self.placeholder, self.placeholder_style)
        } else {
            (&self.query[start..], self.text_style)
        };
        Paragraph::new(text)
            .style(style)
            .block(self.block)
            .render(area, buf);
    }
}

/// Unicode-aware editor state and rendering, without a terminal or event loop.
pub mod textarea;

pub mod localization;
mod translations;
