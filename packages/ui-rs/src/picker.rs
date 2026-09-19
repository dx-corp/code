//! Searchable result presentation with caller-owned selection and scrolling.
use crate::{Notice, NoticeTone, SELECTION_MARKER, SearchField, UiTheme};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

/// A search field, result list (or message), and optional keyboard help.
///
/// Filtering, editing, navigation and effects remain with the caller. The
/// supplied ListState is the existing scroll/selection owner.
#[must_use]
pub struct Picker<'a> {
    query: &'a str,
    cursor: Option<usize>,
    placeholder: &'a str,
    items: Vec<ListItem<'a>>,
    theme: UiTheme,
    empty: Line<'a>,
    help: Option<Line<'a>>,
    message: Option<(Line<'a>, Style)>,
}

impl<'a> Picker<'a> {
    /// Present already-filtered rows with the application's current palette.
    pub fn new(
        query: &'a str,
        placeholder: &'a str,
        items: Vec<ListItem<'a>>,
        theme: UiTheme,
    ) -> Self {
        Self {
            query,
            cursor: None,
            placeholder,
            items,
            theme,
            empty: crate::localization::tr("No matches found").into(),
            help: None,
            message: None,
        }
    }
    /// Keep the caller's byte cursor visible in the search field.
    pub fn cursor(mut self, cursor: usize) -> Self {
        self.cursor = Some(cursor);
        self
    }
    /// Set the message shown when the result collection is empty.
    pub fn empty(mut self, text: impl Into<Line<'a>>) -> Self {
        self.empty = text.into();
        self
    }
    /// Reserve the final row for the caller's actual keyboard bindings.
    pub fn help(mut self, text: impl Into<Line<'a>>) -> Self {
        self.help = Some(text.into());
        self
    }
    /// Replace results with a caller-owned loading or error message.
    pub fn message(mut self, text: impl Into<Line<'a>>, style: Style) -> Self {
        self.message = Some((text.into(), style));
        self
    }
    /// Replace results with an explicit semantic notice.
    pub fn notice(mut self, text: impl Into<Line<'a>>, tone: NoticeTone) -> Self {
        let notice = Notice::themed(text, tone, self.theme);
        self.message = Some((notice.text, notice.style));
        self
    }

    /// Render inside a surface and return the search rectangle for cursor placement.
    pub fn render(self, frame: &mut Frame, area: Rect, state: &mut ListState) -> Rect {
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(u16::from(self.help.is_some())),
        ])
        .split(area.intersection(frame.area()));
        let mut search = SearchField::new(self.query, self.placeholder)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(self.theme.border)),
            )
            .theme(self.theme);
        if let Some(cursor) = self.cursor {
            search = search.cursor(cursor);
        }
        if let Some(position) = search.cursor_position(chunks[0]) {
            frame.set_cursor_position(position);
        }
        frame.render_widget(search, chunks[0]);
        if let Some((message, style)) = self.message {
            frame.render_widget(Notice::new(message).style(style), chunks[1]);
        } else if self.items.is_empty() {
            frame.render_widget(
                Paragraph::new(self.empty).style(Style::default().fg(self.theme.muted)),
                chunks[1],
            );
        } else {
            frame.render_stateful_widget(
                List::new(self.items)
                    .style(self.theme.text_style())
                    // Keep semantic foreground colors against their normal surface.
                    // A marker and weight identify selection without color collisions.
                    .highlight_symbol(SELECTION_MARKER)
                    .highlight_style(self.theme.selection_style()),
                chunks[1],
                state,
            );
        }
        if let Some(help) = self.help {
            frame.render_widget(
                Paragraph::new(help).style(Style::default().fg(self.theme.muted)),
                chunks[2],
            );
        }
        chunks[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };

    fn theme() -> UiTheme {
        UiTheme {
            panel: None,
            selection: None,
            surface: Color::Black,
            text: Color::White,
            muted: Color::Gray,
            border: Color::Blue,
            focus: Color::Magenta,
            success: Color::Green,
            attention: Color::Yellow,
            error: Color::Red,
        }
    }

    #[test]
    fn picker_preserves_selection_and_scrolls_to_selected_result() {
        let mut terminal = Terminal::new(TestBackend::new(32, 8)).unwrap();
        let mut state = ListState::default().with_selected(Some(19));
        terminal
            .draw(|frame| {
                let items = (0..20).map(|n| ListItem::new(format!("row {n}"))).collect();
                Picker::new("query", "Search", items, theme())
                    .help("Enter select")
                    .render(frame, frame.area(), &mut state);
            })
            .unwrap();
        assert_eq!(state.selected(), Some(19));
        assert!(state.offset() > 0);
        let buf = terminal.backend().buffer();
        let text: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("row 19"));
        assert!(text.contains("Enter select"));
        assert!(text.contains("› row 19"));
        assert!(
            buf.content
                .iter()
                .any(|c| c.modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn picker_selection_preserves_semantic_foreground_contrast() {
        use ratatui::text::Span;
        let mut palette = theme();
        palette.surface = Color::White;
        palette.text = Color::Black;
        palette.focus = Color::Blue;
        palette.attention = palette.focus;
        let mut terminal = Terminal::new(TestBackend::new(32, 8)).unwrap();
        let mut state = ListState::default().with_selected(Some(0));
        terminal
            .draw(|frame| {
                let row = ListItem::new(Line::from(vec![
                    Span::styled("count", Style::default().fg(palette.focus)),
                    Span::styled(" match", Style::default().fg(palette.attention)),
                ]));
                Picker::new("", "", vec![row], palette).render(frame, frame.area(), &mut state);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 2)].symbol(), "›");
        for x in 2..13 {
            let cell = &buffer[(x, 2)];
            assert_eq!(cell.fg, palette.focus);
            assert_eq!(cell.bg, palette.surface);
            assert_ne!(cell.fg, cell.bg);
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn picker_empty_and_error_replace_results_without_changing_owner_state() {
        let mut terminal = Terminal::new(TestBackend::new(32, 8)).unwrap();
        let mut state = ListState::default().with_selected(Some(2));
        terminal
            .draw(|frame| {
                Picker::new("", "Filter here", vec![], theme())
                    .empty("No matching sessions")
                    .help("Esc close")
                    .render(frame, frame.area(), &mut state);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Filter here"));
        assert!(text.contains("No matching sessions"));
        assert_eq!(state.selected(), Some(2));
        terminal
            .draw(|frame| {
                Picker::new("", "", vec![ListItem::new("hidden row")], theme())
                    .message("Read failed", Style::default().fg(Color::Red))
                    .render(frame, frame.area(), &mut state);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Read failed"));
        assert!(!text.contains("hidden row"));
    }

    #[test]
    fn picker_handles_zero_and_narrow_areas() {
        for (width, height) in [(1, 1), (2, 3), (8, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    Picker::new("日本語", "", vec![ListItem::new("row")], theme())
                        .help("Enter")
                        .render(frame, frame.area(), &mut ListState::default());
                    Picker::new("", "", vec![], theme()).render(
                        frame,
                        Rect::default(),
                        &mut ListState::default(),
                    );
                })
                .unwrap();
        }
    }
}
