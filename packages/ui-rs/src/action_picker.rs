//! Caller-owned picker lifecycle and input, paired with existing UI primitives.
use crate::{KeyHint, Notice, NoticeTone, Picker, SELECTION_MARKER, UiTheme, key_hints};
use crossterm::event::KeyCode;
use maestro_interaction::{Action, ActionCatalog, Selection, Shortcut};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use std::{collections::HashSet, fmt, sync::LazyLock};

#[derive(Clone, Copy)]
enum Command {
    Up,
    Down,
    Confirm,
    Cancel,
    Left,
    Right,
    Backspace,
}
const CONTROLS: [Action<Command>; 7] = [
    Action::new("up", "Previous item", Command::Up).shortcut(Shortcut::Up),
    Action::new("down", "Next item", Command::Down).shortcut(Shortcut::Down),
    Action::new("confirm", "Select", Command::Confirm).shortcut(Shortcut::Enter),
    Action::new("cancel", "Cancel", Command::Cancel).shortcut(Shortcut::Escape),
    Action::new("left", "Move left", Command::Left).shortcut(Shortcut::Left),
    Action::new("right", "Move right", Command::Right).shortcut(Shortcut::Right),
    Action::new("backspace", "Delete character", Command::Backspace).shortcut(Shortcut::Backspace),
];
static CATALOG: LazyLock<ActionCatalog<'static, Command>> =
    LazyLock::new(|| ActionCatalog::new(&CONTROLS).expect("picker controls must be unambiguous"));

/// Selection changes are previews; a terminal result is returned once per opening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome<T> {
    Pending,
    Changed(Option<T>),
    Selected(T),
    Cancelled,
}

/// Invalid host-provided item identity. Replacement leaves the picker unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerError {
    DuplicateId(String),
}
impl fmt::Display for PickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(f, "duplicate picker item ID: {id}"),
        }
    }
}
impl std::error::Error for PickerError {}

/// Host-owned loading or failure state, shown in place of selectable results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PickerStatus {
    #[default]
    Ready,
    Loading(String),
    Error(String),
}

/// Wording for the shared bindings; bindings themselves come from the catalog.
#[derive(Clone, Copy)]
pub struct PickerHelp<'a> {
    pub navigation: &'a str,
    pub confirm: &'a str,
    pub key_separator: &'a str,
}
impl Default for PickerHelp<'_> {
    fn default() -> Self {
        Self {
            navigation: crate::localization::tr("navigate"),
            confirm: crate::localization::tr("select"),
            key_separator: ": ",
        }
    }
}
impl PickerHelp<'_> {
    /// Render the same bindings that handle input, using product-owned wording.
    pub fn text(self) -> String {
        let key = |id| {
            CATALOG
                .find(id)
                .and_then(|a| a.shortcut)
                .expect("picker action has a binding")
                .label()
        };
        crate::localization::format(
            "{0}{1}{2}{3} · {4}{5}{6} · {7}{8}cancel",
            &[
                (key("up")).to_string(),
                (key("down")).to_string(),
                (self.key_separator).to_string(),
                (self.navigation).to_string(),
                (key("confirm")).to_string(),
                (self.key_separator).to_string(),
                (self.confirm).to_string(),
                (key("cancel")).to_string(),
                (self.key_separator).to_string(),
            ],
        )
    }
    /// Styled bindings, retaining the existing custom wording and key separator.
    pub fn line(self, theme: UiTheme) -> Line<'static> {
        let key = |id| {
            CATALOG
                .find(id)
                .and_then(|a| a.shortcut)
                .expect("picker action has a binding")
                .label()
        };
        let mut line = Line::default();
        for (index, (key, label)) in [
            (format!("{}{}", key("up"), key("down")), self.navigation),
            (
                key(crate::localization::tr("confirm")).to_owned(),
                self.confirm,
            ),
            (
                key(crate::localization::tr("cancel")).to_owned(),
                crate::localization::tr("cancel"),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if index > 0 {
                line.spans.push(Span::styled(" · ", theme.muted_style()));
            }
            let hint = key_hints(&[KeyHint::new(&key, label)], theme);
            // Own the generated footer so dynamic position labels can be temporary.
            let key_style = hint.spans[0].style;
            line.spans.push(Span::styled(key, key_style));
            line.spans.push(Span::styled(
                self.key_separator.to_owned(),
                theme.muted_style(),
            ));
            line.spans
                .push(Span::styled(label.to_owned(), theme.muted_style()));
        }
        line.style(theme.muted_style())
    }
}

/// Presentation options. Omit search by constructing a picker without `searchable`.
#[derive(Default, Clone, Copy)]
pub struct PickerOptions<'a> {
    pub placeholder: &'a str,
    pub empty: &'a str,
    pub help: PickerHelp<'a>,
    /// Full caller-owned footer override, including any additional bindings.
    /// When omitted, help is generated from the shared bindings.
    pub help_text: Option<&'a str>,
    /// Styled caller-owned bindings. Takes precedence over `help_text`.
    pub hints: Option<&'a [KeyHint<'a>]>,
    /// Show the selected position in plain lists when all rows cannot fit.
    pub position_when_clipped: bool,
}

/// Owns transient query, selection, scroll, and open/closed state. No I/O or executor.
/// The surrounding application keeps ownership of which modal receives input.
pub struct ActionPicker<T> {
    items: Vec<T>,
    filtered: Vec<usize>,
    selection: Selection,
    list_state: ListState,
    query: String,
    cursor: usize,
    search_text: Option<fn(&T) -> &str>,
    matches: Option<fn(&T, &str) -> bool>,
    identity: Option<fn(&T) -> &str>,
    status: PickerStatus,
    visible: bool,
}
impl<T> ActionPicker<T> {
    pub fn new(items: Vec<T>) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selection: Selection::default(),
            list_state: ListState::default(),
            query: String::new(),
            cursor: 0,
            search_text: None,
            matches: None,
            identity: None,
            status: PickerStatus::Ready,
            visible: false,
        }
    }
    /// Enable case-insensitive substring search over host-provided item text.
    pub fn searchable(mut self, text: fn(&T) -> &str) -> Self {
        self.search_text = Some(text);
        self.matches = None;
        self
    }
    /// Enable host-defined matching. The query is passed exactly as typed,
    /// without normalization; the host owns case sensitivity and ranking.
    pub fn matching(mut self, matches: fn(&T, &str) -> bool) -> Self {
        self.matches = Some(matches);
        self.search_text = None;
        self
    }
    pub fn identified_by(mut self, identity: fn(&T) -> &str) -> Result<Self, PickerError> {
        Self::validate_ids(&self.items, identity)?;
        self.identity = Some(identity);
        Ok(self)
    }
    fn validate_ids(items: &[T], identity: fn(&T) -> &str) -> Result<(), PickerError> {
        let mut ids = HashSet::new();
        for item in items {
            let id = identity(item);
            if !ids.insert(id) {
                return Err(PickerError::DuplicateId(id.to_owned()));
            }
        }
        Ok(())
    }
    /// Select a currently visible item by ID; unknown or hidden IDs do nothing.
    pub fn select_id(&mut self, id: &str) -> bool {
        if self.status != PickerStatus::Ready {
            return false;
        }
        let Some(identity) = self.identity else {
            return false;
        };
        let Some(index) = self
            .filtered
            .iter()
            .position(|&i| identity(&self.items[i]) == id)
        else {
            return false;
        };
        self.select_index(index);
        true
    }
    fn select_index(&mut self, index: usize) {
        self.selection.reset();
        for _ in 0..index {
            self.selection.down(self.filtered.len());
        }
        self.sync_selection();
    }
    pub fn set_status(&mut self, status: PickerStatus) {
        self.status = status;
    }
    fn has_search(&self) -> bool {
        self.search_text.is_some() || self.matches.is_some()
    }
    fn selected_index(&self) -> Option<usize> {
        (self.status == PickerStatus::Ready)
            .then(|| self.selection.get(&self.filtered).copied())
            .flatten()
    }
    /// Open with an empty search and the first result selected.
    pub fn open(&mut self) {
        self.visible = true;
        self.query.clear();
        self.cursor = 0;
        self.selection.reset();
        self.list_state = ListState::default();
        self.filtered.clear();
        self.filter();
    }
    pub fn close(&mut self) {
        self.visible = false;
    }
    pub fn is_open(&self) -> bool {
        self.visible
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn selected(&self) -> Option<&T> {
        self.selected_index().and_then(|i| self.items.get(i))
    }
    fn filter(&mut self) {
        let selected = self.selection.get(&self.filtered).copied();
        let query = self.query.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                self.matches.map_or_else(
                    || {
                        self.search_text
                            .is_none_or(|text| text(item).to_lowercase().contains(&query))
                    },
                    |matches| matches(item, &self.query),
                )
            })
            .map(|(i, _)| i)
            .collect();
        let index = selected.and_then(|selected| self.filtered.iter().position(|&i| i == selected));
        self.select_index(index.unwrap_or(0));
    }
    fn sync_selection(&mut self) {
        self.list_state
            .select(self.selection.index(self.filtered.len()));
    }
    /// Draw the open picker; item styling remains a product decision.
    pub fn render<'a>(
        &'a mut self,
        frame: &mut Frame,
        area: Rect,
        theme: UiTheme,
        options: PickerOptions<'_>,
        row: impl Fn(&'a T) -> ListItem<'a>,
    ) {
        let theme = theme.on_panel();
        if !self.visible {
            return;
        }
        let status_text = match &self.status {
            PickerStatus::Ready => None,
            PickerStatus::Loading(message) | PickerStatus::Error(message) => Some(message.as_str()),
        };
        let items: Vec<_> = if status_text.is_some() {
            Vec::new()
        } else {
            self.filtered.iter().map(|&i| row(&self.items[i])).collect()
        };
        let help = if let Some(hints) = options.hints {
            key_hints(hints, theme)
        } else if let Some(text) = options.help_text {
            Line::raw(text)
        } else {
            options.help.line(theme)
        };
        let tone = match self.status {
            PickerStatus::Ready => NoticeTone::Neutral,
            PickerStatus::Loading(_) => NoticeTone::Busy,
            PickerStatus::Error(_) => NoticeTone::Error,
        };
        let empty = status_text.unwrap_or(options.empty);
        if self.has_search() {
            let mut picker = Picker::new(&self.query, options.placeholder, items, theme)
                .cursor(self.cursor)
                .empty(empty)
                .help(help);
            if let Some(message) = status_text {
                picker = picker.notice(message, tone);
            }
            picker.render(frame, area, &mut self.list_state);
        } else {
            let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                .split(area.intersection(frame.area()));
            let help = if options.hints.is_none()
                && options.help_text.is_none()
                && status_text.is_none()
                && options.position_when_clipped
                && self.filtered.len() > usize::from(chunks[0].height)
            {
                let position = format!(
                    "{}/{}",
                    self.selection
                        .index(self.filtered.len())
                        .map_or(0, |i| i + 1),
                    self.filtered.len()
                );
                PickerHelp {
                    navigation: &position,
                    ..options.help
                }
                .line(theme)
            } else {
                help
            };
            if items.is_empty() {
                frame.render_widget(Notice::themed(empty, tone, theme), chunks[0]);
            } else {
                frame.render_stateful_widget(
                    List::new(items)
                        .style(theme.text_style())
                        .highlight_symbol(SELECTION_MARKER)
                        .highlight_style(theme.selection_style()),
                    chunks[0],
                    &mut self.list_state,
                );
            }
            frame.render_widget(Paragraph::new(help).style(theme.muted_style()), chunks[1]);
        }
    }
}
impl<T: Clone> ActionPicker<T> {
    /// Replace results atomically, retaining query and open state. Stable IDs
    /// preserve the current choice when visible; otherwise select the first row.
    /// Always returns the fresh selected payload so hosts can refresh previews.
    pub fn replace_items(&mut self, items: Vec<T>) -> Result<PickerOutcome<T>, PickerError> {
        if let Some(identity) = self.identity {
            Self::validate_ids(&items, identity)?;
        }
        let selected_id = self.identity.and_then(|identity| {
            self.selection
                .get(&self.filtered)
                .map(|&i| identity(&self.items[i]).to_owned())
        });
        self.items = items;
        self.selection.reset();
        self.list_state = ListState::default();
        self.filtered.clear();
        self.filter();
        if let (Some(id), Some(identity)) = (selected_id, self.identity) {
            if let Some(index) = self
                .filtered
                .iter()
                .position(|&i| identity(&self.items[i]) == id)
            {
                self.select_index(index);
            }
        }
        Ok(PickerOutcome::Changed(self.selected().cloned()))
    }
    /// Paste through the same search and selection path as ordinary typing.
    pub fn insert_str(&mut self, text: &str) -> PickerOutcome<T> {
        if !self.visible || !self.has_search() {
            return PickerOutcome::Pending;
        }
        let before = self.selected_index();
        self.query.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.filter();
        self.changed_since(before)
    }
    fn changed_since(&self, before: Option<usize>) -> PickerOutcome<T> {
        if before == self.selected_index() {
            PickerOutcome::Pending
        } else {
            PickerOutcome::Changed(self.selected().cloned())
        }
    }
    /// Route native keys once; Ctrl+characters cannot accidentally edit the query.
    pub fn handle_key(&mut self, code: KeyCode, ctrl: bool) -> PickerOutcome<T> {
        if !self.visible {
            return PickerOutcome::Pending;
        }
        let before = self.selected_index();
        let shortcut = match code {
            KeyCode::Up => Some(Shortcut::Up),
            KeyCode::Down => Some(Shortcut::Down),
            KeyCode::Enter => Some(Shortcut::Enter),
            KeyCode::Esc => Some(Shortcut::Escape),
            KeyCode::Left => Some(Shortcut::Left),
            KeyCode::Right => Some(Shortcut::Right),
            KeyCode::Backspace => Some(Shortcut::Backspace),
            _ => None,
        };
        match shortcut
            .and_then(|key| CATALOG.binding(key))
            .map(|a| a.value)
        {
            Some(Command::Up) if self.status == PickerStatus::Ready => {
                self.selection.up(self.filtered.len())
            }
            Some(Command::Down) if self.status == PickerStatus::Ready => {
                self.selection.down(self.filtered.len())
            }
            Some(Command::Confirm) => {
                if self.status != PickerStatus::Ready {
                    return PickerOutcome::Pending;
                }
                let selected = self.selected().cloned();
                self.close();
                return selected.map_or(PickerOutcome::Cancelled, PickerOutcome::Selected);
            }
            Some(Command::Cancel) => {
                self.close();
                return PickerOutcome::Cancelled;
            }
            Some(Command::Left) if self.has_search() => {
                self.cursor -= self.query[..self.cursor]
                    .chars()
                    .last()
                    .map_or(0, char::len_utf8);
            }
            Some(Command::Right) if self.has_search() => {
                self.cursor += self.query[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            }
            Some(Command::Backspace) if self.has_search() && self.cursor > 0 => {
                self.cursor -= self.query[..self.cursor]
                    .chars()
                    .last()
                    .map_or(0, char::len_utf8);
                self.query.remove(self.cursor);
                self.filter();
            }
            _ => {
                if let KeyCode::Char(c) = code {
                    if !ctrl {
                        self.insert_str(&c.to_string());
                    }
                }
            }
        }
        self.sync_selection();
        self.changed_since(before)
    }
}
