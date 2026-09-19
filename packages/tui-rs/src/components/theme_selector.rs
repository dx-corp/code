//! Theme selection uses the shared picker; applying a theme remains with the app.
use crate::themes;
use crossterm::event::KeyCode;
use maestro_ui::{ActionPicker, KeyHint, Modal, ModalSize, PickerOptions, PickerOutcome};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::ListItem,
};

pub struct ThemeSelector {
    picker: ActionPicker<String>,
    current_theme: Option<String>,
    original_theme: Option<themes::Theme>,
}
impl Default for ThemeSelector {
    fn default() -> Self {
        Self::new()
    }
}
impl ThemeSelector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            picker: ActionPicker::new(themes::available_themes())
                .identified_by(String::as_str)
                .expect("theme names are unique")
                .searchable(String::as_str),
            current_theme: None,
            original_theme: None,
        }
    }
    pub fn set_current_theme(&mut self, name: Option<String>) {
        self.current_theme = name;
    }
    pub fn show(&mut self) {
        let original = themes::current_theme();
        self.current_theme = Some(original.name.clone());
        self.picker.open();
        self.picker.select_id(&original.name);
        self.original_theme = Some(original);
    }
    pub fn hide(&mut self) {
        self.picker.close();
    }
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.picker.is_open()
    }
    pub fn insert_str(&mut self, text: &str) -> PickerOutcome<String> {
        self.picker.insert_str(text)
    }
    pub fn handle_key(&mut self, code: KeyCode, ctrl: bool) -> PickerOutcome<String> {
        self.picker.handle_key(code, ctrl)
    }
    /// The opening palette is kept in memory, including custom theme contents.
    pub fn original_theme(&self) -> Option<&themes::Theme> {
        self.original_theme.as_ref()
    }
    /// Resolve a preview/commit/restore without applying global state here.
    pub fn theme_for(
        &self,
        outcome: &PickerOutcome<String>,
    ) -> Result<Option<themes::Theme>, themes::ThemeError> {
        match outcome {
            PickerOutcome::Changed(Some(name)) | PickerOutcome::Selected(name) => {
                themes::load_theme(name).map(Some)
            }
            PickerOutcome::Changed(None) | PickerOutcome::Cancelled => {
                Ok(self.original_theme.clone())
            }
            PickerOutcome::Pending => Ok(None),
        }
    }
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.picker.is_open() {
            return;
        }
        let theme = themes::current_ui_theme();
        let inner = Modal::sized(
            maestro_ui::localization::tr("Select Theme"),
            ModalSize::Standard,
        )
        .theme(theme)
        .render(frame, area);
        let current = &self.current_theme;
        self.picker.render(
            frame,
            inner,
            theme,
            PickerOptions {
                placeholder: maestro_ui::localization::tr("Type to filter themes..."),
                empty: maestro_ui::localization::tr("No matching themes"),
                hints: Some(&[
                    KeyHint::new("Enter", maestro_ui::localization::tr("select")),
                    KeyHint::new("Esc", maestro_ui::localization::tr("cancel")),
                    KeyHint::new("↑↓", maestro_ui::localization::tr("navigate")),
                ]),
                ..PickerOptions::default()
            },
            |name| {
                let mut spans = vec![Span::styled(name.as_str(), Style::default())];
                if current.as_ref().is_some_and(|c| c == name) {
                    spans.push(Span::styled(
                        maestro_ui::localization::tr(" (current)"),
                        Style::default().fg(theme.success),
                    ));
                }
                ListItem::new(Line::from(spans))
            },
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vscode_palette_can_be_previewed_cancelled_and_selected() {
        let mut selector = ThemeSelector::new();
        selector.show();
        let original = selector.original_theme().unwrap().name.clone();
        let preview = selector.insert_str("vscode-monokai");
        let theme = selector.theme_for(&preview).unwrap().unwrap();
        assert_eq!(theme.name, "vscode-monokai");
        assert_eq!(theme.colors.assistant_message_bg, "#272822");
        let cancel = selector.handle_key(KeyCode::Esc, false);
        assert_eq!(selector.theme_for(&cancel).unwrap().unwrap().name, original);

        selector.show();
        selector.insert_str("vscode-light-modern");
        let selected = selector.handle_key(KeyCode::Enter, false);
        assert_eq!(
            selector.theme_for(&selected).unwrap().unwrap().name,
            "vscode-light-modern"
        );
        assert!(!selector.is_visible());
    }

    #[test]
    fn picker_keeps_filter_and_choices_without_a_nested_sample() {
        use ratatui::{Terminal, backend::TestBackend};
        for (width, height) in [(100, 30), (60, 20)] {
            let mut selector = ThemeSelector::new();
            selector.show();
            let original = selector.original_theme().unwrap().name.clone();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| selector.render(f, f.area())).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Select Theme"));
            assert!(text.contains("Type to filter themes"));
            assert!(!text.contains("Ask Dex"));
            assert_eq!(selector.original_theme().unwrap().name, original);
        }
    }

    #[test]
    fn current_theme_is_selected_and_preview_cancel_returns_the_opening_palette() {
        let mut selector = ThemeSelector::new();
        selector.show();
        let original = selector.original_theme().unwrap().clone();
        assert_eq!(selector.picker.selected(), Some(&original.name));
        let name = if original.name == "light" {
            "dark"
        } else {
            "light"
        };
        let outcome = selector.insert_str(name);
        assert!(matches!(outcome, PickerOutcome::Changed(Some(_))));
        assert_eq!(selector.theme_for(&outcome).unwrap().unwrap().name, name);
        let cancel = selector.handle_key(KeyCode::Esc, false);
        assert_eq!(
            selector.theme_for(&cancel).unwrap().unwrap().name,
            original.name
        );
        assert_eq!(
            selector.theme_for(&cancel).unwrap().unwrap().colors.accent,
            original.colors.accent
        );
    }

    #[test]
    fn theme_picker_filters_paste_and_confirms_without_applying_a_theme() {
        let mut selector = ThemeSelector::new();
        assert!(!selector.is_visible());
        selector.show();
        let name = selector.picker.selected().unwrap().clone();
        selector.insert_str(&name);
        assert_eq!(
            selector.handle_key(KeyCode::Enter, false),
            PickerOutcome::Selected(name)
        );
        assert!(!selector.is_visible());
        assert_eq!(
            selector.handle_key(KeyCode::Enter, false),
            PickerOutcome::Pending
        );
    }
    #[test]
    fn theme_picker_cancel_and_empty_results_never_return_a_theme() {
        let mut selector = ThemeSelector::new();
        selector.show();
        selector.handle_key(KeyCode::Down, false);
        assert_eq!(
            selector.handle_key(KeyCode::Esc, false),
            PickerOutcome::Cancelled
        );
        selector.show();
        selector.insert_str("no-such-result-zzz");
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| selector.render(f, f.area())).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("No matching themes"));
        assert!(text.contains("Enter select · Esc cancel"));
        assert!(text.contains("↑↓ navigate"));
        assert_eq!(
            selector.handle_key(KeyCode::Enter, false),
            PickerOutcome::Cancelled
        );
    }
}
