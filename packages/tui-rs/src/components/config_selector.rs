//! Configuration/Preferences selector modal
//!
//! Provides a UI for viewing and modifying runtime configuration settings.

use maestro_ui::{Modal, SettingField, SettingsForm};
use ratatui::{Frame, layout::Rect, widgets::ListState};

/// A single configurable setting
#[derive(Debug, Clone)]
pub struct ConfigOption {
    localized: bool,
    /// Setting key/identifier
    pub key: String,
    /// Human-readable name
    pub name: String,
    /// Current value display
    pub value: String,
    /// Available options (for enum-like settings)
    pub options: Vec<String>,
    /// Index of current option
    pub current_option: usize,
    /// Description of the setting
    pub description: String,
    /// Category for grouping
    pub category: ConfigCategory,
}

impl ConfigOption {
    /// Create a new config option
    pub fn new(
        key: &str,
        name: &str,
        description: &str,
        options: Vec<&str>,
        current: usize,
        category: ConfigCategory,
    ) -> Self {
        Self {
            localized: false,
            key: key.to_string(),
            name: name.to_string(),
            value: options
                .get(current)
                .map(|s| (*s).to_string())
                .unwrap_or_default(),
            options: options.into_iter().map(String::from).collect(),
            current_option: current,
            description: description.to_string(),
            category,
        }
    }

    /// Cycle to next option
    pub fn next_option(&mut self) {
        if !self.options.is_empty() {
            self.current_option = (self.current_option + 1) % self.options.len();
            self.value = self.options[self.current_option].clone();
        }
    }

    /// Cycle to previous option
    pub fn prev_option(&mut self) {
        if !self.options.is_empty() {
            self.current_option = if self.current_option == 0 {
                self.options.len() - 1
            } else {
                self.current_option - 1
            };
            self.value = self.options[self.current_option].clone();
        }
    }
}

/// Categories for grouping settings
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigCategory {
    /// AI/Model settings
    Model,
    /// Security settings
    Security,
    /// UI/Display settings
    Display,
    /// Tool execution settings
    Tools,
}

impl ConfigCategory {
    fn display_name(&self) -> &'static str {
        match self {
            ConfigCategory::Model => "Model & AI",
            ConfigCategory::Security => maestro_ui::localization::tr("Security"),
            ConfigCategory::Display => maestro_ui::localization::tr("Display"),
            ConfigCategory::Tools => maestro_ui::localization::tr("Tools"),
        }
    }
}

/// Event emitted when a config option changes
#[derive(Debug, Clone)]
pub struct ConfigChangeEvent {
    /// The setting key that changed
    pub key: String,
    /// The new value
    pub value: String,
    /// Index of the new option
    pub option_index: usize,
}

/// Configuration selector modal state
pub struct ConfigSelector {
    /// Available settings
    options: Vec<ConfigOption>,
    /// Selected setting index
    selected: usize,
    /// Whether the modal is visible
    visible: bool,
    /// Pending changes (key -> new value)
    changes: Vec<ConfigChangeEvent>,
    /// List state for scrolling
    list_state: ListState,
    experiment_saved: Option<bool>,
    experiment_error: Option<String>,
}

impl Default for ConfigSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigSelector {
    /// Create a new config selector with default settings
    #[must_use]
    pub fn new() -> Self {
        let mut options = maestro_ui::localization::with_locale(
            crate::localization::Locale::English,
            Self::default_options,
        );
        for option in &mut options {
            option.localized = true;
        }
        Self {
            options,
            selected: 0,
            visible: false,
            changes: Vec::new(),
            list_state: ListState::default(),
            experiment_saved: None,
            experiment_error: None,
        }
    }

    /// Create default configuration options
    fn default_options() -> Vec<ConfigOption> {
        vec![
            // Model settings
            ConfigOption::new(
                "reasoning_effort",
                maestro_ui::localization::tr("Reasoning Effort"),
                maestro_ui::localization::tr("How much 'thinking' the model does"),
                vec!["off", "low", "medium", "high"],
                0,
                ConfigCategory::Model,
            ),
            ConfigOption::new(
                "reasoning_summary",
                maestro_ui::localization::tr("Reasoning Summary"),
                maestro_ui::localization::tr("How reasoning is summarized in output"),
                vec!["off", "auto", "concise", "detailed"],
                1,
                ConfigCategory::Model,
            ),
            ConfigOption::new(
                "verbosity",
                maestro_ui::localization::tr("Model Verbosity"),
                maestro_ui::localization::tr("How verbose the model's responses are"),
                vec!["quiet", "normal", "verbose"],
                1,
                ConfigCategory::Model,
            ),
            // Security settings
            ConfigOption::new(
                "sandbox_mode",
                maestro_ui::localization::tr("Sandbox Mode"),
                maestro_ui::localization::tr("Restrict command execution to safe zones"),
                vec!["off", "permissive", "strict"],
                0,
                ConfigCategory::Security,
            ),
            ConfigOption::new(
                "approval_policy",
                maestro_ui::localization::tr("Approval Policy"),
                maestro_ui::localization::tr("When to require user approval"),
                vec!["auto", "suggest", "always"],
                0,
                ConfigCategory::Security,
            ),
            // Display settings
            ConfigOption::new(
                "notifications",
                maestro_ui::localization::tr("Notifications"),
                maestro_ui::localization::tr("When to send desktop notifications"),
                vec!["off", "errors", "all"],
                1,
                ConfigCategory::Display,
            ),
            ConfigOption::new(
                "syntax_highlight",
                maestro_ui::localization::tr("Syntax Highlighting"),
                maestro_ui::localization::tr("Enable code syntax highlighting"),
                vec!["off", "on"],
                1,
                ConfigCategory::Display,
            ),
            // Tool settings
            ConfigOption::new(
                "web_search",
                maestro_ui::localization::tr("Web Search"),
                maestro_ui::localization::tr("Enable web search tool"),
                vec!["off", "on"],
                1,
                ConfigCategory::Tools,
            ),
            ConfigOption::new(
                "view_image",
                maestro_ui::localization::tr("Image Viewing"),
                maestro_ui::localization::tr("Enable image/screenshot tools"),
                vec!["off", "on"],
                1,
                ConfigCategory::Tools,
            ),
            ConfigOption::new(
                "experiments",
                "Experiments",
                "Randomized standard or smaller initial tools; content-free numeric telemetry.",
                vec!["Off", "On"],
                0,
                ConfigCategory::Model,
            ),
        ]
    }

    /// The reachable preferences panel includes only settings with a durable owner.
    pub fn experiments() -> Self {
        let mut selector = Self::new();
        selector
            .options
            .retain(|option| option.key == "experiments");
        selector
    }

    pub fn show_experiments(&mut self) {
        self.show();
        self.load_experiment_result(
            maestro_local_host::experiments::load().map(|value| value.enabled),
        );
    }

    fn load_experiment_result(&mut self, result: anyhow::Result<bool>) {
        match result {
            Ok(enabled) => {
                self.experiment_saved = Some(enabled);
                self.experiment_error = None;
                self.set_option("experiments", usize::from(enabled));
            }
            Err(error) => {
                self.experiment_saved = None;
                self.experiment_error = Some(format!(
                    "Could not read Experiments: {error}. Close and reopen to retry."
                ));
                if let Some(option) = self
                    .options
                    .iter_mut()
                    .find(|option| option.key == "experiments")
                {
                    option.value = "Unavailable".into();
                }
            }
        }
    }

    pub fn save_experiments(&mut self) -> bool {
        self.save_experiments_with(|enabled| {
            maestro_local_host::experiments::set_enabled(enabled).map(|value| value.enabled)
        })
    }

    fn save_experiments_with(&mut self, save: impl FnOnce(bool) -> anyhow::Result<bool>) -> bool {
        let Some(previous) = self.experiment_saved else {
            return false;
        };
        let enabled = self
            .options
            .iter()
            .any(|option| option.key == "experiments" && option.current_option == 1);
        // Opening, confirming an unchanged setting, and cancelling never enroll.
        if enabled == previous {
            return true;
        }
        match save(enabled) {
            Ok(accepted) => {
                self.experiment_saved = Some(accepted);
                self.experiment_error = None;
                self.set_option("experiments", usize::from(accepted));
                true
            }
            Err(error) => {
                self.set_option("experiments", usize::from(previous));
                self.experiment_error = Some(format!(
                    "Could not save Experiments: {error}. Change the setting to retry."
                ));
                false
            }
        }
    }

    /// Show the modal
    pub fn show(&mut self) {
        self.visible = true;
        self.changes.clear();
        self.selected = 0;
        if self.options.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    /// Hide the modal
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Check if visible
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Move selection up
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.list_state.select(Some(self.selected));
        }
    }

    /// Move selection down
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
            self.list_state.select(Some(self.selected));
        }
    }

    /// Cycle selected option to next value
    pub fn next_value(&mut self) {
        if self.options.len() == 1 && self.experiment_saved.is_none() {
            return;
        }
        if let Some(opt) = self.options.get_mut(self.selected) {
            opt.next_option();
            self.changes.push(ConfigChangeEvent {
                key: opt.key.clone(),
                value: opt.value.clone(),
                option_index: opt.current_option,
            });
        }
    }

    /// Cycle selected option to previous value
    pub fn prev_value(&mut self) {
        if self.options.len() == 1 && self.experiment_saved.is_none() {
            return;
        }
        if let Some(opt) = self.options.get_mut(self.selected) {
            opt.prev_option();
            self.changes.push(ConfigChangeEvent {
                key: opt.key.clone(),
                value: opt.value.clone(),
                option_index: opt.current_option,
            });
        }
    }

    /// Get the selected option
    #[must_use]
    pub fn selected_option(&self) -> Option<&ConfigOption> {
        self.options.get(self.selected)
    }

    /// Get all options
    #[must_use]
    pub fn options(&self) -> &[ConfigOption] {
        &self.options
    }

    /// Get pending changes and clear them
    pub fn take_changes(&mut self) -> Vec<ConfigChangeEvent> {
        std::mem::take(&mut self.changes)
    }

    /// Update a setting by key
    pub fn set_option(&mut self, key: &str, option_index: usize) {
        if let Some(opt) = self.options.iter_mut().find(|o| o.key == key) {
            if option_index < opt.options.len() {
                opt.current_option = option_index;
                opt.value = opt.options[option_index].clone();
            }
        }
    }

    /// Confirm and return all changes
    pub fn confirm(&mut self) -> Vec<ConfigChangeEvent> {
        self.hide();
        self.take_changes()
    }

    /// Cancel and discard changes
    pub fn cancel(&mut self) {
        self.hide();
        self.changes.clear();
    }

    /// Render the modal
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.visible {
            return;
        }

        let theme = crate::themes::current_ui_theme();
        let mut inner = Modal::new(maestro_ui::localization::tr(" Preferences "), 70, 20)
            .theme(theme)
            .render(frame, area);
        if self.options.len() == 1
            && self
                .selected_option()
                .is_some_and(|option| option.key == "experiments")
        {
            use ratatui::{
                layout::{Constraint, Layout},
                widgets::{Paragraph, Wrap},
            };
            let chunks =
                Layout::vertical([Constraint::Min(3), Constraint::Length(11)]).split(inner);
            inner = chunks[0];
            let disclosure = concat!(
                "Native tool profile v1: randomized standard or smaller initial tools.\n",
                "Content-free numeric telemetry: usage, timing, runtime outcomes,\n",
                "assignment and tool fingerprints. No prompts, code or tool contents.\n",
                "Changes apply at the next safe turn.\n",
                "Off stops future collection and removes unsent records.\n",
                "Off restores standard tools at the next safe turn.\n",
                "Accepted records follow workspace retention.\n",
                "Disabled telemetry excludes participation.",
            );
            frame.render_widget(
                Paragraph::new(disclosure).wrap(Wrap { trim: true }),
                chunks[1],
            );
        }
        let fields: Vec<_> = self
            .options
            .iter()
            .map(|option| SettingField {
                category: option.category.display_name(),
                label: if option.localized {
                    maestro_ui::localization::tr(&option.name)
                } else {
                    &option.name
                },
                value: if option.localized {
                    maestro_ui::localization::tr(&option.value)
                } else {
                    &option.value
                },
                description: if option.localized {
                    maestro_ui::localization::tr(&option.description)
                } else {
                    &option.description
                },
                error: if option.key == "experiments" && self.experiment_error.is_some() {
                    self.experiment_error.as_deref()
                } else if option.options.get(option.current_option).is_none() {
                    Some(maestro_ui::localization::tr(
                        "Choose a valid option with the left or right arrow",
                    ))
                } else {
                    None
                },
            })
            .collect();
        SettingsForm::new(&fields, Some(self.selected), theme)
            .help(maestro_ui::localization::tr(
                "↑ ↓ select  ← → change  Enter confirm  Esc cancel",
            ))
            .render(frame, inner, &mut self.list_state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiments_read_failure_is_unavailable_and_cannot_enroll() {
        let mut selector = ConfigSelector::experiments();
        selector.load_experiment_result(Err(anyhow::anyhow!("broken config")));
        assert_eq!(selector.options[0].value, "Unavailable");
        selector.next_value();
        assert!(!selector.save_experiments_with(|_| panic!("must not save after failed read")));
    }

    #[test]
    fn experiments_changes_save_only_on_confirm_and_restore_on_failure() {
        let mut selector = ConfigSelector::experiments();
        selector.load_experiment_result(Ok(false));
        assert_eq!(selector.options[0].value, "Off");
        assert!(selector.save_experiments_with(|_| panic!("unchanged must not enroll")));
        selector.next_value();
        assert_eq!(selector.experiment_saved, Some(false));
        assert!(!selector.save_experiments_with(|enabled| {
            assert!(enabled);
            Err(anyhow::anyhow!("disk full"))
        }));
        assert_eq!(selector.options[0].value, "Off");
        assert!(
            selector
                .experiment_error
                .as_ref()
                .unwrap()
                .contains("disk full")
        );
        selector.next_value();
        assert!(selector.save_experiments_with(Ok));
        assert_eq!(selector.experiment_saved, Some(true));
        assert_eq!(selector.options[0].value, "On");
        selector.prev_value();
        assert!(selector.save_experiments_with(|enabled| {
            assert!(!enabled);
            Ok(false)
        }));
        assert_eq!(selector.experiment_saved, Some(false));
    }

    #[test]
    fn experiments_cancel_preserves_saved_state_and_renders_consent() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut selector = ConfigSelector::experiments();
        selector.show();
        selector.load_experiment_result(Ok(true));
        selector.prev_value();
        selector.cancel();
        assert_eq!(selector.experiment_saved, Some(true));
        selector.show();
        selector.load_experiment_result(Ok(true));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Experiments  On"));
        assert!(text.contains("randomized standard or smaller"));
        assert!(text.contains("numeric telemetry"));
        assert!(text.contains("next safe turn"));
        assert!(text.contains("unsent records"));
        assert!(text.contains("Off restores standard tools"));
        assert!(text.contains("Accepted records follow workspace retention"));
        // The last disclosure line proves the explanation is not vertically clipped.
        assert!(text.contains("Disabled telemetry excludes participation"));
    }

    #[test]
    fn settings_navigation_scrolls_to_the_selected_option_after_group_headers() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut selector = ConfigSelector::new();
        selector.show();
        for _ in 0..selector.options.len() {
            selector.move_down();
        }
        let label = selector.selected_option().unwrap().name.clone();
        let old_value = selector.selected_option().unwrap().value.clone();
        selector.next_value();
        assert_ne!(selector.selected_option().unwrap().value, old_value);
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal
            .draw(|frame| selector.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains(&label));
        assert!(selector.list_state.offset() > 0);
        assert!(selector.list_state.selected().unwrap() > selector.selected);
    }

    #[test]
    fn test_config_selector_creation() {
        let selector = ConfigSelector::new();
        assert!(!selector.is_visible());
        assert!(!selector.options.is_empty());
    }

    #[test]
    fn test_config_selector_show_hide() {
        let mut selector = ConfigSelector::new();
        selector.show();
        assert!(selector.is_visible());
        selector.hide();
        assert!(!selector.is_visible());
    }

    #[test]
    fn test_config_selector_navigation() {
        let mut selector = ConfigSelector::new();
        selector.show();

        assert_eq!(selector.selected, 0);
        selector.move_down();
        assert_eq!(selector.selected, 1);
        selector.move_up();
        assert_eq!(selector.selected, 0);
    }

    #[test]
    fn test_config_option_cycle() {
        let mut opt = ConfigOption::new(
            "test",
            "Test",
            "Description",
            vec!["a", "b", "c"],
            0,
            ConfigCategory::Model,
        );

        assert_eq!(opt.value, "a");
        opt.next_option();
        assert_eq!(opt.value, "b");
        opt.next_option();
        assert_eq!(opt.value, "c");
        opt.next_option();
        assert_eq!(opt.value, "a"); // Wraps around
    }

    #[test]
    fn test_config_option_prev() {
        let mut opt = ConfigOption::new(
            "test",
            "Test",
            "Description",
            vec!["a", "b", "c"],
            0,
            ConfigCategory::Model,
        );

        assert_eq!(opt.value, "a");
        opt.prev_option();
        assert_eq!(opt.value, "c"); // Wraps around to end
        opt.prev_option();
        assert_eq!(opt.value, "b");
    }

    #[test]
    fn test_config_selector_changes() {
        let mut selector = ConfigSelector::new();
        selector.show();

        selector.next_value();
        let changes = selector.take_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key, "reasoning_effort");
    }

    #[test]
    fn test_config_selector_set_option() {
        let mut selector = ConfigSelector::new();

        selector.set_option("sandbox_mode", 2);
        let opt = selector
            .options
            .iter()
            .find(|o| o.key == "sandbox_mode")
            .unwrap();
        assert_eq!(opt.value, "strict");
    }

    #[test]
    fn test_config_selector_confirm() {
        let mut selector = ConfigSelector::new();
        selector.show();
        selector.next_value();

        let changes = selector.confirm();
        assert!(!changes.is_empty());
        assert!(!selector.is_visible());
    }

    #[test]
    fn test_config_selector_cancel() {
        let mut selector = ConfigSelector::new();
        selector.show();
        selector.next_value();

        selector.cancel();
        assert!(!selector.is_visible());
    }

    #[test]
    fn test_config_category_display() {
        assert_eq!(ConfigCategory::Model.display_name(), "Model & AI");
        assert_eq!(ConfigCategory::Security.display_name(), "Security");
        assert_eq!(ConfigCategory::Display.display_name(), "Display");
        assert_eq!(ConfigCategory::Tools.display_name(), "Tools");
    }
}

#[cfg(test)]
mod localization_regression_tests {
    use super::*;

    #[test]
    fn language_does_not_change_setting_ids_or_values() {
        use crate::localization::{Locale, with_locale};
        let canonical = ConfigSelector::new().options;
        for locale in Locale::ALL {
            with_locale(locale, || {
                let mut localized = ConfigSelector::new();
                for (actual, expected) in localized.options.iter_mut().zip(&canonical) {
                    assert_eq!(actual.key, expected.key);
                    assert_eq!(actual.value, expected.value);
                    assert_eq!(actual.options, expected.options);
                    let mut next = expected.clone();
                    next.next_option();
                    actual.next_option();
                    assert_eq!(actual.value, next.value);
                }
            });
        }
    }
}
