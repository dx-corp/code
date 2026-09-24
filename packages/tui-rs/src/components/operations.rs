//! Read-only view of tool executions persisted in recent sessions.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    time::SystemTime,
};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Map, Value};

use crate::agent::{
    ExecutionPhase, ExecutionReceipt, ExecutionSource, ExecutionStatus, ToolReceiptDetails,
    credential_store::redact_credentials_in_json,
};
use crate::session::{AppMessage, ContentBlock, ParsedSession, SessionManager, SessionReader};
use crate::tools::CoordinationSnapshot;
use crate::tools::ToolDetails;
use crate::tools::background_tasks::MonitorEvent;

const RECENT_SESSION_LIMIT: usize = 20;
const ROW_LIMIT: usize = 200;
const STRING_LIMIT: usize = 160;
const COLLECTION_LIMIT: usize = 12;
const VALUE_DEPTH_LIMIT: usize = 3;
const NARROW_WIDTH: u16 = 100;
const SCROLL_STEP: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptSummary {
    pub source: &'static str,
    pub status: &'static str,
    pub phase: Option<&'static str>,
    pub duration_ms: Option<u64>,
    pub detail_kind: &'static str,
    pub detail: Option<String>,
}

impl ReceiptSummary {
    fn from_receipt(receipt: &ExecutionReceipt) -> Self {
        let (status, phase) = match receipt.status {
            ExecutionStatus::Succeeded => ("succeeded", None),
            ExecutionStatus::Failed => (maestro_ui::localization::tr("failed"), None),
            ExecutionStatus::Denied => ("denied", None),
            ExecutionStatus::Cancelled { phase } => (
                maestro_ui::localization::tr("cancelled"),
                Some(match phase {
                    ExecutionPhase::Queued => maestro_ui::localization::tr("queued"),
                    ExecutionPhase::Running => maestro_ui::localization::tr("running"),
                }),
            ),
            ExecutionStatus::Indeterminate => ("indeterminate", None),
        };
        let source = match receipt.source {
            ExecutionSource::Native => "native",
            ExecutionSource::RemoteClient => maestro_ui::localization::tr("remote client"),
            ExecutionSource::Cache => "cache",
        };
        let (detail_kind, detail) = match &receipt.details {
            ToolReceiptDetails::BuiltIn(details) => (built_in_kind(details), None),
            ToolReceiptDetails::Mcp {
                server,
                tool,
                is_error,
            } => (
                "mcp",
                Some(format!(
                    "server={} tool={} error={is_error}",
                    bounded_text(server, STRING_LIMIT),
                    bounded_text(tool, STRING_LIMIT)
                )),
            ),
            ToolReceiptDetails::Origin(origin) => {
                ("origin", Some(bounded_text(origin, STRING_LIMIT)))
            }
            ToolReceiptDetails::FeedbackDraft { .. } => {
                (maestro_ui::localization::tr("feedback draft"), None)
            }
            ToolReceiptDetails::Cached => ("cached", None),
            ToolReceiptDetails::None => (maestro_ui::localization::tr("none"), None),
        };
        Self {
            source,
            status,
            phase,
            duration_ms: receipt.duration_ms,
            detail_kind,
            detail,
        }
    }
}

fn built_in_kind(details: &ToolDetails) -> &'static str {
    match details {
        ToolDetails::Bash(_) => "bash",
        ToolDetails::Read(_) => "read",
        ToolDetails::Write(_) => "write",
        ToolDetails::Edit(_) => "edit",
        ToolDetails::Image(_) => "image",
        ToolDetails::WebFetch(_) => maestro_ui::localization::tr("web fetch"),
        ToolDetails::Glob(_) => "glob",
        ToolDetails::Grep(_) => "grep",
        ToolDetails::Diff(_) => "diff",
        ToolDetails::List(_) => "list",
        ToolDetails::InlineTool(_) => maestro_ui::localization::tr("inline tool"),
        ToolDetails::Batch(_) => "batch",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    pub session_id: String,
    pub session_title: String,
    pub session_cwd: String,
    pub session_timestamp: String,
    pub call_id: String,
    pub tool_name: String,
    pub task_args: Option<String>,
    pub timestamp_ms: u64,
    pub result_status: &'static str,
    pub receipt: Option<ReceiptSummary>,
}

impl OperationRow {
    fn display_status(&self) -> &'static str {
        self.receipt
            .as_ref()
            .map_or(self.result_status, |receipt| receipt.status)
    }
}

/// Project requests and results into one row per call ID within a session.
#[must_use]
pub fn project_session(session: &ParsedSession) -> Vec<OperationRow> {
    let mut rows: Vec<OperationRow> = Vec::new();
    let mut by_call_id = HashMap::<String, usize>::new();
    let session_title = session.title();

    for message in &session.messages {
        match message {
            AppMessage::Assistant {
                content, timestamp, ..
            } => {
                for block in content {
                    let ContentBlock::ToolCall { id, name, args, .. } = block else {
                        continue;
                    };
                    let row = OperationRow {
                        session_id: bounded_text(&session.header.id, STRING_LIMIT),
                        session_title: bounded_text(&session_title, STRING_LIMIT),
                        session_cwd: bounded_text(&session.header.cwd, STRING_LIMIT),
                        session_timestamp: bounded_text(&session.header.timestamp, STRING_LIMIT),
                        call_id: bounded_text(id, STRING_LIMIT),
                        tool_name: bounded_text(name, STRING_LIMIT),
                        task_args: Some(format_bounded_value(args)),
                        timestamp_ms: *timestamp,
                        result_status: maestro_ui::localization::tr("pending"),
                        receipt: None,
                    };
                    if let Some(index) = by_call_id.get(id).copied() {
                        let existing = &mut rows[index];
                        existing.tool_name = row.tool_name;
                        existing.task_args = row.task_args;
                        if row.timestamp_ms != 0 {
                            existing.timestamp_ms = row.timestamp_ms;
                        }
                    } else {
                        by_call_id.insert(id.clone(), rows.len());
                        rows.push(row);
                    }
                }
            }
            AppMessage::ToolResult {
                tool_call_id,
                tool_name,
                receipt,
                is_error,
                timestamp,
                ..
            } => {
                let summary = receipt.as_ref().map(ReceiptSummary::from_receipt);
                if let Some(index) = by_call_id.get(tool_call_id).copied() {
                    let row = &mut rows[index];
                    row.result_status = if *is_error {
                        maestro_ui::localization::tr("failed")
                    } else {
                        "succeeded"
                    };
                    row.receipt = summary;
                    if row.timestamp_ms == 0 {
                        row.timestamp_ms = *timestamp;
                    }
                } else {
                    by_call_id.insert(tool_call_id.clone(), rows.len());
                    rows.push(OperationRow {
                        session_id: bounded_text(&session.header.id, STRING_LIMIT),
                        session_title: bounded_text(&session_title, STRING_LIMIT),
                        session_cwd: bounded_text(&session.header.cwd, STRING_LIMIT),
                        session_timestamp: bounded_text(&session.header.timestamp, STRING_LIMIT),
                        call_id: bounded_text(tool_call_id, STRING_LIMIT),
                        tool_name: bounded_text(tool_name, STRING_LIMIT),
                        task_args: None,
                        timestamp_ms: *timestamp,
                        result_status: if *is_error {
                            maestro_ui::localization::tr("failed")
                        } else {
                            "succeeded"
                        },
                        receipt: summary,
                    });
                }
            }
            AppMessage::User { .. } => {}
        }
    }
    rows
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    [
        "authorization",
        "cookie",
        "credential",
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "apikey",
        "private_key",
    ]
    .iter()
    .any(|candidate| key.contains(candidate))
}

fn bounded_value(value: &Value, depth: usize) -> Value {
    if depth >= VALUE_DEPTH_LIMIT {
        return Value::String("[truncated]".to_string());
    }
    match value {
        Value::String(text) => Value::String(bounded_text(text, STRING_LIMIT)),
        Value::Array(values) => {
            let mut bounded: Vec<_> = values
                .iter()
                .take(COLLECTION_LIMIT)
                .map(|value| bounded_value(value, depth + 1))
                .collect();
            if values.len() > COLLECTION_LIMIT {
                bounded.push(Value::String("[truncated]".to_string()));
            }
            Value::Array(bounded)
        }
        Value::Object(values) => {
            let mut bounded = Map::new();
            for (key, value) in values.iter().take(COLLECTION_LIMIT) {
                bounded.insert(
                    bounded_text(key, STRING_LIMIT),
                    if is_sensitive_key(key) {
                        Value::String("[redacted]".to_string())
                    } else {
                        bounded_value(value, depth + 1)
                    },
                );
            }
            if values.len() > COLLECTION_LIMIT {
                bounded.insert("...".to_string(), Value::String("[truncated]".to_string()));
            }
            Value::Object(bounded)
        }
        scalar => scalar.clone(),
    }
}

fn format_bounded_value(value: &Value) -> String {
    serde_json::to_string_pretty(&bounded_value(value, 0)).unwrap_or_else(|_| "{}".to_string())
}

fn bounded_text(text: &str, limit: usize) -> String {
    let redacted = redacted_text(text);
    let mut output: String = redacted.chars().take(limit).collect();
    if redacted.chars().count() > limit {
        output.push_str("...");
    }
    output
}

fn redacted_text(text: &str) -> String {
    match redact_credentials_in_json(&Value::String(text.to_string())) {
        Value::String(redacted) => redacted,
        _ => String::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusedPane {
    Session,
    Task,
    Receipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationsView {
    Operations,
    Agents,
}

impl FocusedPane {
    fn next(self) -> Self {
        match self {
            Self::Session => Self::Task,
            Self::Task => Self::Receipt,
            Self::Receipt => Self::Session,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Session => Self::Receipt,
            Self::Task => Self::Session,
            Self::Receipt => Self::Task,
        }
    }
}

pub struct OperationsModal {
    manager: SessionManager,
    rows: Vec<OperationRow>,
    agents: Vec<CoordinationSnapshot>,
    selected: usize,
    agent_selected: usize,
    list_state: ListState,
    agent_list_state: ListState,
    focus: FocusedPane,
    view: OperationsView,
    visible: bool,
    loading: bool,
    load_rx: Option<Receiver<OperationsLoad>>,
    error: Option<String>,
    parse_failures: Vec<String>,
    task_scroll: u16,
    receipt_scroll: u16,
}

struct OperationsLoad {
    rows: Vec<OperationRow>,
    agents: Vec<CoordinationSnapshot>,
    parse_failures: Vec<String>,
    error: Option<String>,
}

fn load_operations(manager: &SessionManager) -> OperationsLoad {
    let sessions = match recent_session_paths(manager.sessions_dir()) {
        Ok(sessions) => sessions,
        Err(error) => {
            return OperationsLoad {
                rows: Vec::new(),
                agents: Vec::new(),
                parse_failures: Vec::new(),
                error: Some(maestro_ui::localization::format(
                    "Failed to load operations: {0}",
                    &[bounded_text(&error.to_string(), STRING_LIMIT).clone()],
                )),
            };
        }
    };

    let mut rows = Vec::new();
    let mut parse_failures = Vec::new();
    for path in sessions {
        match SessionReader::read_file(&path) {
            Ok(session) => {
                retain_recent_rows(&mut rows, project_session(&session));
            }
            Err(error) => parse_failures.push(format!(
                "{}: {}",
                bounded_text(
                    &path.file_name().unwrap_or_default().to_string_lossy(),
                    STRING_LIMIT
                ),
                bounded_text(&error.to_string(), STRING_LIMIT)
            )),
        }
    }
    retain_recent_rows(
        &mut rows,
        crate::tools::background_tasks::monitor_event_history()
            .iter()
            .map(monitor_event_row)
            .collect(),
    );

    let agents = crate::tools::coordination_snapshots(Path::new(manager.cwd())).unwrap_or_default();
    OperationsLoad {
        rows,
        agents,
        parse_failures,
        error: None,
    }
}

fn monitor_event_row(event: &MonitorEvent) -> OperationRow {
    OperationRow {
        session_id: "background".to_string(),
        session_title: maestro_ui::localization::format(
            "Task {0}",
            &[(bounded_text(&event.task_id, 24)).clone()],
        ),
        session_cwd: String::new(),
        session_timestamp: String::new(),
        call_id: event.monitor_id.clone(),
        tool_name: "monitor".to_string(),
        task_args: Some(format!("{}: {}", event.stream, event.output)),
        timestamp_ms: event.timestamp_ms,
        result_status: "succeeded",
        receipt: None,
    }
}

fn recent_session_paths(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }

    let mut recent: Vec<(Option<SystemTime>, PathBuf)> = Vec::with_capacity(RECENT_SESSION_LIMIT);
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        recent.push((modified, path));
        recent.sort_unstable_by(|(left, _), (right, _)| match (right, left) {
            (Some(right), Some(left)) => right.cmp(left),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        recent.truncate(RECENT_SESSION_LIMIT);
    }
    Ok(recent.into_iter().map(|(_, path)| path).collect())
}

fn retain_recent_rows(rows: &mut Vec<OperationRow>, projected: Vec<OperationRow>) {
    rows.extend(projected);
    rows.sort_unstable_by(|left, right| {
        right
            .timestamp_ms
            .cmp(&left.timestamp_ms)
            .then_with(|| right.session_timestamp.cmp(&left.session_timestamp))
            .then_with(|| right.call_id.cmp(&left.call_id))
    });
    let mut seen = HashSet::new();
    rows.retain(|row| {
        seen.insert((
            row.session_id.clone(),
            row.call_id.clone(),
            row.timestamp_ms,
            row.tool_name.clone(),
            row.task_args.clone(),
        ))
    });
    rows.truncate(ROW_LIMIT);
}

impl OperationsModal {
    #[must_use]
    pub fn new(cwd: impl Into<String>) -> Self {
        Self::with_manager(SessionManager::new(cwd))
    }

    fn with_manager(manager: SessionManager) -> Self {
        Self {
            manager,
            rows: Vec::new(),
            agents: Vec::new(),
            selected: 0,
            agent_selected: 0,
            list_state: ListState::default(),
            agent_list_state: ListState::default(),
            focus: FocusedPane::Session,
            view: OperationsView::Operations,
            visible: false,
            loading: false,
            load_rx: None,
            error: None,
            parse_failures: Vec::new(),
            task_scroll: 0,
            receipt_scroll: 0,
        }
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.selected = 0;
        self.agent_selected = 0;
        self.focus = FocusedPane::Session;
        self.task_scroll = 0;
        self.receipt_scroll = 0;
        self.refresh();
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn refresh(&mut self) {
        if self.loading {
            return;
        }
        let cwd = self.manager.cwd().to_string();
        let sessions_dir = self.manager.sessions_dir().to_path_buf();
        let (tx, rx) = sync_channel(1);
        self.loading = true;
        self.error = None;
        self.parse_failures.clear();
        self.rows.clear();
        self.agents.clear();
        self.sync_selection();
        self.load_rx = Some(rx);
        std::thread::spawn(move || {
            let manager = SessionManager::with_sessions_dir(cwd, sessions_dir);
            let _ = tx.send(load_operations(&manager));
            maestro_local_host::ui_wake::wake();
        });
    }

    pub fn add_monitor_event(&mut self, event: &MonitorEvent) {
        retain_recent_rows(&mut self.rows, vec![monitor_event_row(event)]);
        self.sync_selection();
    }

    /// Apply a completed background load without blocking the TUI loop.
    pub fn poll_load(&mut self) -> bool {
        let result = match self.load_rx.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(Ok(result)),
            Some(Err(TryRecvError::Disconnected)) => Some(Err(())),
            Some(Err(TryRecvError::Empty)) | None => None,
        };
        let Some(result) = result else {
            return false;
        };

        self.load_rx = None;
        self.loading = false;
        match result {
            Ok(result) => {
                retain_recent_rows(&mut self.rows, result.rows);
                self.agents = result.agents;
                self.parse_failures = result.parse_failures;
                self.error = result.error;
                if self.rows.is_empty() && !self.parse_failures.is_empty() {
                    self.focus = FocusedPane::Receipt;
                }
            }
            Err(()) => {
                self.error = Some(
                    maestro_ui::localization::tr("Failed to load operations: loader stopped")
                        .to_string(),
                );
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.agent_selected = self.agent_selected.min(self.agents.len().saturating_sub(1));
        self.task_scroll = 0;
        self.receipt_scroll = 0;
        self.sync_selection();
        true
    }

    pub fn move_up(&mut self) {
        match self.view {
            OperationsView::Operations => self.selected = self.selected.saturating_sub(1),
            OperationsView::Agents => {
                self.agent_selected = self.agent_selected.saturating_sub(1);
            }
        }
        self.reset_pane_scroll();
        self.sync_selection();
    }

    pub fn move_down(&mut self) {
        match self.view {
            OperationsView::Operations if self.selected + 1 < self.rows.len() => {
                self.selected += 1;
            }
            OperationsView::Agents if self.agent_selected + 1 < self.agents.len() => {
                self.agent_selected += 1;
            }
            _ => {}
        }
        self.reset_pane_scroll();
        self.sync_selection();
    }

    pub fn select_first(&mut self) {
        match self.view {
            OperationsView::Operations => self.selected = 0,
            OperationsView::Agents => self.agent_selected = 0,
        }
        self.reset_pane_scroll();
        self.sync_selection();
    }

    pub fn select_last(&mut self) {
        match self.view {
            OperationsView::Operations => self.selected = self.rows.len().saturating_sub(1),
            OperationsView::Agents => {
                self.agent_selected = self.agents.len().saturating_sub(1);
            }
        }
        self.reset_pane_scroll();
        self.sync_selection();
    }

    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    pub fn focus_previous(&mut self) {
        self.focus = self.focus.previous();
    }

    pub fn scroll_up(&mut self) {
        match self.focus {
            FocusedPane::Task => self.task_scroll = self.task_scroll.saturating_sub(SCROLL_STEP),
            FocusedPane::Receipt => {
                self.receipt_scroll = self.receipt_scroll.saturating_sub(SCROLL_STEP);
            }
            FocusedPane::Session => {}
        }
    }

    pub fn scroll_down(&mut self) {
        match self.focus {
            FocusedPane::Task => self.task_scroll = self.task_scroll.saturating_add(SCROLL_STEP),
            FocusedPane::Receipt => {
                self.receipt_scroll = self.receipt_scroll.saturating_add(SCROLL_STEP);
            }
            FocusedPane::Session => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn scroll_offsets(&self) -> (u16, u16) {
        (self.task_scroll, self.receipt_scroll)
    }

    fn reset_pane_scroll(&mut self) {
        self.task_scroll = 0;
        self.receipt_scroll = 0;
    }

    fn sync_selection(&mut self) {
        self.list_state
            .select((!self.rows.is_empty()).then_some(self.selected));
        self.agent_list_state
            .select((!self.agents.is_empty()).then_some(self.agent_selected));
    }

    pub fn toggle_view(&mut self) {
        self.view = match self.view {
            OperationsView::Operations => OperationsView::Agents,
            OperationsView::Agents => OperationsView::Operations,
        };
        self.focus = FocusedPane::Session;
        self.reset_pane_scroll();
    }

    #[must_use]
    pub fn selected_agent_id(&self) -> Option<&str> {
        (self.view == OperationsView::Agents)
            .then(|| self.agents.get(self.agent_selected))
            .flatten()
            .map(|agent| agent.subagent_id.as_str())
    }

    #[must_use]
    pub fn selected_held_control_id(&self) -> Option<&str> {
        (self.view == OperationsView::Agents)
            .then(|| self.agents.get(self.agent_selected))
            .flatten()
            .and_then(|agent| agent.held_control_id.as_deref())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_with_theme(frame, area, crate::themes::current_ui_theme());
    }

    fn render_with_theme(&mut self, frame: &mut Frame, area: Rect, theme: maestro_ui::UiTheme) {
        if !self.visible {
            return;
        }
        let width = area.width.saturating_sub(2).clamp(1, 140);
        let height = area.height.saturating_sub(2).clamp(1, 36);
        let modal_area = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, modal_area);
        let title = if self.view == OperationsView::Agents {
            format!(" Agents ({}) ", self.agents.len())
        } else if self.parse_failures.is_empty() {
            format!(" Operations ({}) ", self.rows.len())
        } else {
            maestro_ui::localization::format(
                " Operations ({0}, {1} load failures) ",
                &[
                    (self.rows.len()).to_string(),
                    (self.parse_failures.len()).to_string(),
                ],
            )
        };
        let outer = Block::default()
            .title(title)
            .title_bottom(Line::styled(
                maestro_ui::localization::tr(" v operations/agents  Up/Down select  Left/Right pane  a approve  c cancel  r refresh  Esc close "), theme.muted_style(),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title_style(Style::default().fg(theme.focus).add_modifier(Modifier::BOLD))
            .style(theme.text_style());
        let inner = outer.inner(modal_area);
        frame.render_widget(outer, modal_area);

        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(error.as_str()).style(Style::default().fg(theme.error)),
                inner,
            );
            return;
        }
        if self.loading {
            frame.render_widget(
                Paragraph::new(maestro_ui::localization::tr(
                    "Loading persisted tool executions...",
                ))
                .style(Style::default().fg(theme.attention)),
                inner,
            );
            return;
        }
        if self.view == OperationsView::Agents {
            self.render_agents(frame, inner, theme);
            return;
        }
        if self.rows.is_empty() {
            if self.parse_failures.is_empty() {
                frame.render_widget(
                    Paragraph::new(maestro_ui::localization::tr(
                        "No persisted tool executions in recent sessions.",
                    ))
                    .style(Style::default().fg(theme.muted)),
                    inner,
                );
            } else {
                self.render_parse_failures(frame, inner, true, theme);
            }
            return;
        }

        if modal_area.width < NARROW_WIDTH {
            self.render_focused(frame, inner, theme);
        } else {
            let panes = Layout::horizontal([
                Constraint::Percentage(38),
                Constraint::Percentage(31),
                Constraint::Percentage(31),
            ])
            .split(inner);
            self.render_sessions(frame, panes[0], self.focus == FocusedPane::Session, theme);
            self.render_task(frame, panes[1], self.focus == FocusedPane::Task, theme);
            self.render_receipt(frame, panes[2], self.focus == FocusedPane::Receipt, theme);
        }
    }

    fn render_agents(&mut self, frame: &mut Frame, area: Rect, theme: maestro_ui::UiTheme) {
        if self.agents.is_empty() {
            frame.render_widget(
                Paragraph::new(maestro_ui::localization::tr(
                    "No delegated agents have been recorded.",
                ))
                .style(Style::default().fg(theme.muted)),
                area,
            );
            return;
        }
        if area.width < NARROW_WIDTH {
            match self.focus {
                FocusedPane::Session => self.render_agent_list(frame, area, true, theme),
                FocusedPane::Task => self.render_agent_status(frame, area, true, theme),
                FocusedPane::Receipt => self.render_agent_coordination(frame, area, true, theme),
            }
            return;
        }
        let panes = Layout::horizontal([
            Constraint::Percentage(38),
            Constraint::Percentage(31),
            Constraint::Percentage(31),
        ])
        .split(area);
        self.render_agent_list(frame, panes[0], self.focus == FocusedPane::Session, theme);
        self.render_agent_status(frame, panes[1], self.focus == FocusedPane::Task, theme);
        self.render_agent_coordination(frame, panes[2], self.focus == FocusedPane::Receipt, theme);
    }

    fn render_agent_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let items = self.agents.iter().map(|agent| {
            let status_color = match agent.status.as_str() {
                "running" | "queued" => theme.attention,
                "completed" => theme.success,
                _ => theme.error,
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<10}", bounded_text(&agent.role, 10)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {:<11}", agent.status),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!(" #{}", agent.attempt),
                    Style::default().fg(theme.muted),
                ),
            ]))
        });
        let list = List::new(items)
            .block(Self::pane_block(
                maestro_ui::localization::tr("Agents"),
                focused,
                theme,
            ))
            .style(theme.text_style())
            .highlight_style(theme.selection_style());
        frame.render_stateful_widget(list, area, &mut self.agent_list_state);
    }

    fn render_agent_status(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let agent = &self.agents[self.agent_selected];
        let elapsed_end = agent.finished_at_ms.unwrap_or_else(current_timestamp_ms);
        let elapsed = agent
            .started_at_ms
            .map(|start| elapsed_end.saturating_sub(start));
        let mut lines = vec![
            labelled_line(
                maestro_ui::localization::tr("Agent"),
                &agent.agent_ref,
                theme,
            ),
            labelled_line(maestro_ui::localization::tr("Role"), &agent.role, theme),
            labelled_line(maestro_ui::localization::tr("Status"), &agent.status, theme),
            labelled_line(
                maestro_ui::localization::tr("Attempt"),
                &agent.attempt.to_string(),
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Created"),
                &format_timestamp(agent.created_at_ms),
                theme,
            ),
        ];
        if let Some(elapsed) = elapsed {
            lines.push(labelled_line(
                maestro_ui::localization::tr("Elapsed"),
                &format!("{elapsed} ms"),
                theme,
            ));
        }
        if let Some(error) = &agent.error {
            lines.push(Line::from(""));
            lines.extend(raw_lines(error, Some(STRING_LIMIT)));
        }
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .block(Self::pane_block(
                    maestro_ui::localization::tr("Run"),
                    focused,
                    theme,
                ))
                .scroll((self.task_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_agent_coordination(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let agent = &self.agents[self.agent_selected];
        let mut lines = vec![
            labelled_line(
                maestro_ui::localization::tr("Parent"),
                &agent.parent_scope_id,
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Lifecycle"),
                if agent.lifecycle_published {
                    "published"
                } else {
                    maestro_ui::localization::tr("pending")
                },
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Last control"),
                agent
                    .last_control_id
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("none")),
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Mode"),
                agent
                    .last_control_mode
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("none")),
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Delivery"),
                agent
                    .last_control_state
                    .as_deref()
                    .unwrap_or(maestro_ui::localization::tr("none")),
                theme,
            ),
        ];
        if agent.held_control_id.is_some() {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                maestro_ui::localization::tr("Press a to approve the held control."),
                Style::default().fg(theme.attention),
            ));
        }
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .block(Self::pane_block(
                    maestro_ui::localization::tr("Coordination"),
                    focused,
                    theme,
                ))
                .scroll((self.receipt_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_focused(&mut self, frame: &mut Frame, area: Rect, theme: maestro_ui::UiTheme) {
        match self.focus {
            FocusedPane::Session => self.render_sessions(frame, area, true, theme),
            FocusedPane::Task => self.render_task(frame, area, true, theme),
            FocusedPane::Receipt => self.render_receipt(frame, area, true, theme),
        }
    }

    fn pane_block(title: &str, focused: bool, theme: maestro_ui::UiTheme) -> Block<'static> {
        Block::default()
            .title(format!(" {title} "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused { theme.focus } else { theme.border }))
            .style(theme.text_style())
    }

    fn render_sessions(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let items = self.rows.iter().map(|row| {
            let status_color = match row.display_status() {
                "succeeded" => theme.success,
                "pending" => theme.attention,
                _ => theme.error,
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<10}", bounded_text(&row.tool_name, 10)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {:<9}", row.display_status()),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!(" {}", bounded_text(&row.session_id, 10)),
                    Style::default().fg(theme.muted),
                ),
            ]))
        });
        let list = List::new(items)
            .block(Self::pane_block(
                maestro_ui::localization::tr("Session"),
                focused,
                theme,
            ))
            .style(theme.text_style())
            .highlight_style(theme.selection_style());
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn render_task(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let row = &self.rows[self.selected];
        let args = row
            .task_args
            .as_deref()
            .unwrap_or(maestro_ui::localization::tr("Not recorded"));
        let mut lines = vec![
            labelled_line(maestro_ui::localization::tr("Tool"), &row.tool_name, theme),
            labelled_line(maestro_ui::localization::tr("Call"), &row.call_id, theme),
            labelled_line(
                maestro_ui::localization::tr("Task time"),
                &format_timestamp(row.timestamp_ms),
                theme,
            ),
            Line::from(""),
            Line::styled(
                maestro_ui::localization::tr("Arguments"),
                Style::default().fg(theme.focus),
            ),
        ];
        lines.extend(raw_lines(args, None));
        let text = Text::from(lines);
        let block = Self::pane_block(maestro_ui::localization::tr("Task"), focused, theme);
        let pane = block.inner(area);
        let max_scroll = crate::wrapping::wrapped_line_count(&text, pane.width as usize)
            .saturating_sub(pane.height as usize)
            .min(u16::MAX as usize) as u16;
        self.task_scroll = self.task_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(block)
                .scroll((self.task_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_receipt(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let row = &self.rows[self.selected];
        let mut lines = vec![
            labelled_line(
                maestro_ui::localization::tr("Session"),
                &row.session_id,
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Title"),
                &row.session_title,
                theme,
            ),
            labelled_line(
                maestro_ui::localization::tr("Started"),
                &row.session_timestamp,
                theme,
            ),
            labelled_line(maestro_ui::localization::tr("Cwd"), &row.session_cwd, theme),
            Line::from(""),
        ];
        if let Some(receipt) = &row.receipt {
            lines.extend([
                labelled_line(
                    maestro_ui::localization::tr("Status"),
                    receipt.status,
                    theme,
                ),
                labelled_line(
                    maestro_ui::localization::tr("Source"),
                    receipt.source,
                    theme,
                ),
                labelled_line(
                    maestro_ui::localization::tr("Detail"),
                    receipt.detail_kind,
                    theme,
                ),
            ]);
            if let Some(phase) = receipt.phase {
                lines.push(labelled_line(
                    maestro_ui::localization::tr("Phase"),
                    phase,
                    theme,
                ));
            }
            if let Some(duration_ms) = receipt.duration_ms {
                lines.push(labelled_line(
                    maestro_ui::localization::tr("Duration"),
                    &format!("{duration_ms} ms"),
                    theme,
                ));
            }
            if let Some(detail) = &receipt.detail {
                lines.push(Line::from(""));
                lines.extend(raw_lines(detail, Some(STRING_LIMIT)));
            }
        } else {
            lines.push(labelled_line(
                maestro_ui::localization::tr("Status"),
                row.result_status,
                theme,
            ));
            lines.push(Line::styled(
                maestro_ui::localization::tr("No typed receipt persisted."),
                Style::default().fg(theme.muted),
            ));
        }
        if !self.parse_failures.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                maestro_ui::localization::tr("Session load failures"),
                Style::default().fg(theme.error),
            ));
            lines.extend(
                self.parse_failures
                    .iter()
                    .flat_map(|failure| raw_lines(failure, Some(STRING_LIMIT))),
            );
        }
        let text = Text::from(lines);
        let block = Self::pane_block(maestro_ui::localization::tr("Receipt"), focused, theme);
        let pane = block.inner(area);
        let max_scroll = crate::wrapping::wrapped_line_count(&text, pane.width as usize)
            .saturating_sub(pane.height as usize)
            .min(u16::MAX as usize) as u16;
        self.receipt_scroll = self.receipt_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(block)
                .scroll((self.receipt_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_parse_failures(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        theme: maestro_ui::UiTheme,
    ) {
        let text = Text::from(
            self.parse_failures
                .iter()
                .flat_map(|failure| raw_lines(failure, Some(STRING_LIMIT)))
                .collect::<Vec<_>>(),
        );
        let block = Self::pane_block(
            maestro_ui::localization::tr("Session load failures"),
            focused,
            theme,
        );
        let pane = block.inner(area);
        let max_scroll = crate::wrapping::wrapped_line_count(&text, pane.width as usize)
            .saturating_sub(pane.height as usize)
            .min(u16::MAX as usize) as u16;
        self.receipt_scroll = self.receipt_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .block(block)
                .scroll((self.receipt_scroll, 0))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(theme.error)),
            area,
        );
    }
}

fn labelled_line(label: &str, value: &str, theme: maestro_ui::UiTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(theme.focus)),
        Span::raw(bounded_text(value, STRING_LIMIT)),
    ])
}

fn raw_lines(value: &str, limit: Option<usize>) -> Vec<Line<'static>> {
    let value = limit.map_or_else(|| redacted_text(value), |limit| bounded_text(value, limit));
    if value.is_empty() {
        return vec![Line::raw("")];
    }
    value
        .lines()
        .map(|line| Line::raw(line.to_string()))
        .collect()
}

fn format_timestamp(timestamp_ms: u64) -> String {
    if timestamp_ms == 0 {
        return maestro_ui::localization::tr("Not recorded").to_string();
    }
    chrono::DateTime::from_timestamp_millis(timestamp_ms as i64).map_or_else(
        || timestamp_ms.to_string(),
        |timestamp| timestamp.to_rfc3339(),
    )
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn operations_and_agents_use_shared_theme() {
        for theme in crate::components::theme_test::palettes() {
            for width in [80, 120] {
                let mut modal = OperationsModal::new("/workspace");
                modal.visible = true;
                modal.rows = project_session(&session("session-a", call_and_result()));
                modal.agents = vec![CoordinationSnapshot {
                    subagent_id: "child-1".to_string(),
                    agent_ref: "subagent:child-1:2".to_string(),
                    parent_scope_id: "session:parent".to_string(),
                    role: "code".to_string(),
                    status: "running".to_string(),
                    attempt: 2,
                    created_at_ms: 1,
                    started_at_ms: Some(2),
                    finished_at_ms: None,
                    lifecycle_published: false,
                    last_control_id: Some("control-1".to_string()),
                    last_control_mode: Some("steer".to_string()),
                    last_control_state: Some("held".to_string()),
                    held_control_id: Some("control-1".to_string()),
                    error: None,
                }];
                modal.sync_selection();
                for view in [OperationsView::Operations, OperationsView::Agents] {
                    modal.view = view;
                    let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                    terminal
                        .draw(|frame| modal.render_with_theme(frame, frame.area(), theme))
                        .unwrap();
                    crate::components::theme_test::assert_palette(
                        terminal.backend().buffer(),
                        theme,
                    );
                }
            }
        }
    }

    use super::*;
    use crate::session::{MessageContent, SessionHeader, SessionStats};
    use ratatui::{Terminal, backend::TestBackend};
    use tempfile::tempdir;

    fn session(id: &str, messages: Vec<AppMessage>) -> ParsedSession {
        let header: SessionHeader = serde_json::from_value(serde_json::json!({
            "id": id,
            "timestamp": "2026-07-23T10:00:00Z",
            "cwd": "/workspace",
            "model": "test",
            "thinkingLevel": "off"
        }))
        .unwrap();
        ParsedSession {
            selective_summary_context: None,
            header,
            messages,
            meta: None,
            stats: SessionStats::default(),
            thinking_level_changes: Vec::new(),
            model_changes: Vec::new(),
            compactions: Vec::new(),
            session_events: Vec::new(),
            context_budget_snapshots: Vec::new(),
            lifecycle_notifications: Vec::new(),
            pending_lifecycle_agent_notes: Vec::new(),
            usage_entries: Vec::new(),
            side_questions: Vec::new(),
            plan_review_events: Vec::new(),
            file_path: String::new(),
        }
    }

    fn call_and_result() -> Vec<AppMessage> {
        vec![
            AppMessage::Assistant {
                content: vec![ContentBlock::ToolCall {
                    id: "shared-call".to_string(),
                    name: "read".to_string(),
                    args: serde_json::json!({"path": "README.md"}),
                    contract: None,
                    gemini_context: None,
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 10,
            },
            AppMessage::ToolResult {
                tool_call_id: "shared-call".to_string(),
                tool_name: "read".to_string(),
                content: "ok".to_string(),
                details: None,
                receipt: Some(ExecutionReceipt {
                    code_authority: None,
                    call_id: "shared-call".to_string(),
                    tool_name: "read".to_string(),
                    source: ExecutionSource::Native,
                    status: ExecutionStatus::Succeeded,
                    duration_ms: Some(12),
                    policy: None,
                    details: ToolReceiptDetails::None,
                }),
                is_error: false,
                timestamp: 11,
            },
        ]
    }

    #[test]
    fn pairs_calls_and_results_within_each_session() {
        let first = project_session(&session("session-a", call_and_result()));
        let second = project_session(&session("session-b", call_and_result()));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].session_id, "session-a");
        assert_eq!(first[0].display_status(), "succeeded");
        assert_eq!(second[0].session_id, "session-b");
    }

    #[test]
    fn retains_result_only_rows_and_typed_receipt_summary() {
        let rows = project_session(&session(
            "session-a",
            vec![AppMessage::ToolResult {
                tool_call_id: "call-1".to_string(),
                tool_name: "remote_read".to_string(),
                content: "cancelled".to_string(),
                details: None,
                receipt: Some(ExecutionReceipt {
                    code_authority: None,
                    call_id: "call-1".to_string(),
                    tool_name: "remote_read".to_string(),
                    source: ExecutionSource::RemoteClient,
                    status: ExecutionStatus::Cancelled {
                        phase: ExecutionPhase::Running,
                    },
                    duration_ms: Some(42),
                    policy: None,
                    details: ToolReceiptDetails::Mcp {
                        server: "filesystem".to_string(),
                        tool: "read".to_string(),
                        is_error: true,
                    },
                }),
                is_error: true,
                timestamp: 1,
            }],
        ));
        let receipt = rows[0].receipt.as_ref().unwrap();
        assert_eq!(receipt.status, "cancelled");
        assert_eq!(receipt.phase, Some("running"));
        assert_eq!(receipt.source, "remote client");
        assert_eq!(receipt.detail_kind, "mcp");
    }

    #[test]
    fn pairing_is_independent_of_message_order() {
        let mut messages = call_and_result();
        messages.reverse();
        let rows = project_session(&session("session-a", messages));

        assert_eq!(rows.len(), 1);
        assert!(rows[0].task_args.as_deref().unwrap().contains("README.md"));
        assert_eq!(rows[0].display_status(), "succeeded");
        assert!(rows[0].receipt.is_some());
    }

    #[test]
    fn task_arguments_are_redacted_and_bounded() {
        let rows = project_session(&session(
            "session-a",
            vec![AppMessage::Assistant {
                content: vec![ContentBlock::ToolCall {
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    args: serde_json::json!({
                        "authorization": "Bearer exposed",
                        "nested": {"apiKey": "exposed", "safe": "visible"},
                        "command": "curl -H 'Authorization: Bearer embedded-secret-token' example.test",
                        "long": "x".repeat(300)
                    }),
                    contract: None,
                    gemini_context: None,
                }],
                api: None,
                provider: None,
                model: None,
                usage: None,
                stop_reason: None,
                timestamp: 1,
            }],
        ));
        let args = rows[0].task_args.as_deref().unwrap();
        assert!(!args.contains("Bearer exposed"));
        assert!(!args.contains("exposed"));
        assert!(!args.contains("embedded-secret-token"));
        assert!(args.contains("[redacted]"));
        assert!(args.contains("portable-export"));
        assert!(args.contains("visible"));
        assert!(args.contains("..."));
    }

    #[test]
    fn session_metadata_and_receipt_details_use_pattern_redaction() {
        let mut parsed = session(
            "session-a",
            vec![
                AppMessage::User {
                    content: MessageContent::Text("Bearer title-secret-token-value".to_string()),
                    attachments: None,
                    timestamp: 1,
                },
                AppMessage::ToolResult {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "remote_read".to_string(),
                    content: "ok".to_string(),
                    details: None,
                    receipt: Some(ExecutionReceipt {
                        code_authority: None,
                        call_id: "call-1".to_string(),
                        tool_name: "remote_read".to_string(),
                        source: ExecutionSource::RemoteClient,
                        status: ExecutionStatus::Succeeded,
                        duration_ms: None,
                        policy: None,
                        details: ToolReceiptDetails::Mcp {
                            server: "Bearer server-secret-token-value".to_string(),
                            tool: "read".to_string(),
                            is_error: false,
                        },
                    }),
                    is_error: false,
                    timestamp: 2,
                },
            ],
        );
        parsed.header.cwd = "Bearer cwd-secret-token-value".to_string();

        let rows = project_session(&parsed);
        let rendered = format!(
            "{} {} {}",
            rows[0].session_title,
            rows[0].session_cwd,
            rows[0].receipt.as_ref().unwrap().detail.as_ref().unwrap()
        );
        assert!(!rendered.contains("secret-token-value"));
        assert!(rendered.contains("portable-export"));
    }

    #[test]
    fn recent_rows_are_globally_sorted_and_bounded() {
        let mut rows = project_session(&session("newer-session", call_and_result()));
        rows[0].timestamp_ms = 10;
        let mut projected = Vec::new();
        for timestamp in 11..=(ROW_LIMIT as u64 + 11) {
            let mut row = rows[0].clone();
            row.call_id = format!("call-{timestamp}");
            row.timestamp_ms = timestamp;
            projected.push(row);
        }

        retain_recent_rows(&mut rows, projected);

        assert_eq!(rows.len(), ROW_LIMIT);
        assert_eq!(rows[0].timestamp_ms, ROW_LIMIT as u64 + 11);
        assert_eq!(rows.last().unwrap().timestamp_ms, 12);
    }

    #[test]
    fn async_load_merge_preserves_live_monitor_rows() {
        let mut modal = OperationsModal::new("/workspace");
        let event = MonitorEvent {
            monitor_id: "monitor-live".to_string(),
            task_id: "task-live".to_string(),
            stream: "stdout",
            output: "matched".to_string(),
            timestamp_ms: 42,
        };
        modal.add_monitor_event(&event);
        let (tx, rx) = sync_channel(1);
        modal.loading = true;
        modal.load_rx = Some(rx);
        tx.send(OperationsLoad {
            rows: Vec::new(),
            agents: Vec::new(),
            parse_failures: Vec::new(),
            error: None,
        })
        .unwrap();

        assert!(modal.poll_load());
        assert!(
            modal.rows.iter().any(
                |row| row.call_id == event.monitor_id && row.timestamp_ms == event.timestamp_ms
            )
        );
    }

    #[test]
    fn async_load_merge_deduplicates_monitor_history() {
        let mut modal = OperationsModal::new("/workspace");
        let event = MonitorEvent {
            monitor_id: "monitor-shared".to_string(),
            task_id: "task-shared".to_string(),
            stream: "stderr",
            output: "matched".to_string(),
            timestamp_ms: 43,
        };
        modal.add_monitor_event(&event);
        let (tx, rx) = sync_channel(1);
        modal.loading = true;
        modal.load_rx = Some(rx);
        tx.send(OperationsLoad {
            rows: vec![monitor_event_row(&event)],
            agents: Vec::new(),
            parse_failures: Vec::new(),
            error: None,
        })
        .unwrap();

        assert!(modal.poll_load());
        assert_eq!(
            modal
                .rows
                .iter()
                .filter(|row| row.call_id == event.monitor_id)
                .count(),
            1
        );
    }

    #[test]
    fn agents_view_joins_run_control_and_lifecycle_state() {
        let mut modal = OperationsModal::new("/workspace");
        modal.visible = true;
        modal.agents = vec![CoordinationSnapshot {
            subagent_id: "child-1".to_string(),
            agent_ref: "subagent:child-1:2".to_string(),
            parent_scope_id: "session:parent".to_string(),
            role: "code".to_string(),
            status: "running".to_string(),
            attempt: 2,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: None,
            lifecycle_published: false,
            last_control_id: Some("control-1".to_string()),
            last_control_mode: Some("steer".to_string()),
            last_control_state: Some("held".to_string()),
            held_control_id: Some("control-1".to_string()),
            error: None,
        }];
        modal.toggle_view();
        modal.sync_selection();

        assert_eq!(modal.selected_agent_id(), Some("child-1"));
        assert_eq!(modal.selected_held_control_id(), Some("control-1"));
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("session:parent"));
        assert!(rendered.contains("control-1"));
        assert!(rendered.contains("Lifecycle"));
    }

    #[test]
    fn user_messages_do_not_create_operations() {
        let rows = project_session(&session(
            "session-a",
            vec![AppMessage::User {
                content: MessageContent::Text("hello".to_string()),
                attachments: None,
                timestamp: 1,
            }],
        ));
        assert!(rows.is_empty());
    }

    fn render_modal(modal: &mut OperationsModal, width: u16) -> String {
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area()))
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

    fn wait_for_load(modal: &mut OperationsModal) {
        for _ in 0..100 {
            if modal.poll_load() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("operations load did not finish");
    }

    #[test]
    fn wide_modal_renders_all_panes_and_narrow_modal_renders_focus() {
        let mut modal = OperationsModal::new("/workspace");
        modal.visible = true;
        modal.rows = project_session(&session("session-a", call_and_result()));
        modal.sync_selection();

        let wide = render_modal(&mut modal, 120);
        assert!(wide.contains("Session"));
        assert!(wide.contains("Task"));
        assert!(wide.contains("Receipt"));

        let narrow = render_modal(&mut modal, 80);
        assert!(narrow.contains("Session"));
        assert!(!narrow.contains(" Task "));
        modal.focus_next();
        let task = render_modal(&mut modal, 80);
        assert!(task.contains(" Task "));
        assert!(!task.contains(" Receipt "));
    }

    #[test]
    fn focused_task_and_receipt_panes_scroll_and_reset_on_selection() {
        let mut modal = OperationsModal::new("/workspace");
        modal.visible = true;
        modal.rows = project_session(&session("session-a", call_and_result()));
        modal.rows[0].task_args = Some("argument line\n".repeat(40));
        modal.parse_failures = (0..40)
            .map(|index| format!("session-{index}: invalid entry"))
            .collect();
        modal.sync_selection();

        modal.focus = FocusedPane::Task;
        modal.scroll_down();
        let _ = render_modal(&mut modal, 80);
        assert_eq!(modal.task_scroll, SCROLL_STEP);

        modal.focus = FocusedPane::Receipt;
        modal.scroll_down();
        let _ = render_modal(&mut modal, 80);
        assert_eq!(modal.receipt_scroll, SCROLL_STEP);

        modal.select_first();
        assert_eq!(modal.task_scroll, 0);
        assert_eq!(modal.receipt_scroll, 0);
    }

    #[test]
    fn show_projects_recent_persisted_sessions() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("session-a.jsonl"),
            concat!(
                "{\"type\":\"session\",\"id\":\"session-a\",\"timestamp\":\"2026-07-23T10:00:00Z\",\"cwd\":\"/workspace\",\"model\":\"test\",\"thinkingLevel\":\"off\"}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-07-23T10:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"call-1\",\"name\":\"read\",\"arguments\":{\"path\":\"README.md\"}}],\"timestamp\":1}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-07-23T10:00:02Z\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call-1\",\"toolName\":\"read\",\"content\":\"ok\",\"isError\":false,\"timestamp\":2}}\n"
            ),
        )
        .unwrap();
        let manager = SessionManager::with_sessions_dir("/workspace", directory.path());
        let mut modal = OperationsModal::with_manager(manager);

        modal.show();
        assert!(modal.loading);
        assert!(modal.rows.is_empty());
        wait_for_load(&mut modal);

        assert!(modal.is_visible());
        assert_eq!(modal.rows.len(), 1);
        assert_eq!(modal.rows[0].session_id, "session-a");
        assert_eq!(modal.rows[0].call_id, "call-1");
    }

    #[test]
    fn surfaces_each_recent_session_parse_failure() {
        let directory = tempdir().unwrap();
        for id in ["broken-a", "broken-b"] {
            std::fs::write(
                directory.path().join(format!("{id}.jsonl")),
                format!(
                    "{{\"type\":\"session\",\"id\":\"{id}\",\"timestamp\":\"2026-07-23T10:00:00Z\",\"cwd\":\"/workspace\",\"model\":\"test\",\"thinkingLevel\":\"off\"}}\nnot-json\n"
                ),
            )
            .unwrap();
        }
        let manager = SessionManager::with_sessions_dir("/workspace", directory.path());
        let mut modal = OperationsModal::with_manager(manager);

        modal.show();
        wait_for_load(&mut modal);

        assert_eq!(modal.parse_failures.len(), 2);
        assert!(
            modal
                .parse_failures
                .iter()
                .any(|failure| failure.contains("broken-a"))
        );
        assert!(
            modal
                .parse_failures
                .iter()
                .any(|failure| failure.contains("broken-b"))
        );
        let rendered = render_modal(&mut modal, 80);
        assert!(rendered.contains("load failures"));
    }
}
