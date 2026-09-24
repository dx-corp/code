//! Session switcher modal
//!
//! Provides a UI for listing and switching between sessions.

use crate::search_cli::{SearchMatch, collect_documents, search_documents};
use maestro_ui::{KeyHint, Modal, ModalSize, NoticeTone, Picker, key_hints};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{ListItem, ListState},
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::session::{
    IndexedSession, SessionInfo, SessionManager, SessionMeta, SessionReadError, SessionStats,
    ThinkingLevel,
};

/// Session switcher modal state
pub struct SessionSwitcher {
    /// Session manager
    manager: SessionManager,
    /// Session index cache path (fast previews); None disables the fast path
    index_path: Option<PathBuf>,
    /// Available sessions
    sessions: Vec<SessionInfo>,
    /// Selected index
    selected: usize,
    /// Whether the modal is visible
    visible: bool,
    /// Filter query
    query: String,
    /// Filtered sessions
    filtered: Vec<usize>,
    /// Non-authoritative content results, computed through the existing search owner.
    content_matches: HashMap<String, SearchMatch>,
    content_pending: Option<std::sync::mpsc::Receiver<(String, Vec<SearchMatch>)>>,
    content_query: Option<String>,
    query_changed_at: Instant,
    branch_root: Option<String>,
    /// Loading state
    loading: bool,
    pending: Option<std::sync::mpsc::Receiver<Result<Vec<SessionInfo>, String>>>,
    /// Error message
    error: Option<String>,
    /// List state for scrolling
    list_state: ListState,
}

impl SessionSwitcher {
    /// Create a new session switcher
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            manager: SessionManager::new(cwd),
            index_path: crate::session::default_index_path(),
            sessions: Vec::new(),
            selected: 0,
            visible: false,
            query: String::new(),
            filtered: Vec::new(),
            content_matches: HashMap::new(),
            content_pending: None,
            content_query: None,
            query_changed_at: Instant::now(),
            branch_root: None,
            loading: false,
            pending: None,
            error: None,
            list_state: ListState::default(),
        }
    }

    /// Show the modal and load sessions
    pub fn show(&mut self) {
        self.visible = true;
        self.query.clear();
        self.content_matches.clear();
        self.content_query = None;
        self.branch_root = None;
        self.selected = 0;
        self.loading = true;
        self.error = None;
        self.refresh_async();
    }

    /// Keep a failed selection actionable without replacing the existing list.
    pub fn show_error(&mut self, error: String) {
        self.visible = true;
        self.pending = None;
        self.loading = false;
        self.error = Some(error);
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

    /// The content search has not caught up with the visible query.
    ///
    /// The 150 ms debounce and the worker thread both need another loop turn.
    #[must_use]
    pub fn content_search_pending(&self) -> bool {
        self.visible
            && !self.query.trim().is_empty()
            && self.content_query.as_deref() != Some(self.query.as_str())
    }

    /// Refresh metadata off the input thread, coalescing repeated opens.
    pub fn refresh_async(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let mut loader = Self::new(self.manager.cwd());
        loader.manager =
            SessionManager::with_sessions_dir(self.manager.cwd(), self.manager.sessions_dir());
        loader.index_path.clone_from(&self.index_path);
        self.begin_load(move || loader.load_sessions().map_err(|error| error.to_string()));
    }

    fn begin_load(
        &mut self,
        load: impl FnOnce() -> Result<Vec<SessionInfo>, String> + Send + 'static,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.loading = true;
        self.error = None;
        match std::thread::Builder::new()
            .name("maestro-session-list".into())
            .spawn(move || {
                let _ = tx.send(load());
                maestro_local_host::ui_wake::wake();
            }) {
            Ok(_) => self.pending = Some(rx),
            Err(error) => {
                self.loading = false;
                self.error = Some(error.to_string());
            }
        }
    }

    /// Adopt only this refresh's result, retaining search and stable selection.
    pub fn poll_refresh(&mut self) -> bool {
        let content_changed = self.poll_content_search();
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.pending else {
            return content_changed;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return content_changed,
            Err(TryRecvError::Disconnected) => {
                Err(maestro_ui::localization::tr("Session loading stopped. Try again.").to_owned())
            }
        };
        self.pending = None;
        self.loading = false;
        match result {
            Ok(sessions) => {
                let selected = self.selected_session().map(|session| session.id.clone());
                self.sessions = sessions;
                self.filter();
                if let Some(index) = self
                    .filtered
                    .iter()
                    .position(|&i| Some(&self.sessions[i].id) == selected.as_ref())
                {
                    self.selected = index;
                    self.list_state.select(Some(index));
                }
            }
            Err(error) => {
                self.error = Some(maestro_ui::localization::format(
                    "Failed to load sessions: {0}",
                    &[error],
                ));
            }
        }
        true
    }

    /// Refresh session list
    pub fn refresh(&mut self) {
        self.pending = None;
        match self.load_sessions() {
            Ok(sessions) => {
                self.sessions = sessions;
                self.loading = false;
                self.filter();
            }
            Err(e) => {
                self.error = Some(maestro_ui::localization::format(
                    "Failed to load sessions: {0}",
                    &[(e).to_string()],
                ));
                self.loading = false;
            }
        }
    }

    /// List sessions across all working directories, preferring the session
    /// index (cached previews, no per-open parsing) and falling back to header
    /// reads when the index yields nothing.
    fn load_sessions(&self) -> Result<Vec<SessionInfo>, SessionReadError> {
        let indexed = self.list_from_index();
        if !indexed.is_empty() {
            return Ok(indexed);
        }
        self.manager.list_all_sessions()
    }

    /// Read this directory's sessions from the shared session index. Returns
    /// an empty list when the index is unavailable or has no entries here.
    fn list_from_index(&self) -> Vec<SessionInfo> {
        let Some(index_path) = self.index_path.as_deref() else {
            return Vec::new();
        };
        let Some(root) = self.manager.sessions_dir().parent() else {
            return Vec::new();
        };
        crate::session::collect_sessions(root, Some(index_path))
            .iter()
            .map(indexed_session_info)
            .collect()
    }

    /// Insert a character in filter
    pub fn insert_char(&mut self, c: char) {
        self.query.push(c);
        self.query_changed();
    }

    /// Insert a string into the filter (e.g. pasted text).
    pub fn insert_str(&mut self, s: &str) {
        self.query.push_str(s);
        self.query_changed();
    }

    /// Delete character from filter
    pub fn backspace(&mut self) {
        self.query.pop();
        self.query_changed();
    }

    /// Clear filter
    pub fn clear_filter(&mut self) {
        self.query.clear();
        self.query_changed();
    }

    /// Filter sessions based on query
    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        let branch_root = self.branch_root.as_ref().map(|anchor| {
            self.sessions
                .iter()
                .find(|session| session.id == *anchor)
                .and_then(|session| self.branch_path(session).first().cloned())
                .unwrap_or_else(|| anchor.clone())
        });
        self.filtered = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                let in_branch = branch_root
                    .as_ref()
                    .is_none_or(|root| self.branch_path(session).first() == Some(root));
                in_branch
                    && (query.is_empty()
                        || session.id.to_lowercase().contains(&query)
                        || session.title().to_lowercase().contains(&query)
                        || session.cwd.to_lowercase().contains(&query)
                        || self.content_matches.contains_key(&session.id))
            })
            .map(|(index, _)| index)
            .collect();
        if self.branch_root.is_some() {
            let sessions = &self.sessions;
            let mut ordered = self.filtered.clone();
            ordered.sort_by_cached_key(|&index| self.branch_path(&sessions[index]));
            self.filtered = ordered;
        }
        // Reset selection if out of bounds
        if self.selected >= self.filtered.len() {
            self.selected = 0;
        }
        // Sync list state
        if self.filtered.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(self.selected));
        }
    }

    fn query_changed(&mut self) {
        self.content_matches.clear();
        self.content_query = None;
        self.query_changed_at = Instant::now();
        self.filter();
    }

    /// Switch between all sessions and the selected session's persisted fork family.
    pub fn toggle_branches(&mut self) {
        self.branch_root = if self.branch_root.is_some() {
            None
        } else {
            self.selected_session()
                .and_then(|session| self.branch_path(session).first().cloned())
        };
        self.clear_filter();
        self.selected = 0;
        self.filter();
    }

    /// Open a persisted family even while its metadata is loading.
    pub fn show_branches(&mut self, session_id: Option<&str>) {
        self.show();
        self.branch_root = session_id.map(str::to_owned);
        self.filter();
    }

    fn branch_path(&self, session: &SessionInfo) -> Vec<String> {
        let mut path = vec![session.id.clone()];
        let mut seen = HashSet::from([session.id.as_str()]);
        let mut parent = session.parent_session.as_deref();
        while let Some(id) = parent {
            if !seen.insert(id) {
                // Corrupt cyclic lineage must remain usable and cannot hang input.
                path.sort();
                return path;
            }
            path.push(id.to_owned());
            parent = self
                .sessions
                .iter()
                .find(|candidate| candidate.id == id)
                .and_then(|candidate| candidate.parent_session.as_deref());
        }
        path.reverse();
        path
    }

    fn poll_content_search(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;
        let mut changed = false;
        if let Some(rx) = &self.content_pending {
            match rx.try_recv() {
                Ok((query, results)) => {
                    self.content_pending = None;
                    if self.visible && query == self.query {
                        self.content_query = Some(query);
                        for result in results {
                            self.content_matches
                                .entry(result.session_id.clone())
                                .or_insert(result);
                        }
                        self.filter();
                    }
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.content_pending = None;
                    self.content_query = Some(self.query.clone());
                    self.error = Some(
                        maestro_ui::localization::tr("Conversation search stopped. Try again.")
                            .to_owned(),
                    );
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.visible
            && !self.query.trim().is_empty()
            && self.content_pending.is_none()
            && self.content_query.as_deref() != Some(&self.query)
            && self.query_changed_at.elapsed() >= Duration::from_millis(150)
        {
            let Some(root) = self.manager.sessions_dir().parent().map(PathBuf::from) else {
                return changed;
            };
            let cache = self
                .index_path
                .as_ref()
                .map(|path| path.with_file_name("search-index.json"));
            let query = self.query.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("maestro-session-search".into())
                .spawn(move || {
                    let documents = collect_documents(&root, cache.as_deref());
                    let results = search_documents(&documents, &query, None, documents.len());
                    let _ = tx.send((query, results));
                    maestro_local_host::ui_wake::wake();
                }) {
                Ok(_) => self.content_pending = Some(rx),
                Err(error) => {
                    self.content_query = Some(self.query.clone());
                    self.error = Some(error.to_string());
                }
            }
            changed = true;
        }
        changed
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
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
            self.list_state.select(Some(self.selected));
        }
    }

    /// Get the selected session
    #[must_use]
    pub fn selected_session(&self) -> Option<&SessionInfo> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.sessions.get(idx))
    }

    /// Confirm selection and return the session ID
    pub fn confirm(&mut self) -> Option<String> {
        let id = self.selected_session().map(|s| s.id.clone());
        self.hide();
        id
    }

    /// Select a session by stable ID, refreshing the list if necessary.
    pub fn select_by_id(&mut self, id: &str) -> bool {
        self.refresh();
        let Some(index) = self.sessions.iter().position(|session| session.id == id) else {
            return false;
        };
        self.filtered = vec![index];
        self.selected = 0;
        self.list_state.select(Some(0));
        true
    }

    /// Delete the selected session and its owned sidecar data.
    pub fn delete_selected(&mut self) -> Result<(), String> {
        if let Some(session) = self.selected_session().cloned() {
            self.manager.delete_session(&session).map_err(|e| {
                maestro_ui::localization::format(
                    "Failed to delete session: {0}",
                    &[(e).to_string()],
                )
            })?;
            self.refresh();
        }
        Ok(())
    }

    /// Render the modal
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.visible {
            return;
        }

        let theme = crate::themes::current_ui_theme();
        let count = if self.filtered.len() == self.sessions.len() {
            self.sessions.len().to_string()
        } else {
            format!("{}/{}", self.filtered.len(), self.sessions.len())
        };
        let title = maestro_ui::localization::format(
            if self.branch_root.is_some() {
                "Session branches ({0})"
            } else {
                "Sessions ({0})"
            },
            std::slice::from_ref(&count),
        );
        let inner = Modal::sized(title, ModalSize::Wide)
            .theme(theme)
            .render(frame, area);
        let items = self
            .filtered
            .iter()
            .filter_map(|&idx| self.sessions.get(idx))
            .map(|session| {
                let item = Self::render_session_item(session);
                if let Some(result) = self.content_matches.get(&session.id) {
                    let snippet: String = result
                        .snippet
                        .text
                        .chars()
                        .filter(|c| !c.is_control() || c.is_whitespace())
                        .collect();
                    let snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                    ListItem::new(vec![
                        Self::session_line(session),
                        Line::from(Span::styled(format!("  {snippet}"), theme.muted_style())),
                    ])
                } else {
                    item
                }
            })
            .collect();
        let compact = inner.width < 70;
        let hints = if compact {
            key_hints(
                &[
                    KeyHint::new("Enter", "select"),
                    KeyHint::new("Esc", "cancel"),
                ],
                theme,
            )
        } else {
            key_hints(
                &[
                    KeyHint::new("Ctrl+F", "branches"),
                    KeyHint::new("Enter", "select"),
                    KeyHint::new("Esc", "cancel"),
                    KeyHint::new("↑↓", "navigate"),
                    KeyHint::new("Del", "delete"),
                ],
                theme,
            )
        };
        let mut picker = Picker::new(
            &self.query,
            maestro_ui::localization::tr("Search names, messages, and tool results..."),
            items,
            theme,
        )
        .empty(if self.query.is_empty() {
            maestro_ui::localization::tr("No sessions found")
        } else {
            maestro_ui::localization::tr("No matching sessions")
        })
        .help(hints);
        if self.loading {
            picker = picker.notice(
                maestro_ui::localization::tr("Loading sessions..."),
                NoticeTone::Busy,
            );
        } else if self.content_pending.is_some() {
            picker = picker.notice(
                maestro_ui::localization::tr("Searching conversations..."),
                NoticeTone::Busy,
            );
        } else if let Some(error) = &self.error {
            picker = picker.notice(error.as_str(), NoticeTone::Error);
        }
        let mut picker_area = inner;
        if compact && inner.height > 1 {
            picker_area.height -= 1;
            frame.render_widget(
                ratatui::widgets::Paragraph::new(key_hints(
                    &[KeyHint::new("Ctrl+F", "branches")],
                    theme,
                )),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
        picker.render(frame, picker_area, &mut self.list_state);
    }

    fn render_session_item(session: &SessionInfo) -> ListItem<'static> {
        ListItem::new(Self::session_line(session))
    }

    fn session_line(session: &SessionInfo) -> Line<'static> {
        let theme = crate::themes::current_ui_theme();
        let mut spans = Vec::new();

        // Favorite indicator
        if session.is_favorite() {
            spans.push(Span::styled("★ ", Style::default().fg(theme.attention)));
        }

        if let Some(parent) = &session.parent_session {
            spans.push(Span::styled(
                maestro_ui::localization::format(
                    "Fork of {0} · ",
                    &[parent.chars().take(8).collect()],
                ),
                theme.muted_style(),
            ));
        }
        // Title
        let title: String = session.title().chars().take(30).collect();
        spans.push(Span::styled(
            title,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ));

        // Timestamp
        let time_str = format_relative_time(&session.timestamp);
        spans.push(Span::styled(format!("  {time_str}"), theme.muted_style()));

        // Message count
        spans.push(Span::styled(
            maestro_ui::localization::format(
                "  {0} msgs",
                &[(session.stats.total_messages()).to_string()],
            ),
            Style::default().fg(theme.focus),
        ));

        let cwd = session
            .cwd
            .trim_end_matches(std::path::MAIN_SEPARATOR)
            .rsplit(std::path::MAIN_SEPARATOR)
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("/");
        spans.push(Span::styled(format!("  [{cwd}]"), theme.muted_style()));

        Line::from(spans)
    }
}

impl Default for SessionSwitcher {
    fn default() -> Self {
        Self::new(".")
    }
}

#[cfg(test)]
mod continuity_tests {
    use super::*;
    use std::fmt::Write as _;

    fn write_session(
        dir: &std::path::Path,
        id: &str,
        parent: Option<&str>,
        role: &str,
        text: &str,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        let content = if role == "assistant" {
            serde_json::json!([{"type":"text","text":text}])
        } else {
            serde_json::json!(text)
        };
        let entries = [
            serde_json::json!({"type":"session","id":id,"parentSession":parent,"timestamp":"2024-01-15T10:30:00Z","cwd":"/project","model":"ollama/qwen","thinkingLevel":"medium"}),
            serde_json::json!({"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{"role":"user","content":"unrelated opening","timestamp":0}}),
            serde_json::json!({"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{"role":role,"toolCallId":"call-1","toolName":"bash","isError":false,"content":content,"timestamp":1}}),
        ];
        let mut encoded = String::new();
        for entry in entries {
            writeln!(encoded, "{entry}").unwrap();
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), encoded).unwrap();
    }

    fn switcher(dir: &std::path::Path) -> SessionSwitcher {
        let mut picker = SessionSwitcher::new("/project");
        picker.manager = SessionManager::with_sessions_dir("/project", dir);
        picker.index_path = None;
        picker.visible = true;
        picker.refresh();
        picker
    }

    #[test]
    fn content_search_finds_reply_and_tool_result_and_ignores_malformed_session() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions/project");
        write_session(
            &dir,
            "reply-session",
            None,
            "assistant",
            "A 日本語 needle only in the reply",
        );
        write_session(
            &dir,
            "tool-session",
            None,
            "toolResult",
            "Another 日本語 needle from the tool",
        );
        std::fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();
        let mut picker = switcher(&dir);
        picker.insert_str("日本語");
        assert!(
            picker.filtered.is_empty(),
            "metadata alone cannot match later content"
        );
        picker.query_changed_at = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.content_query.is_none() {
            picker.poll_refresh();
            assert!(Instant::now() < deadline, "content search did not complete");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(picker.filtered.len(), 2);
        assert!(
            picker.content_matches["reply-session"]
                .snippet
                .text
                .contains("日本語")
        );
        assert!(
            picker.content_matches["tool-session"]
                .snippet
                .text
                .contains("日本語")
        );
    }

    #[test]
    fn stale_content_results_do_not_replace_a_new_query() {
        let mut picker = SessionSwitcher::new("/project");
        picker.visible = true;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        picker.content_pending = Some(rx);
        picker.insert_str("new query");
        tx.send((
            "old query".into(),
            vec![SearchMatch {
                session_id: "old".into(),
                project: "/project".into(),
                timestamp_ms: 1,
                kind: crate::search_cli::SearchKind::Assistant,
                score: 1,
                snippet: crate::search_cli::Snippet {
                    text: "old query".into(),
                    highlights: vec![],
                },
            }],
        ))
        .unwrap();
        picker.poll_refresh();
        assert_eq!(picker.query, "new query");
        assert!(picker.content_matches.is_empty());
    }

    #[test]
    fn branch_navigation_uses_persisted_lineage_and_keeps_orphans_accessible() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions/project");
        write_session(&dir, "parent", None, "assistant", "parent reply");
        write_session(&dir, "child", Some("parent"), "assistant", "child reply");
        write_session(
            &dir,
            "grandchild",
            Some("child"),
            "assistant",
            "grandchild reply",
        );
        write_session(&dir, "orphan", Some("missing"), "assistant", "orphan reply");
        let mut picker = switcher(&dir);
        picker.branch_root = Some("child".into());
        picker.filter();
        let ids: Vec<_> = picker
            .filtered
            .iter()
            .map(|&index| picker.sessions[index].id.as_str())
            .collect();
        assert_eq!(ids, ["parent", "child", "grandchild"]);
        picker.branch_root = Some("orphan".into());
        picker.filter();
        assert_eq!(picker.selected_session().unwrap().id, "orphan");
        picker.toggle_branches();
        assert_eq!(picker.filtered.len(), 4);
    }

    #[test]
    fn cyclic_lineage_cannot_hang_the_picker() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions/project");
        write_session(&dir, "a", Some("b"), "assistant", "reply");
        write_session(&dir, "b", Some("a"), "assistant", "reply");
        let mut picker = switcher(&dir);
        picker.branch_root = Some("a".into());
        picker.filter();
        assert_eq!(picker.filtered.len(), 2);
    }
}

/// Map a session-index entry onto the switcher's list model.
///
/// The index stores neither the model nor the per-role message breakdown, so
/// `model` is left empty and the total count sits in `user_messages` — only
/// `stats.total_messages()` is rendered here, and both fields are repopulated
/// from the full file when a session is actually resumed.
fn indexed_session_info(indexed: &IndexedSession) -> SessionInfo {
    let entry = &indexed.entry;
    SessionInfo {
        parent_session: entry.parent_session.clone(),
        id: entry.id.clone(),
        path: indexed.path.clone(),
        cwd: entry.cwd.clone(),
        model: String::new(),
        thinking_level: ThinkingLevel::default(),
        timestamp: entry.started_at.clone(),
        stats: SessionStats {
            user_messages: entry.message_count,
            ..SessionStats::default()
        },
        meta: Some(SessionMeta {
            title: entry.title.clone(),
            favorite: entry.favorite,
            ..SessionMeta::default()
        }),
        preview: entry.preview.clone(),
        modified: Some(UNIX_EPOCH + Duration::from_millis(indexed.modified_ms)),
    }
}

/// Format a timestamp relative to now
fn format_relative_time(timestamp: &str) -> String {
    // Try to parse ISO timestamp
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        let now = chrono::Utc::now();
        let duration = now.signed_duration_since(dt.with_timezone(&chrono::Utc));

        if duration.num_minutes() < 1 {
            return maestro_ui::localization::tr("just now").to_string();
        } else if duration.num_hours() < 1 {
            let mins = duration.num_minutes();
            return maestro_ui::localization::format("{0}m ago", &[(mins).to_string()]);
        } else if duration.num_days() < 1 {
            let hours = duration.num_hours();
            return maestro_ui::localization::format("{0}h ago", &[(hours).to_string()]);
        } else if duration.num_days() < 7 {
            let days = duration.num_days();
            return maestro_ui::localization::format("{0}d ago", &[(days).to_string()]);
        } else if duration.num_weeks() < 4 {
            let weeks = duration.num_weeks();
            return maestro_ui::localization::format("{0}w ago", &[(weeks).to_string()]);
        }
    }

    // Fall back to raw timestamp
    timestamp.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait_for_refresh(switcher: &mut SessionSwitcher) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while switcher.pending.is_some() && std::time::Instant::now() < deadline {
            switcher.poll_refresh();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!switcher.loading);
    }

    #[test]
    fn failed_history_refresh_retains_the_existing_selection() {
        let mut switcher = SessionSwitcher::new("/tmp");
        switcher.sessions.push(SessionInfo {
            parent_session: None,
            id: "saved".into(),
            path: "/tmp/saved.jsonl".into(),
            cwd: "/tmp".into(),
            model: "ollama/qwen".into(),
            thinking_level: ThinkingLevel::default(),
            timestamp: String::new(),
            stats: SessionStats::default(),
            meta: None,
            preview: Some("existing conversation".into()),
            modified: None,
        });
        switcher.filter();
        switcher.begin_load(|| Err("disk unavailable".into()));
        wait_for_refresh(&mut switcher);
        assert_eq!(switcher.selected_session().unwrap().id, "saved");
        assert!(
            switcher
                .error
                .as_deref()
                .unwrap()
                .contains("disk unavailable")
        );
    }

    #[test]
    fn slow_history_loading_allows_filtering_and_dismissal() {
        let mut switcher = SessionSwitcher::new("/tmp");
        let (release, gate) = std::sync::mpsc::channel();
        switcher.begin_load(move || {
            gate.recv().unwrap();
            Ok(Vec::new())
        });
        switcher.show(); // must coalesce rather than replace the blocked loader
        switcher.insert_str("my draft");
        assert!(!switcher.poll_refresh());
        assert_eq!(switcher.query, "my draft");
        switcher.hide();
        assert!(!switcher.is_visible());
        release.send(()).unwrap();
        wait_for_refresh(&mut switcher);
        assert_eq!(switcher.query, "my draft");
        assert!(!switcher.is_visible());
    }

    #[test]
    fn session_switcher_notices_use_busy_and_error_colors() {
        use ratatui::{Terminal, backend::TestBackend};
        let theme = crate::themes::current_ui_theme();
        let mut selector = SessionSwitcher::new(".");
        selector.visible = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        for (loading, error, message, color) in [
            (true, None, "Loading sessions...", theme.focus),
            (
                false,
                Some("Session read failed".to_owned()),
                "Session read failed",
                theme.error,
            ),
        ] {
            selector.loading = loading;
            selector.error = error;
            terminal
                .draw(|frame| selector.render(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = (0..buffer.area.height)
                .find(|&y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains(message)
                })
                .expect("notice visible");
            let first = (0..buffer.area.width)
                .find(|&x| buffer[(x, row)].symbol() == &message[..1])
                .unwrap();
            assert_eq!(buffer[(first, row)].fg, color);
            assert_eq!(buffer[(first, row)].bg, theme.surface);
        }
    }

    #[test]
    fn session_switcher_shared_picker_renders_empty_query_result_and_help() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut selector = SessionSwitcher::new(".");
        selector.visible = true;
        selector.query = "no-such-result-zzz".into();
        selector.filtered.clear();
        selector.loading = false;
        let before = selector.query.clone();
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
        assert!(text.contains("No matching sessions"));
        assert!(text.contains("Del delete"));
        assert_eq!(selector.query, before);
    }

    #[test]
    fn format_relative_time_works() {
        // Just ensure it doesn't panic
        let _ = format_relative_time("2024-01-15T10:30:00Z");
        let _ = format_relative_time("invalid");
    }

    #[test]
    fn insert_str_appends_to_filter() {
        let mut switcher = SessionSwitcher::new("/tmp");
        switcher.insert_str("fix(tui)");
        switcher.insert_char('!');
        assert_eq!(switcher.query, "fix(tui)!");
    }

    #[test]
    fn session_switcher_basics() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions/project");
        let mut switcher = SessionSwitcher::new("/tmp");
        switcher.manager = SessionManager::with_sessions_dir("/tmp", &sessions);
        switcher.index_path = Some(temp.path().join("session-index.json"));
        assert!(!switcher.is_visible());

        switcher.show();
        wait_for_refresh(&mut switcher);
        assert!(switcher.is_visible());
        assert!(switcher.error.is_none());
        assert!(switcher.sessions.is_empty());

        switcher.hide();
        assert!(!switcher.is_visible());
    }

    #[test]
    fn refresh_reads_previews_from_session_index() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let dir = root.join("--tmp--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2024-01-15T10-30-00-000Z_session-1.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        writeln!(
            file,
            r#"{{"type":"session","id":"session-1","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"untangle the resume flow","timestamp":0}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-15T10:30:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"On it."}}],"timestamp":1}}}}"#
        )
        .unwrap();
        drop(file);

        let index_path = temp.path().join("session-index.json");
        let mut switcher = SessionSwitcher::new("/tmp");
        switcher.manager = SessionManager::with_sessions_dir("/tmp", &dir);
        switcher.index_path = Some(index_path.clone());

        switcher.show();
        wait_for_refresh(&mut switcher);

        assert_eq!(switcher.sessions.len(), 1);
        let session = &switcher.sessions[0];
        // The model is only filled by the header-read fallback; an empty model
        // proves this row came from the index fast path.
        assert!(session.model.is_empty());
        assert_eq!(session.stats.total_messages(), 2);
        assert_eq!(session.title(), "untangle the resume flow");
        assert!(
            index_path.exists(),
            "refresh persisted the session index for the next open"
        );
        let spill_dir = dir.join("tool-output/session-1");
        std::fs::create_dir_all(&spill_dir).unwrap();
        std::fs::write(spill_dir.join("large.txt"), "output").unwrap();

        // Deleting the session prunes its transcript, spill data, list row,
        // and index entry together.
        switcher.delete_selected().unwrap();
        assert!(switcher.sessions.is_empty());
        assert!(!spill_dir.exists());
        let raw = std::fs::read_to_string(&index_path).unwrap();
        assert!(!raw.contains("session-1"));
    }

    #[test]
    fn refresh_falls_back_to_header_reads_when_index_is_empty() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let dir = root.join("--tmp--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2024-01-15T10-30-00-000Z_session-2.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        writeln!(
            file,
            r#"{{"type":"session","id":"session-2","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"fallback path","timestamp":0}}}}"#
        )
        .unwrap();
        drop(file);

        let mut switcher = SessionSwitcher::new("/tmp");
        switcher.manager = SessionManager::with_sessions_dir("/tmp", &dir);
        // Index disabled: the header-read fallback must still list the session.
        switcher.index_path = None;

        switcher.show();
        wait_for_refresh(&mut switcher);

        assert_eq!(switcher.sessions.len(), 1);
        assert_eq!(switcher.sessions[0].model, "openai/gpt-5.2");
    }

    #[test]
    fn refresh_lists_sessions_across_workspaces() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let first = root.join("workspace-one");
        let second = root.join("workspace-two");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let write_session = |dir: &std::path::Path, id: &str, cwd: &str| {
            let path = dir.join(format!("2024-01-15T10-30-00-000Z_{id}.jsonl"));
            let mut file = std::fs::File::create(path).unwrap();
            use std::io::Write;
            writeln!(
                file,
                r#"{{"type":"session","id":"{id}","timestamp":"2024-01-15T10:30:00Z","cwd":"{cwd}","model":"openai/gpt-5.2","thinkingLevel":"medium"}}"#
            )
            .unwrap();
            writeln!(
                file,
                r#"{{"type":"message","timestamp":"2024-01-15T10:30:01Z","message":{{"role":"user","content":"{id}","timestamp":0}}}}"#
            )
            .unwrap();
        };
        write_session(&first, "session-one", "/tmp/one");
        write_session(&second, "session-two", "/tmp/two");

        let mut switcher = SessionSwitcher::new("/tmp/one");
        switcher.manager = SessionManager::with_sessions_dir("/tmp/one", &first);
        switcher.index_path = None;

        switcher.show();
        wait_for_refresh(&mut switcher);

        assert_eq!(switcher.sessions.len(), 2);
        switcher.insert_str("/tmp/two");
        assert_eq!(switcher.filtered.len(), 1);
        assert_eq!(switcher.selected_session().unwrap().id, "session-two");
    }
}
