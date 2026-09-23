//! Command palette modal over typed palette resources.
//!
//! The palette (Ctrl+K by default) searches every [`PaletteResource`] the app
//! hands it: slash commands, workspace files, sessions, models, and themes.
//!
//! # Result rows
//!
//! `search()` rebuilds two parallel views of the current results:
//!
//! - `matches`: the selectable resource indices in display order. `selected`
//!   indexes into this list, so keyboard navigation never lands on a caption.
//! - `rows`: what is rendered, which is `matches` interleaved with
//!   non-selectable group captions on a browse (empty text) query.
//!
//! ## Empty text (browse)
//!
//! Groups appear in this order, each with a caption:
//! 1. `Recent`: non-command resources confirmed earlier in this session (session-local,
//!    bounded, looked up by stable id so they survive `set_resources`).
//! 2. `Commands`: primary commands in registry browse order.
//! 3. One group per non-command resource kind (`Files`, `Sessions`,
//!    `Models`, `Themes`). Without a kind filter each kind shows a short
//!    preview; with a kind filter (`@`, `#`, `:`, `%`, `>`) the group gets
//!    the full result budget.
//!
//! ## Typed text (search)
//!
//! Results are a flat ranked list. Every candidate is ranked before the result
//! limit is applied, so an exact or prefix match on an id, label, or alias
//! always beats a description-only match. Ties keep the incoming order.
//!
//! # Selection and scrolling
//!
//! When the query text changes the selection and list scroll offset reset to
//! the top. Registry or resource refreshes that leave the text untouched keep
//! the current selection when it is still in range.

use std::collections::HashSet;
use std::sync::Arc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, ListItem, ListState},
};

use crate::commands::CommandRegistry;
use crate::palette_resource::{PaletteResource, PaletteResourceKind};
use maestro_ui::{KeyHint, Modal, ModalSize, Picker, UiTheme, key_hints};

/// Maximum selectable rows in any result view.
const RESULT_LIMIT: usize = 30;
/// Maximum resources remembered in the session-local recent list.
const RECENT_LIMIT: usize = 5;
/// Rows shown per kind group on an unfiltered browse query.
const GROUP_PREVIEW_LIMIT: usize = 4;
/// Kind group order on a browse query.
const KIND_ORDER: [PaletteResourceKind; 5] = [
    PaletteResourceKind::Command,
    PaletteResourceKind::File,
    PaletteResourceKind::Session,
    PaletteResourceKind::Model,
    PaletteResourceKind::Theme,
];

const MIN_LABEL_WIDTH: usize = 10;
const MAX_LABEL_WIDTH: usize = 24;

fn parse_filter(query: &str) -> (Option<PaletteResourceKind>, &str) {
    let trimmed = query.trim_start();
    let prefixes = [
        (">", PaletteResourceKind::Command),
        ("@", PaletteResourceKind::File),
        ("#", PaletteResourceKind::Session),
        (":", PaletteResourceKind::Model),
        ("%", PaletteResourceKind::Theme),
        ("command:", PaletteResourceKind::Command),
        ("cmd:", PaletteResourceKind::Command),
        ("file:", PaletteResourceKind::File),
        ("session:", PaletteResourceKind::Session),
        ("model:", PaletteResourceKind::Model),
        ("theme:", PaletteResourceKind::Theme),
    ];
    for (prefix, kind) in prefixes {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return (Some(kind), rest.trim_start());
        }
    }
    (None, trimmed)
}

fn kind_caption(kind: PaletteResourceKind) -> &'static str {
    match kind {
        PaletteResourceKind::Command => maestro_ui::localization::tr("All commands"),
        PaletteResourceKind::File => maestro_ui::localization::tr("Files"),
        PaletteResourceKind::Session => maestro_ui::localization::tr("Sessions"),
        PaletteResourceKind::Model => maestro_ui::localization::tr("Models"),
        PaletteResourceKind::Theme => maestro_ui::localization::tr("Themes"),
    }
}

/// Rank buckets for a typed query. Lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchRank {
    Exact,
    Prefix,
    Contains,
    Description,
}

fn rank_resource(resource: &PaletteResource, query: &str) -> Option<MatchRank> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(MatchRank::Exact);
    }
    let names = std::iter::once(resource.id.as_str())
        .chain(std::iter::once(resource.label.as_str()))
        .chain(resource.search_terms.iter().map(String::as_str))
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if names.contains(&query) {
        return Some(MatchRank::Exact);
    }
    if names.iter().any(|name| name.starts_with(&query)) {
        return Some(MatchRank::Prefix);
    }
    if names.iter().any(|name| name.contains(&query)) {
        return Some(MatchRank::Contains);
    }
    resource
        .display_description()
        .is_some_and(|value| value.to_lowercase().contains(&query))
        .then_some(MatchRank::Description)
}

/// One rendered line in the results list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaletteRow {
    /// Non-selectable group heading.
    Caption(&'static str),
    /// Selectable row; the payload is the position in `matches`.
    Item(usize),
}

/// Stateful command palette modal over typed resources.
///
/// Tracks the query and cursor, the ranked or grouped result rows, the
/// selected result, the session-local recent list, and modal visibility.
/// Provides `render(&mut Frame)` rather than implementing `Widget` so it can
/// place the terminal cursor inside the input.
pub struct CommandPalette {
    locale: crate::localization::Locale,
    panel_title: Option<String>,
    registry: Arc<CommandRegistry>,
    resources: Vec<PaletteResource>,
    /// Current search query
    query: String,
    /// Query text used to build the current rows; detects query changes.
    searched_query: String,
    /// Cursor position in query
    cursor: usize,
    /// Selectable resource indices in display order
    matches: Vec<usize>,
    /// Rendered rows: captions interleaved with `matches`
    rows: Vec<PaletteRow>,
    /// Selected index into `matches`
    selected: usize,
    /// Stable ids of resources confirmed this session, most recent first
    recent: Vec<String>,
    /// Whether the modal is visible
    visible: bool,
    /// List state for scrolling
    list_state: ListState,
}

impl CommandPalette {
    /// Create a new command palette
    #[must_use]
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        let resources = command_resources(&registry);
        Self {
            locale: crate::localization::Locale::English,
            registry,
            resources,
            query: String::new(),
            searched_query: String::new(),
            panel_title: None,
            cursor: 0,
            matches: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            recent: Vec::new(),
            visible: false,
            list_state: ListState::default(),
        }
    }

    /// Update the registry
    pub fn update_registry(&mut self, registry: Arc<CommandRegistry>) {
        if self.panel_title.is_none() {
            self.resources
                .retain(|resource| resource.kind != PaletteResourceKind::Command);
            self.resources.extend(command_resources(&registry));
        }
        self.registry = registry;
        self.search();
    }

    /// Replace the searchable resources. The recent list is kept and re-resolved
    /// against the new resources by stable id.
    pub fn set_resources(&mut self, resources: Vec<PaletteResource>) {
        self.panel_title = None;
        self.resources = resources;
        self.search();
    }

    /// Show the modal and reset search state.
    ///
    /// Clears the query, resets cursor and selection, and performs an initial
    /// search to populate the browse view.
    pub fn show(&mut self) {
        self.visible = true;
        self.query.clear();
        self.cursor = 0;
        self.selected = 0;
        self.reset_scroll();
        self.search();
    }

    /// Browse a bounded set of actions without executing or persisting a choice.
    pub fn show_panel(&mut self, title: String, resources: Vec<PaletteResource>) {
        self.resources = resources;
        self.panel_title = Some(title);
        self.show();
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

    /// Insert a character at the cursor position and update search results.
    ///
    /// Handles unicode characters correctly by using character byte length.
    pub fn insert_char(&mut self, c: char) {
        self.query.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.search();
    }

    /// Insert a string at the cursor position (e.g. pasted text).
    pub fn insert_str(&mut self, s: &str) {
        self.query.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.search();
    }

    /// Delete character before cursor
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.query[..self.cursor]
                .chars()
                .last()
                .map_or(0, char::len_utf8);
            self.query.remove(self.cursor - prev);
            self.cursor -= prev;
            self.search();
        }
    }

    /// Move cursor left
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            let prev = self.query[..self.cursor]
                .chars()
                .last()
                .map_or(0, char::len_utf8);
            self.cursor -= prev;
        }
    }

    /// Move cursor right
    pub fn move_right(&mut self) {
        if self.cursor < self.query.len() {
            let next = self.query[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
            self.cursor += next;
        }
    }

    /// Move selection up
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.sync_list_state();
        }
    }

    /// Move selection down
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            self.sync_list_state();
        }
    }

    /// Get the selected resource
    #[must_use]
    pub fn selected_resource(&self) -> Option<&PaletteResource> {
        self.matches
            .get(self.selected)
            .and_then(|index| self.resources.get(*index))
    }

    /// Confirm the selected resource, remember it as recent, hide the modal,
    /// and return it.
    ///
    /// Returns `None` if the results list is empty.
    pub fn confirm(&mut self) -> Option<PaletteResource> {
        let resource = self.selected_resource().cloned();
        if let Some(resource) = &resource {
            self.remember_recent(resource);
        }
        self.hide();
        resource
    }

    fn remember_recent(&mut self, resource: &PaletteResource) {
        if resource.kind == PaletteResourceKind::Command {
            return;
        }
        let stable_id = resource.stable_id();
        self.recent.retain(|id| *id != stable_id);
        self.recent.insert(0, stable_id);
        self.recent.truncate(RECENT_LIMIT);
    }

    /// Rebuild `matches` and `rows` for the current query.
    ///
    /// A changed query resets selection and scroll to the top. An unchanged
    /// query (resource or registry refresh) keeps the selection while it is
    /// still in range.
    pub fn set_locale(&mut self, locale: crate::localization::Locale) {
        if self.locale != locale {
            self.locale = locale;
            self.search();
        }
    }

    fn search(&mut self) {
        crate::localization::with_locale(self.locale, || self.search_in_locale());
    }

    fn search_in_locale(&mut self) {
        let (filter, text) = parse_filter(&self.query);
        let (matches, rows) = if self.panel_title.is_some() {
            let matches: Vec<usize> = self
                .resources
                .iter()
                .enumerate()
                .filter(|(_, resource)| resource.matches(&self.query))
                .map(|(index, _)| index)
                .collect();
            let rows = (0..matches.len()).map(PaletteRow::Item).collect();
            (matches, rows)
        } else if text.trim().is_empty() {
            self.browse_rows(filter)
        } else {
            let matches = self.ranked_matches(filter, text);
            let rows = (0..matches.len()).map(PaletteRow::Item).collect();
            (matches, rows)
        };
        self.matches = matches;
        self.rows = rows;

        if self.searched_query != self.query {
            self.searched_query.clone_from(&self.query);
            self.selected = 0;
            self.reset_scroll();
        } else if self.selected >= self.matches.len() {
            self.selected = 0;
        }
        self.sync_list_state();
    }

    /// Flat ranked results for a typed query: exact, then prefix, then other
    /// name matches, then description-only matches. Ranking happens over every
    /// candidate before `RESULT_LIMIT` is applied.
    fn ranked_matches(&self, filter: Option<PaletteResourceKind>, text: &str) -> Vec<usize> {
        let mut ranked: Vec<(MatchRank, usize)> = self
            .resources
            .iter()
            .enumerate()
            .filter(|(_, resource)| filter.is_none_or(|kind| resource.kind == kind))
            .filter_map(|(index, resource)| rank_resource(resource, text).map(|rank| (rank, index)))
            .collect();
        ranked.sort_by_key(|(rank, index)| (*rank, *index));
        ranked
            .into_iter()
            .map(|(_, index)| index)
            .take(RESULT_LIMIT)
            .collect()
    }

    /// Grouped rows for an empty query: recent, common commands, then one
    /// group per kind.
    fn browse_rows(&self, filter: Option<PaletteResourceKind>) -> (Vec<usize>, Vec<PaletteRow>) {
        let mut matches: Vec<usize> = Vec::new();
        let mut rows: Vec<PaletteRow> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        let in_filter =
            |resource: &PaletteResource| filter.is_none_or(|kind| resource.kind == kind);

        let mut push_group = |caption: &'static str, indices: Vec<usize>, cap: usize| {
            let mut added = 0;
            for index in indices {
                if added >= cap || matches.len() >= RESULT_LIMIT {
                    break;
                }
                if !seen.insert(index) {
                    continue;
                }
                if added == 0 {
                    rows.push(PaletteRow::Caption(caption));
                }
                rows.push(PaletteRow::Item(matches.len()));
                matches.push(index);
                added += 1;
            }
        };

        let recent: Vec<usize> = self
            .recent
            .iter()
            .filter_map(|stable_id| {
                self.resources.iter().position(|resource| {
                    in_filter(resource)
                        && resource.kind != PaletteResourceKind::Command
                        && resource.stable_id() == *stable_id
                })
            })
            .collect();
        push_group(maestro_ui::localization::tr("Recent"), recent, RECENT_LIMIT);

        let primary = self.registry.primary_commands();
        let common: Vec<usize> = primary
            .iter()
            .filter_map(|name| {
                self.resources.iter().position(|resource| {
                    in_filter(resource)
                        && resource.kind == PaletteResourceKind::Command
                        && resource.id == name.name
                })
            })
            .collect();
        push_group(
            maestro_ui::localization::tr("Commands"),
            common,
            primary.len(),
        );

        let group_cap = if filter.is_some() {
            RESULT_LIMIT
        } else {
            GROUP_PREVIEW_LIMIT
        };
        for kind in KIND_ORDER {
            // Advanced and compatibility commands remain reachable by typing.
            if kind == PaletteResourceKind::Command || filter.is_some_and(|wanted| wanted != kind) {
                continue;
            }
            let indices: Vec<usize> = self
                .resources
                .iter()
                .enumerate()
                .filter(|(_, resource)| resource.kind == kind)
                .map(|(index, _)| index)
                .collect();
            push_group(kind_caption(kind), indices, group_cap);
        }

        (matches, rows)
    }

    fn reset_scroll(&mut self) {
        *self.list_state.offset_mut() = 0;
    }

    /// Point the list state at the row that renders the selected match.
    fn sync_list_state(&mut self) {
        let row = self
            .rows
            .iter()
            .position(|row| *row == PaletteRow::Item(self.selected));
        self.list_state.select(row);
    }

    /// Render the command palette modal to the frame.
    ///
    /// Does nothing if the modal is not visible.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.visible || area.width < 4 || area.height < 4 {
            return;
        }

        let theme = crate::themes::current_ui_theme();
        // The palette keeps its existing exclusive presentation over the composer.
        frame.render_widget(Clear, area);
        frame
            .buffer_mut()
            .set_style(area, crate::themes::current_theme().canvas_style());
        let inner = Modal::sized(
            self.panel_title
                .as_deref()
                .unwrap_or(maestro_ui::localization::tr("Search")),
            ModalSize::Wide,
        )
        .theme(theme)
        .render(frame, area);
        let area = inner;
        let label_width = self.label_column_width(area.width);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| match row {
                PaletteRow::Caption(caption) => ListItem::new(Line::from(Span::styled(
                    format!("  {caption}"),
                    theme.muted_style().add_modifier(Modifier::BOLD),
                ))),
                PaletteRow::Item(position) => {
                    let resource = &self.resources[self.matches[*position]];
                    if self.panel_title.is_some() {
                        return render_panel_resource(
                            resource,
                            *position == self.selected,
                            area.width.saturating_sub(2),
                            theme,
                        );
                    }
                    render_resource(
                        resource,
                        *position == self.selected,
                        area.width.saturating_sub(2),
                        label_width,
                        theme,
                    )
                }
            })
            .collect();

        Picker::new(
            &self.query,
            if self.panel_title.is_some() {
                maestro_ui::localization::tr("Filter actions…")
            } else {
                maestro_ui::localization::tr("Search commands, files, sessions…")
            },
            items,
            theme,
        )
        .cursor(self.cursor)
        .empty(if self.query.is_empty() {
            maestro_ui::localization::tr("Type to search resources...")
        } else {
            maestro_ui::localization::tr("No matching resources")
        })
        .help(if self.panel_title.is_some() {
            key_hints(
                &[
                    KeyHint::new("Enter", maestro_ui::localization::tr("select / apply")),
                    KeyHint::new("Esc", maestro_ui::localization::tr("close unchanged")),
                ],
                theme,
            )
        } else {
            footer_hint(inner.width, theme)
        })
        .render(frame, inner, &mut self.list_state);
    }

    /// Label column width: wide enough for the widest visible label, clamped
    /// so descriptions keep room on narrow modals.
    fn label_column_width(&self, area_width: u16) -> usize {
        let widest = self
            .matches
            .iter()
            .filter_map(|index| self.resources.get(*index))
            .map(|resource| resource.label.width())
            .max()
            .unwrap_or(0);
        // Two columns go to the selection marker; the label never takes more
        // than half of the rest so the description column stays visible.
        let available = usize::from(area_width).saturating_sub(2);
        let half = available.div_ceil(2).max(MIN_LABEL_WIDTH);
        widest
            .clamp(MIN_LABEL_WIDTH, MAX_LABEL_WIDTH)
            .min(half)
            .min(available)
    }
}

/// Keep selection and cancellation visible before optional resource shortcuts.
fn footer_hint(width: u16, theme: UiTheme) -> Line<'static> {
    let mut hints = vec![
        KeyHint::new("Enter", maestro_ui::localization::tr("select")),
        KeyHint::new("Esc", maestro_ui::localization::tr("cancel")),
    ];
    if width >= 74 {
        hints.extend([
            KeyHint::new(">", maestro_ui::localization::tr("cmd")),
            KeyHint::new("@", maestro_ui::localization::tr("file")),
            KeyHint::new("#", maestro_ui::localization::tr("session")),
            KeyHint::new(":", maestro_ui::localization::tr("model")),
            KeyHint::new("%", maestro_ui::localization::tr("theme")),
        ]);
    } else if width >= 40 {
        hints.push(KeyHint::new("↑↓", maestro_ui::localization::tr("navigate")));
    }
    key_hints(&hints, theme)
}

/// Settings actions use a full-width label so permission choices stay legible.
fn render_panel_resource(
    resource: &PaletteResource,
    selected: bool,
    width: u16,
    theme: UiTheme,
) -> ListItem<'static> {
    let mut lines = vec![Line::from(Span::styled(
        palette_ellipsis(&resource.label, usize::from(width)),
        if selected {
            Style::default()
                .fg(theme.focus)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.text_style()
        },
    ))];
    if let Some(description) = resource.display_description() {
        lines.push(Line::from(Span::styled(
            palette_ellipsis(description, usize::from(width)),
            theme.muted_style(),
        )));
    }
    ListItem::new(lines).style(if selected {
        theme.selection_style()
    } else {
        Style::default()
    })
}

fn render_resource(
    resource: &PaletteResource,
    selected: bool,
    width: u16,
    label_width: usize,
    theme: UiTheme,
) -> ListItem<'static> {
    let mut spans = Vec::new();
    let label = palette_ellipsis(&resource.label, label_width);
    spans.push(Span::styled(
        format!(
            "{}{}",
            label,
            " ".repeat(label_width.saturating_sub(label.width()))
        ),
        Style::default()
            .fg(if selected { theme.focus } else { theme.text })
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    ));

    if let Some(status) = &resource.status {
        spans.push(Span::styled(
            format!("  [{status}]"),
            Style::default().fg(theme.focus),
        ));
    }
    if let Some(description) = resource.display_description() {
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        let remaining = usize::from(width).saturating_sub(used + 3);
        if remaining > 0 {
            spans.push(Span::styled(
                format!("  {}", palette_ellipsis(description, remaining)),
                Style::default().fg(theme.muted),
            ));
        }
    }

    let style = if selected {
        theme.selection_style()
    } else {
        Style::default()
    };

    ListItem::new(Line::from(spans)).style(style)
}

fn palette_ellipsis(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let size = character.width().unwrap_or(0);
        if used + size > width - 1 {
            break;
        }
        result.push(character);
        used += size;
    }
    result.push('…');
    result
}

fn command_resources(registry: &CommandRegistry) -> Vec<PaletteResource> {
    let mut commands = registry.all();
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    commands
        .into_iter()
        .map(|command| {
            PaletteResource::new(
                PaletteResourceKind::Command,
                command.name.clone(),
                format!("/{}", command.name),
            )
            .localized_description(command.description.clone(), command.localized_description)
            .search_terms(command.aliases.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::build_command_registry;
    use ratatui::{Terminal, backend::TestBackend};

    fn palette() -> CommandPalette {
        CommandPalette::new(Arc::new(build_command_registry()))
    }

    fn command(name: &str, description: &str) -> PaletteResource {
        PaletteResource::new(PaletteResourceKind::Command, name, format!("/{name}"))
            .description(description)
    }

    fn file(path: &str) -> PaletteResource {
        PaletteResource::new(PaletteResourceKind::File, path, path)
    }

    fn panel_action(id: &str, label: &str) -> PaletteResource {
        PaletteResource::new(PaletteResourceKind::Command, id, label)
    }

    fn match_ids(palette: &CommandPalette) -> Vec<&str> {
        palette
            .matches
            .iter()
            .map(|index| palette.resources[*index].id.as_str())
            .collect()
    }

    fn captions(palette: &CommandPalette) -> Vec<&'static str> {
        palette
            .rows
            .iter()
            .filter_map(|row| match row {
                PaletteRow::Caption(caption) => Some(*caption),
                PaletteRow::Item(_) => None,
            })
            .collect()
    }

    fn rendered_text(palette: &mut CommandPalette, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| palette.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn command_palette_selection_keeps_description_visible() {
        let mut palette = palette();
        palette.set_resources(vec![command("help", "Show help")]);
        palette.show();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| palette.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let theme = crate::themes::current_ui_theme();
        let selected_row = (0..buffer.area.height)
            .find(|&y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("› /help")
            })
            .expect("selected command row");
        let text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, selected_row)].symbol())
            .collect();
        assert!(text.contains("Show help"));
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, selected_row)];
            if cell.symbol() != " " && cell.modifier.contains(Modifier::BOLD) {
                assert_eq!(cell.bg, theme.selection.unwrap_or(theme.surface));
                if cell.fg != ratatui::style::Color::Reset
                    && cell.bg != ratatui::style::Color::Reset
                {
                    assert_ne!(cell.fg, cell.bg);
                }
            }
        }
    }

    #[test]
    fn command_palette_resource_and_footer_use_supplied_semantic_palette() {
        use ratatui::{buffer::Buffer, style::Color, widgets::Widget};
        for (surface, text, muted, focus) in [
            (Color::Black, Color::White, Color::Gray, Color::Cyan),
            (Color::White, Color::Black, Color::DarkGray, Color::Blue),
        ] {
            let theme = UiTheme {
                surface,
                text,
                muted,
                focus,
                ..UiTheme::default()
            };
            let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 2));
            ratatui::widgets::List::new(vec![render_resource(
                &command("help", "Show help"),
                true,
                80,
                10,
                theme,
            )])
            .style(theme.text_style())
            .render(Rect::new(0, 0, 80, 1), &mut buffer);
            assert_eq!(buffer[(0, 0)].fg, focus);
            assert_eq!(buffer[(0, 0)].bg, surface);
            assert_ne!(buffer[(0, 0)].fg, buffer[(0, 0)].bg);
            assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[(12, 0)].fg, muted);
            footer_hint(80, theme).render(Rect::new(0, 1, 80, 1), &mut buffer);
            assert_eq!(buffer[(0, 1)].fg, focus);
            assert_eq!(buffer[(6, 1)].fg, muted);
            let footer: String = (0..80).map(|x| buffer[(x, 1)].symbol()).collect();
            assert!(footer.contains("Enter select · Esc cancel"));
            assert!(footer.contains("% theme"));
        }
    }

    #[test]
    fn command_palette_fits_small_terminals() {
        let mut palette = palette();
        palette.show();
        for (width, height) in [(3, 3), (20, 8), (40, 12), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| palette.render(frame, frame.area()))
                .unwrap();
        }
    }

    #[test]
    fn registry_refresh_preserves_open_panel_actions_and_selection() {
        let mut palette = palette();
        palette.show_panel(
            "Permissions".to_owned(),
            vec![
                panel_action("approvals safe", "Apply: ask for every tool"),
                panel_action("approvals yolo", "Apply: auto-approve tools"),
            ],
        );
        palette.move_down();
        palette.update_registry(Arc::new(build_command_registry()));
        assert_eq!(palette.panel_title.as_deref(), Some("Permissions"));
        assert_eq!(palette.matches.len(), 2);
        assert_eq!(palette.selected_resource().unwrap().id, "approvals yolo");
        palette.move_up();
        assert_eq!(palette.selected_resource().unwrap().id, "approvals safe");
    }

    #[test]
    fn panel_resources_keep_permission_labels_readable_at_wide_widths() {
        let mut palette = palette();
        palette.show_panel(
            "Settings".to_owned(),
            vec![
                panel_action("ask", "Apply: ask for every tool"),
                panel_action("auto", "Apply: auto-approve tools"),
            ],
        );
        let text = rendered_text(&mut palette, 80, 20);
        assert!(text.contains("Apply: ask for every tool"), "{text}");
        assert!(text.contains("Apply: auto-approve tools"), "{text}");

        palette.show_panel(
            "Settings".to_owned(),
            vec![panel_action("connections", "Connections and capabilities")],
        );
        let text = rendered_text(&mut palette, 120, 20);
        assert!(text.contains("Connections and capabilities"), "{text}");
        assert!(!text.contains("Connections and capabilit…"), "{text}");
    }

    #[test]
    fn command_palette_basics() {
        let mut palette = palette();
        palette.set_resources(vec![command("help", "Show help"), file("src/main.rs")]);

        assert!(!palette.is_visible());

        palette.show();
        assert!(palette.is_visible());
        assert!(!palette.matches.is_empty());

        for c in "help".chars() {
            palette.insert_char(c);
        }
        assert_eq!(palette.selected_resource().unwrap().id, "help");

        palette.hide();
        assert!(!palette.is_visible());
    }

    #[test]
    fn insert_str_inserts_at_cursor() {
        let mut palette = palette();
        palette.show();
        palette.insert_str("help");
        palette.move_left();
        palette.move_left();
        palette.insert_str("--");
        assert_eq!(palette.query, "he--lp");
        assert_eq!(palette.cursor, 4);
    }

    #[test]
    fn navigation() {
        let mut palette = palette();
        palette.set_resources(vec![command("help", ""), file("src/main.rs")]);
        palette.show();

        assert!(!palette.matches.is_empty());
        assert_eq!(palette.selected, 0);
        palette.move_down();
        assert_eq!(palette.selected, 1);
        palette.move_up();
        assert_eq!(palette.selected, 0);
        palette.move_up();
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn typed_prefix_filters_resources() {
        let mut palette = palette();
        palette.set_resources(vec![
            command("model", ""),
            PaletteResource::new(PaletteResourceKind::Model, "gpt-4o", "GPT-4o"),
        ]);
        palette.show();
        palette.insert_char(':');
        assert_eq!(palette.matches.len(), 1);
        assert_eq!(
            palette.selected_resource().unwrap().kind,
            PaletteResourceKind::Model
        );
    }

    #[test]
    fn browse_query_lists_primary_registry_commands_first() {
        let mut palette = palette();
        palette.set_resources(vec![
            command("about", "About"),
            command("compact", "Compact context"),
            command("help", "Show help"),
            command("model", "Pick model"),
            file("src/main.rs"),
        ]);
        palette.show();

        assert_eq!(
            match_ids(&palette),
            vec!["model", "help", "src/main.rs"],
            "primary commands come first in registry browse order; compatibility commands stay hidden"
        );
        assert_eq!(captions(&palette), vec!["Commands", "Files"]);
        assert_eq!(
            palette.selected_resource().unwrap().id,
            maestro_ui::localization::tr("model")
        );

        // Compatibility commands are still available through typed search.
        palette.insert_str("compact");
        assert_eq!(match_ids(&palette), vec!["compact"]);
        assert!(captions(&palette).is_empty());
    }

    #[test]
    fn browse_query_previews_each_kind_and_expands_with_filter() {
        let mut palette = palette();
        let files: Vec<PaletteResource> = (0..10).map(|i| file(&format!("f{i}.rs"))).collect();
        palette.set_resources(files);
        palette.show();
        assert_eq!(palette.matches.len(), GROUP_PREVIEW_LIMIT);
        assert_eq!(captions(&palette), vec!["Files"]);

        palette.insert_char('@');
        assert_eq!(palette.matches.len(), 10);
        assert_eq!(captions(&palette), vec!["Files"]);
    }

    #[test]
    fn typed_query_ranks_exact_and_prefix_before_description_across_limit() {
        let mut palette = palette();
        // Enough description-only matches to exceed RESULT_LIMIT before the
        // name matches appear in insertion order.
        let mut resources: Vec<PaletteResource> = (0..RESULT_LIMIT + 5)
            .map(|i| command(&format!("cmd{i}"), "Adjust the theme colors"))
            .collect();
        resources.push(command("themes", "List"));
        resources.push(command("theme", "Switch"));
        resources.push(command("set-theme", "Switch").search_terms(["theme".to_owned()]));
        palette.set_resources(resources);
        palette.show();
        palette.insert_str("theme");

        let ids = match_ids(&palette);
        assert_eq!(ids.len(), RESULT_LIMIT);
        assert_eq!(
            &ids[..3],
            &["theme", "set-theme", "themes"],
            "exact id, then exact alias, then prefix, all ahead of description-only hits"
        );
        assert!(ids[3..].iter().all(|id| id.starts_with("cmd")));
    }

    #[test]
    fn typed_query_keeps_incoming_order_within_a_rank() {
        let mut palette = palette();
        palette.set_resources(vec![
            command("git", "Git"),
            command("diff", "Show git diff"),
            command("goal", "Goal"),
        ]);
        palette.show();
        palette.insert_char('g');
        assert_eq!(match_ids(&palette), vec!["git", "goal", "diff"]);
    }

    #[test]
    fn typed_query_has_no_captions_and_resets_selection_and_scroll() {
        let mut palette = palette();
        let files: Vec<PaletteResource> = (0..20).map(|i| file(&format!("dir/a{i}.rs"))).collect();
        palette.set_resources(files);
        palette.show();
        palette.insert_char('@');
        for _ in 0..15 {
            palette.move_down();
        }
        assert_eq!(palette.selected, 15);
        // Render so the list state gains a scroll offset.
        rendered_text(&mut palette, 60, 12);
        assert!(palette.list_state.offset() > 0);

        palette.insert_char('a');
        assert!(captions(&palette).is_empty());
        assert_eq!(palette.selected, 0);
        assert_eq!(palette.list_state.offset(), 0);
        assert_eq!(palette.list_state.selected(), Some(0));
    }

    #[test]
    fn resource_refresh_with_same_query_keeps_selection() {
        let mut palette = palette();
        palette.set_resources(vec![
            command("new", ""),
            command("model", ""),
            command("help", ""),
        ]);
        palette.show();
        palette.move_down();
        assert_eq!(palette.selected, 1);
        palette.set_resources(vec![command("new", ""), command("model", "")]);
        assert_eq!(palette.selected, 1);
        palette.set_resources(vec![command("new", "")]);
        assert_eq!(palette.selected, 0, "out-of-range selection snaps to top");
    }

    #[test]
    fn panel_actions_do_not_evict_recent_files() {
        let mut palette = palette();
        let recent_file = file("src/recent.rs");
        palette.set_resources(vec![recent_file.clone()]);
        palette.show();
        palette.confirm();
        for index in 0..=RECENT_LIMIT {
            palette.show_panel(
                "Settings".to_owned(),
                vec![panel_action(&format!("action-{index}"), "Apply setting")],
            );
            palette.confirm();
        }
        palette.set_resources(vec![recent_file]);
        palette.show();
        assert_eq!(captions(&palette), vec!["Recent"]);
        assert_eq!(match_ids(&palette), vec!["src/recent.rs"]);
    }

    #[test]
    fn confirmed_resources_become_recent_and_survive_set_resources() {
        let mut palette = palette();
        let resources = vec![
            command("help", ""),
            command("model", ""),
            file("src/main.rs"),
        ];
        palette.set_resources(resources.clone());
        palette.show();
        palette.insert_str("main");
        let confirmed = palette.confirm().unwrap();
        assert_eq!(confirmed.id, "src/main.rs");
        assert!(!palette.is_visible());

        palette.set_resources(resources.clone());
        palette.show();
        assert_eq!(captions(&palette)[0], "Recent");
        assert_eq!(
            match_ids(&palette),
            vec!["src/main.rs", "model", "help"],
            "resource recents precede commands while primary command order stays fixed"
        );
        assert_eq!(
            match_ids(&palette)
                .iter()
                .filter(|id| **id == "src/main.rs")
                .count(),
            1,
            "a recent resource is not repeated in its kind group"
        );

        // Kind filters apply to the recent group too.
        palette.insert_char('>');
        assert_eq!(match_ids(&palette), vec!["model", "help"]);

        // Confirming a command does not promote it ahead of the primary order.
        palette.move_down();
        assert_eq!(palette.confirm().unwrap().id, "help");
        palette.set_resources(resources.clone());
        palette.show();
        assert_eq!(
            match_ids(&palette),
            vec!["src/main.rs", "model", "help"],
            "command recents do not reorder the Commands group"
        );

        // A recent resource that no longer exists is dropped silently.
        palette.set_resources(vec![command("help", "")]);
        palette.show();
        assert_eq!(captions(&palette), vec!["Commands"]);
    }

    #[test]
    fn recent_list_is_bounded_and_deduplicated_most_recent_first() {
        let mut palette = palette();
        let resources: Vec<PaletteResource> = (0..8).map(|i| file(&format!("f{i}.rs"))).collect();
        palette.set_resources(resources);
        for i in 0..8 {
            palette.show();
            palette.insert_str(&format!("f{i}.rs"));
            assert_eq!(palette.confirm().unwrap().id, format!("f{i}.rs"));
        }
        // Re-confirm f5 so it moves to the front instead of duplicating.
        palette.show();
        palette.insert_str("f5.rs");
        palette.confirm();
        assert_eq!(
            palette.recent,
            vec!["@:f5.rs", "@:f7.rs", "@:f6.rs", "@:f4.rs", "@:f3.rs"]
        );
        assert_eq!(palette.recent.len(), RECENT_LIMIT);

        palette.show();
        assert_eq!(
            &match_ids(&palette)[..RECENT_LIMIT],
            &["f5.rs", "f7.rs", "f6.rs", "f4.rs", "f3.rs"]
        );
    }

    #[test]
    fn grouped_navigation_skips_captions_and_highlights_selected_row() {
        let mut palette = palette();
        palette.set_resources(vec![command("help", "Show help"), file("src/main.rs")]);
        palette.show();
        assert_eq!(
            palette.rows,
            vec![
                PaletteRow::Caption("Commands"),
                PaletteRow::Item(0),
                PaletteRow::Caption("Files"),
                PaletteRow::Item(1),
            ]
        );
        assert_eq!(palette.list_state.selected(), Some(1));

        palette.move_down();
        assert_eq!(palette.selected, 1);
        assert_eq!(palette.selected_resource().unwrap().id, "src/main.rs");
        assert_eq!(
            palette.list_state.selected(),
            Some(3),
            "list row skips the caption"
        );

        palette.move_down();
        assert_eq!(palette.selected, 1, "clamped at the last selectable row");

        let text = rendered_text(&mut palette, 60, 14);
        assert!(text.contains("Commands"));
        assert!(text.contains("Files"));
        assert!(text.contains("› src/main.rs"));
        assert!(text.contains("  /help"));
    }

    #[test]
    fn label_column_narrows_to_visible_labels_and_keeps_description_room() {
        let mut palette = palette();
        palette.set_resources(vec![command("help", "Show help")]);
        palette.show();
        assert_eq!(palette.label_column_width(80), MIN_LABEL_WIDTH);

        palette.set_resources(vec![file("src/components/command_palette.rs")]);
        palette.show();
        assert_eq!(palette.label_column_width(80), MAX_LABEL_WIDTH);
        assert!(palette.label_column_width(24) <= 12);

        let text = rendered_text(&mut palette, 80, 12);
        assert!(text.contains("src/components/command_…"));
    }

    #[test]
    fn footer_shows_keyboard_hints_at_every_width() {
        let mut palette = palette();
        palette.set_resources(vec![command("help", "Show help")]);
        palette.show();

        let wide = rendered_text(&mut palette, 100, 20);
        assert!(wide.contains("Enter select"));
        assert!(wide.contains("Esc cancel"));
        assert!(wide.contains("@ file"));

        let medium = rendered_text(&mut palette, 60, 20);
        assert!(medium.contains("Enter select"));
        assert!(!medium.contains("@ file"));

        let narrow = rendered_text(&mut palette, 30, 20);
        assert!(narrow.contains("Enter"));
        assert!(narrow.contains("Esc"));
    }
}
