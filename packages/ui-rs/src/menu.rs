//! Opinionated menu composition over the shared, effect-free picker controller.
use crate::{ActionPicker, Modal, ModalSize, PickerOptions, UiTheme};
use ratatui::{Frame, layout::Rect, widgets::ListItem};

/// A menu supplies presentation; `ActionPicker` retains input and typed outcomes.
/// The application owns the picker and handles selected values outside rendering.
pub struct Menu<'a, T> {
    title: &'a str,
    state: &'a mut ActionPicker<T>,
    options: PickerOptions<'a>,
    size: ModalSize,
}
impl<'a, T> Menu<'a, T> {
    pub fn new(title: &'a str, state: &'a mut ActionPicker<T>) -> Self {
        Self {
            title,
            state,
            options: PickerOptions {
                empty: "No matching items",
                placeholder: "Type to filter…",
                position_when_clipped: true,
                ..Default::default()
            },
            size: ModalSize::Standard,
        }
    }
    pub fn placeholder(mut self, text: &'a str) -> Self {
        self.options.placeholder = text;
        self
    }
    pub fn empty_message(mut self, text: &'a str) -> Self {
        self.options.empty = text;
        self
    }
    pub fn options(mut self, options: PickerOptions<'a>) -> Self {
        self.options = options;
        self
    }
    pub fn size(mut self, size: ModalSize) -> Self {
        self.size = size;
        self
    }
    /// Customize row content without replacing navigation, chrome, or scrolling.
    pub fn render_items<'b>(
        &'b mut self,
        frame: &mut Frame,
        area: Rect,
        theme: UiTheme,
        row: impl Fn(&'b T) -> ListItem<'b>,
    ) {
        if !self.state.is_open() {
            return;
        }
        let inner = Modal::sized(self.title, self.size)
            .theme(theme)
            .render(frame, area);
        self.state.render(frame, inner, theme, self.options, row);
    }
}
impl<T: AsRef<str>> Menu<'_, T> {
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: UiTheme) {
        self.render_items(frame, area, theme, |item| ListItem::new(item.as_ref()));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::PickerOutcome;
    use crossterm::event::KeyCode;
    #[test]
    fn menu_uses_shared_filter_and_emits_one_selection() {
        let mut state = ActionPicker::new(vec!["Alpha".to_owned(), "Beta".to_owned()])
            .searchable(String::as_str);
        state.open();
        state.insert_str("Beta");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|f| Menu::new("Choose", &mut state).render(f, f.area(), UiTheme::default()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Beta"));
        assert!(!text.contains("Alpha"));
        assert_eq!(
            state.handle_key(KeyCode::Enter, false),
            PickerOutcome::Selected("Beta".into())
        );
        assert_eq!(
            state.handle_key(KeyCode::Enter, false),
            PickerOutcome::Pending
        );
    }
}
