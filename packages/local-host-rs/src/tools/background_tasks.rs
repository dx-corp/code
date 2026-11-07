//! Background task manager for long-running processes.
//!
//! This module provides functionality for managing background processes that
//! persist across agent interactions. Tasks are tracked with unique IDs and
//! their output is logged to files for later retrieval.
//!
//! # Features
//!
//! - Start processes in the background with optional shell mode
//! - Track process status (running, exited, failed, stopped)
//! - Retrieve logs from running or completed tasks
//! - Stop tasks and their child processes
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_local_host::tools::background_tasks::{start, list, stop, logs};
//!
//! // Start a dev server
//! let task = start(
//!     "npm run dev".to_string(),
//!     ".".to_string(),
//!     ".".to_string(),
//!     true,
//!     None,
//!     None,
//! )
//! .await?;
//!
//! // Check running tasks
//! for task in list() {
//!     println!("{}: {:?}", task.id, task.status);
//! }
//!
//! // Get logs
//! let output = logs(&task.id, 50)?;
//! ```

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use regex::{Regex, RegexBuilder};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use super::bash::{BashTool, resolve_shell_config};
use super::process_registry;
use super::process_utils::set_new_process_group;
use super::shell_env::resolve_shell_environment;
use crate::safety::{Severity, check_dangerous_patterns};

mod line_watch;
use line_watch::{LineOutcome, LineWatch};
pub use line_watch::{MonitorNotification, MonitorNotificationKind};

/// Status of a background task.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    /// Task is currently running.
    Running,
    /// Task exited successfully (exit code 0).
    Exited,
    /// Task failed (non-zero exit code or error).
    Failed,
    /// Task was manually stopped.
    Stopped,
}

/// Immutable authorized coordinates captured before a task is spawned.
/// `session_generation` prevents a deleted/recreated session id adopting old tasks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskScope {
    pub session_id: String,
    pub session_generation: String,
    pub owner: Option<String>,
    pub organization_id: Option<String>,
    pub workspace_id: Option<String>,
}

impl BackgroundTaskScope {
    pub fn is_valid(&self) -> bool {
        !self.session_id.trim().is_empty()
            && !self.session_generation.trim().is_empty()
            && self.owner.as_ref().is_none_or(|id| !id.trim().is_empty())
            && match (&self.organization_id, &self.workspace_id) {
                (None, None) => true, // explicit local/legacy subject-only scope
                (Some(org), Some(workspace)) => {
                    !org.trim().is_empty() && !workspace.trim().is_empty()
                }
                _ => false,
            }
    }
}

/// Host-selected access policy; an absent gateway session is never legacy CLI access.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BackgroundTaskAccess {
    #[default]
    Legacy,
    Scoped(BackgroundTaskScope),
    Denied,
}

/// A background task with its metadata and status.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    #[serde(default)]
    pub scope: Option<BackgroundTaskScope>,
    #[serde(default)]
    pub completion_sequence: u64,
    pub id: String,
    pub pid: Option<u32>,
    pub command: String,
    #[allow(dead_code)]
    pub cwd: String,
    pub log_path: String,
    pub log_write_failed: bool,
    pub log_write_error: Option<String>,
    pub status: BackgroundTaskStatus,
    #[allow(dead_code)]
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub exit_code: Option<i32>,
}

static TASKS: std::sync::LazyLock<RwLock<HashMap<String, BackgroundTask>>> =
    std::sync::LazyLock::new(|| RwLock::new(load_terminal_tasks()));
static ROTATION_OBSERVERS: std::sync::LazyLock<RwLock<HashMap<String, LogRotationObserver>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));
static MONITORS: std::sync::LazyLock<RwLock<HashMap<String, BackgroundMonitor>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));
static MONITOR_EVENTS: std::sync::LazyLock<RwLock<VecDeque<MonitorEvent>>> =
    std::sync::LazyLock::new(|| RwLock::new(VecDeque::new()));
static MONITOR_HISTORY: std::sync::LazyLock<RwLock<VecDeque<MonitorEvent>>> =
    std::sync::LazyLock::new(|| RwLock::new(VecDeque::new()));
static MONITOR_BUDGET: std::sync::LazyLock<StdMutex<MonitorBudget>> =
    std::sync::LazyLock::new(|| StdMutex::new(MonitorBudget::new()));
static TASK_LIFECYCLE_EVENTS: std::sync::LazyLock<RwLock<VecDeque<TaskLifecycleEvent>>> =
    std::sync::LazyLock::new(|| RwLock::new(VecDeque::new()));
// Leaf lock: never held while taking TASKS or MONITORS.
static MONITOR_NOTIFICATIONS: std::sync::LazyLock<StdMutex<VecDeque<MonitorNotification>>> =
    std::sync::LazyLock::new(|| StdMutex::new(VecDeque::new()));

// Serialize writes so an older snapshot cannot overwrite a newer lifecycle event.
static SNAPSHOT_WRITE_LOCK: StdMutex<()> = StdMutex::new(());
static SNAPSHOT_ERROR: StdMutex<Option<String>> = StdMutex::new(None);
static COMPLETION_SEQUENCE: std::sync::LazyLock<AtomicU64> = std::sync::LazyLock::new(|| {
    let persisted = read_snapshot_value()["completionSequence"]
        .as_u64()
        .unwrap_or(0);
    let retained = read_owned_tasks()
        .unwrap_or_default()
        .iter()
        .map(|task| task.completion_sequence)
        .max()
        .unwrap_or(0);
    AtomicU64::new(persisted.max(retained))
});

const DEFAULT_LOG_FILE_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_LOG_SEGMENTS: usize = 2;
const MAX_LOG_SEGMENTS: usize = 10;
const MIN_LOG_BYTES: u64 = 50_000;
const MAX_MONITORS: usize = 32;
const MAX_COMPLETED_TASKS: usize = 128;
const MAX_MONITORS_PER_TASK: usize = 8;
const MAX_MONITOR_PATTERN_BYTES: usize = 256;
const MAX_MONITOR_REGEX_BYTES: usize = 1024 * 1024;
const MAX_MONITOR_LINE_CHARS: usize = 8192;
const MAX_MONITOR_OUTPUT_CHARS: usize = 512;
const MAX_MONITOR_EVENTS: usize = 128;
const MAX_MONITOR_HISTORY: usize = 200;
const MAX_MONITOR_EVENTS_PER_SECOND: u32 = 5;
const MAX_MONITOR_EVALUATIONS_PER_SECOND: u32 = 2_048;
const MAX_MONITOR_GLOBAL_EVENTS_PER_SECOND: u32 = 32;
/// Cap concurrent running background processes (Kimi-style max running tasks).
const DEFAULT_MAX_RUNNING_TASKS: usize = 16;
const MAX_TASK_LIFECYCLE_EVENTS: usize = 64;
/// Undelivered all-lines monitor notifications retained across every owner.
const MAX_MONITOR_NOTIFICATIONS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: String,
    pub task_id: String,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorEvent {
    pub monitor_id: String,
    pub task_id: String,
    pub stream: &'static str,
    pub output: String,
    pub timestamp_ms: u64,
}

/// Process exit / stop notifications for the TUI and agent (non-blocking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLifecycleEvent {
    pub task_id: String,
    pub command: String,
    /// `exited` | `failed` | `stopped`
    pub status: String,
    pub exit_code: Option<i32>,
    pub timestamp_ms: u64,
}

/// Snapshot of a task that was running when a previous process exited.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedRunningTask {
    pub id: String,
    pub command: String,
    pub pid: Option<u32>,
    pub log_path: String,
    pub started_at_unix: u64,
}

fn max_running_tasks() -> usize {
    std::env::var("MAESTRO_BACKGROUND_MAX_RUNNING_TASKS")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(DEFAULT_MAX_RUNNING_TASKS)
}

/// Keep recent terminal metadata for log lookup without retaining every command
/// ever launched in this process. Running tasks must never be evicted.
fn prune_completed_tasks(tasks: &mut HashMap<String, BackgroundTask>) {
    let mut terminal = tasks
        .values()
        .filter(|task| !matches!(task.status, BackgroundTaskStatus::Running))
        .map(|task| (task.finished_at.unwrap_or(task.started_at), task.id.clone()))
        .collect::<Vec<_>>();
    let overflow = terminal.len().saturating_sub(MAX_COMPLETED_TASKS);
    if overflow == 0 {
        return;
    }
    terminal.sort_unstable();
    for (_, id) in terminal.into_iter().take(overflow) {
        tasks.remove(&id);
    }
}

fn running_task_count() -> usize {
    TASKS
        .read()
        .map(|tasks| {
            tasks
                .values()
                .filter(|t| matches!(t.status, BackgroundTaskStatus::Running))
                .count()
        })
        .unwrap_or(0)
}

fn persist_path() -> PathBuf {
    // Tests that start a background task used to write `~/.maestro/background_tasks.json`
    // whenever they forgot `MAESTRO_HOME`. The next TUI launch then warned about a
    // dead leftover such as `head -c 60000 /dev/zero; sleep 0.2`.
    #[cfg(test)]
    if std::env::var_os("MAESTRO_HOME").is_none() {
        return std::env::temp_dir()
            .join(format!("maestro-test-bg-tasks-{}.json", std::process::id()));
    }

    crate::path_utils::maestro_home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("background_tasks.json")
}

fn write_running_snapshot(running: &[PersistedRunningTask]) {
    let Ok(_write_guard) = SNAPSHOT_WRITE_LOCK.lock() else {
        return;
    };
    let path = persist_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut payload = read_snapshot_value();
    payload["running"] = serde_json::json!(running);
    if let Ok(raw) = serde_json::to_string_pretty(&payload) {
        let _ = crate::fs_atomic::write_atomic(&path, raw.as_bytes());
    }
}

fn persist_running_snapshot() {
    let _ = persist_task_snapshot();
}

fn persist_task_snapshot() -> Result<(), String> {
    let _write_guard = SNAPSHOT_WRITE_LOCK
        .lock()
        .map_err(|_| "Background snapshot unavailable".to_string())?;
    let result = write_task_snapshot();
    if let Ok(mut error) = SNAPSHOT_ERROR.lock() {
        *error = result.as_ref().err().cloned();
    }
    result
}

fn write_task_snapshot() -> Result<(), String> {
    // A corrupt saved roster must not be replaced by an empty accepted state.
    let _ = read_owned_tasks()?;
    let running: Vec<PersistedRunningTask> = TASKS
        .read()
        .map(|tasks| {
            tasks
                .values()
                .filter(|t| matches!(t.status, BackgroundTaskStatus::Running))
                .map(|t| PersistedRunningTask {
                    id: t.id.clone(),
                    command: t.command.clone(),
                    pid: t.pid,
                    log_path: t.log_path.clone(),
                    started_at_unix: t
                        .started_at
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    let owned = TASKS
        .read()
        .map_err(|_| "Background registry unavailable".to_string())?
        .values()
        .filter(|task| task.scope.is_some())
        .cloned()
        .collect::<Vec<_>>();
    let path = persist_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let payload = serde_json::json!({ "running": running, "ownedTasks": owned,
        "completionSequence": COMPLETION_SEQUENCE.load(Ordering::SeqCst) });
    let raw = serde_json::to_vec_pretty(&payload).map_err(|error| error.to_string())?;
    crate::fs_atomic::write_atomic(&path, &raw).map_err(|error| error.to_string())
}

fn read_snapshot_value() -> serde_json::Value {
    fs::read(persist_path())
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

fn read_owned_tasks() -> Result<Vec<BackgroundTask>, String> {
    let raw = match fs::read(persist_path()) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("Could not read background task state: {error}")),
    };
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("Could not parse background task state: {error}"))?;
    if !value.is_object() {
        return Err("Invalid background task state".into());
    }
    serde_json::from_value(
        value
            .get("ownedTasks")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    )
    .map_err(|error| format!("Could not parse background task roster: {error}"))
}

fn load_terminal_tasks() -> HashMap<String, BackgroundTask> {
    // A restarted host cannot supervise a previous process. Do not invent its
    // completion from PID liveness; only restore recorded terminal events.
    read_owned_tasks()
        .unwrap_or_default()
        .into_iter()
        .filter(|task| {
            !matches!(task.status, BackgroundTaskStatus::Running)
                && task
                    .scope
                    .as_ref()
                    .is_some_and(BackgroundTaskScope::is_valid)
        })
        .map(|task| (task.id.clone(), task))
        .collect()
}

/// Read the existing owner's durable roster, never global/cwd-only tasks.
/// Persistence failures are returned rather than presenting an unsaved completion.
pub fn list_scoped(scope: &BackgroundTaskScope) -> Result<Vec<BackgroundTask>, String> {
    if !scope.is_valid() {
        return Err("Invalid background task scope".to_string());
    }
    if let Some(error) = SNAPSHOT_ERROR
        .lock()
        .map_err(|_| "Background snapshot unavailable".to_string())?
        .clone()
    {
        return Err(format!("Could not save background lifecycle: {error}"));
    }
    // Read completion only from the saved event metadata, not from an
    // optimistic in-memory status or a drained notification queue.
    let mut tasks = read_owned_tasks()?
        .into_iter()
        .filter(|task| task.scope.as_ref() == Some(scope))
        .filter(|task| {
            !matches!(task.status, BackgroundTaskStatus::Running)
                || task_belongs_to_scope(&task.id, scope)
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        left.started_at
            .cmp(&right.started_at)
            .then(left.id.cmp(&right.id))
    });
    for task in &mut tasks {
        task.command = redact_text(&task.command);
    }
    Ok(tasks)
}

/// Scope checks apply to model-facing log/stop/monitor calls as well as the UI.
pub fn task_belongs_to_scope(id: &str, scope: &BackgroundTaskScope) -> bool {
    scope.is_valid()
        && TASKS.read().is_ok_and(|tasks| {
            tasks
                .get(id)
                .is_some_and(|task| task.scope.as_ref() == Some(scope))
        })
}

fn process_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 delivers no signal; `kill` only checks whether a
        // process with this pid exists. PID reuse can produce a false live
        // result, so this is a liveness hint, not a security check.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn persisted_task_is_live(task: &PersistedRunningTask) -> bool {
    task.pid.is_some_and(process_is_live)
}

fn reap_dead_persisted_tasks(tasks: Vec<PersistedRunningTask>) -> Vec<PersistedRunningTask> {
    tasks.into_iter().filter(persisted_task_is_live).collect()
}

fn read_persisted_running_snapshot() -> Vec<PersistedRunningTask> {
    let path = persist_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    value
        .get("running")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Tasks marked running when the previous Maestro process wrote its snapshot.
///
/// Dead PIDs are dropped and the snapshot is rewritten so a finished leftover
/// does not warn on every subsequent launch.
pub fn load_persisted_running_snapshot() -> Vec<PersistedRunningTask> {
    let loaded = read_persisted_running_snapshot();
    let original_len = loaded.len();
    let live = reap_dead_persisted_tasks(loaded);
    if live.len() != original_len {
        write_running_snapshot(&live);
    }
    live
}

fn emit_task_lifecycle(task_id: &str, command: &str, status: &str, exit_code: Option<i32>) {
    let event = TaskLifecycleEvent {
        task_id: task_id.to_string(),
        command: command.to_string(),
        status: status.to_string(),
        exit_code,
        timestamp_ms: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    };
    if let Ok(mut events) = TASK_LIFECYCLE_EVENTS.write() {
        if events.len() >= MAX_TASK_LIFECYCLE_EVENTS {
            events.pop_front();
        }
        events.push_back(event.clone());
    }
    // Also mirror into monitor stream so Operations UI can show lifecycle rows.
    if let Ok(mut events) = MONITOR_EVENTS.write() {
        if events.len() >= MAX_MONITOR_EVENTS {
            events.pop_front();
        }
        events.push_back(MonitorEvent {
            monitor_id: "lifecycle".to_string(),
            task_id: event.task_id.clone(),
            stream: "lifecycle",
            output: format!(
                "task {} {}{}",
                event.status,
                event.command.chars().take(80).collect::<String>(),
                event
                    .exit_code
                    .map(|c| format!(" exit={c}"))
                    .unwrap_or_default()
            ),
            timestamp_ms: event.timestamp_ms,
        });
    }
    persist_running_snapshot();
    crate::ui_wake::wake();
}

/// Drain process exit/stop notifications for the UI (and optional agent nudge).
pub fn poll_task_lifecycle_events() -> Vec<TaskLifecycleEvent> {
    TASK_LIFECYCLE_EVENTS
        .write()
        .map(|mut events| events.drain(..).collect())
        .unwrap_or_default()
}

#[derive(Debug)]
struct BackgroundMonitor {
    info: MonitorInfo,
    matcher: MonitorMatcher,
    window_started: Instant,
    window_events: u32,
}

/// A regex monitor emits matching stdout/stderr lines to the Operations view;
/// a line watch batches every stdout line into owner-scoped agent notifications.
#[derive(Debug)]
enum MonitorMatcher {
    Pattern(Regex),
    Lines(Box<LineWatch>),
}

impl BackgroundMonitor {
    fn new(info: MonitorInfo, matcher: MonitorMatcher) -> Self {
        Self {
            info,
            matcher,
            window_started: Instant::now(),
            window_events: 0,
        }
    }

    /// Per-monitor event window shared by regex matches and line batches.
    fn take_window_event(&mut self, now: Instant) -> bool {
        if now.duration_since(self.window_started) >= Duration::from_secs(1) {
            self.window_started = now;
            self.window_events = 0;
        }
        if self.window_events >= MAX_MONITOR_EVENTS_PER_SECOND {
            return false;
        }
        self.window_events += 1;
        true
    }
}

#[derive(Debug)]
struct MonitorBudget {
    window_started: Instant,
    evaluations: u32,
    events: u32,
}

impl MonitorBudget {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            evaluations: 0,
            events: 0,
        }
    }

    fn reset_window(&mut self, now: Instant) {
        if now.duration_since(self.window_started) >= Duration::from_secs(1) {
            self.window_started = now;
            self.evaluations = 0;
            self.events = 0;
        }
    }

    fn take_evaluation(&mut self, now: Instant) -> bool {
        self.reset_window(now);
        if self.evaluations >= MAX_MONITOR_EVALUATIONS_PER_SECOND {
            return false;
        }
        self.evaluations += 1;
        true
    }

    fn take_event(&mut self, now: Instant) -> bool {
        self.reset_window(now);
        if self.events >= MAX_MONITOR_GLOBAL_EVENTS_PER_SECOND {
            return false;
        }
        self.events += 1;
        true
    }
}

fn take_monitor_evaluation_budget(now: Instant) -> bool {
    let Ok(mut budget) = MONITOR_BUDGET.lock() else {
        return false;
    };
    budget.take_evaluation(now)
}

fn take_monitor_event_budget(now: Instant) -> bool {
    let Ok(mut budget) = MONITOR_BUDGET.lock() else {
        return false;
    };
    budget.take_event(now)
}

pub fn attach_monitor(task_id: &str, pattern: &str) -> Result<MonitorInfo, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("Monitor regex cannot be empty".to_string());
    }
    if pattern.len() > MAX_MONITOR_PATTERN_BYTES {
        return Err(format!(
            "Monitor regex exceeds {MAX_MONITOR_PATTERN_BYTES} bytes"
        ));
    }
    let regex = RegexBuilder::new(pattern)
        .size_limit(MAX_MONITOR_REGEX_BYTES)
        .dfa_size_limit(MAX_MONITOR_REGEX_BYTES)
        .build()
        .map_err(|error| format!("Invalid monitor regex: {error}"))?;
    let tasks = TASKS
        .read()
        .map_err(|_| "Task registry unavailable".to_string())?;
    let task = tasks
        .get(task_id)
        .ok_or_else(|| "Task not found".to_string())?;
    if !matches!(task.status, BackgroundTaskStatus::Running) {
        return Err("Task is not running".to_string());
    }
    insert_monitor(task_id, pattern, MonitorMatcher::Pattern(regex))
}

/// Register a monitor under the shared global and per-task limits.
/// Lock order: callers may hold TASKS; this takes MONITORS.
fn insert_monitor(
    task_id: &str,
    label: &str,
    matcher: MonitorMatcher,
) -> Result<MonitorInfo, String> {
    let mut monitors = MONITORS
        .write()
        .map_err(|_| "Monitor registry unavailable".to_string())?;
    if monitors.len() >= MAX_MONITORS {
        return Err(format!("Monitor limit reached ({MAX_MONITORS})"));
    }
    let task_count = monitors
        .values()
        .filter(|monitor| monitor.info.task_id == task_id)
        .count();
    if task_count >= MAX_MONITORS_PER_TASK {
        return Err(format!(
            "Task monitor limit reached ({MAX_MONITORS_PER_TASK})"
        ));
    }
    let info = MonitorInfo {
        id: Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        pattern: label.to_string(),
    };
    monitors.insert(
        info.id.clone(),
        BackgroundMonitor::new(info.clone(), matcher),
    );
    Ok(info)
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    let mut monitors = MONITORS
        .read()
        .map(|monitors| {
            monitors
                .values()
                .map(|monitor| {
                    let mut info = monitor.info.clone();
                    info.pattern = redact_text(&info.pattern);
                    info
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    monitors.sort_by(|left, right| left.id.cmp(&right.id));
    monitors
}

/// Remove a monitor. Removing a line watch ends the watch, which stops its
/// script with a final `stopped` notification (the watch owns the process).
pub fn remove_monitor(id: &str) -> Result<MonitorInfo, String> {
    let line_watch_task = MONITORS
        .read()
        .map_err(|_| "Monitor registry unavailable".to_string())?
        .get(id)
        .filter(|monitor| matches!(monitor.matcher, MonitorMatcher::Lines(_)))
        .map(|monitor| monitor.info.clone());
    if let Some(info) = line_watch_task {
        stop_with_end(&info.task_id, Some(WatchEnd::Stopped))?;
        return Ok(info);
    }
    MONITORS
        .write()
        .map_err(|_| "Monitor registry unavailable".to_string())?
        .remove(id)
        .map(|monitor| monitor.info)
        .ok_or_else(|| "Monitor not found".to_string())
}

pub fn poll_monitor_events() -> Vec<MonitorEvent> {
    MONITOR_EVENTS
        .write()
        .map(|mut events| events.drain(..).collect())
        .unwrap_or_default()
}

pub fn monitor_event_history() -> Vec<MonitorEvent> {
    MONITOR_HISTORY
        .read()
        .map(|events| events.iter().cloned().collect())
        .unwrap_or_default()
}

fn redact_text(text: &str) -> String {
    match crate::agent::credential_store::redact_credentials_in_json(&serde_json::Value::String(
        text.to_string(),
    )) {
        serde_json::Value::String(value) => value,
        _ => String::new(),
    }
}

fn emit_monitor_matches(task_id: &str, stream: &'static str, line: &str) {
    let bounded: String = line.chars().take(MAX_MONITOR_LINE_CHARS).collect();
    let now = Instant::now();
    let mut matched = Vec::new();
    let mut wakes = Vec::new();
    let mut flooded = false;
    if let Ok(mut monitors) = MONITORS.write() {
        for monitor in monitors
            .values_mut()
            .filter(|monitor| monitor.info.task_id == task_id)
        {
            let regex = match &mut monitor.matcher {
                MonitorMatcher::Pattern(regex) => regex,
                MonitorMatcher::Lines(watch) => {
                    // Stderr goes to the task log only.
                    if stream != "stdout" {
                        continue;
                    }
                    if !take_monitor_evaluation_budget(now) {
                        if watch.drop_line() {
                            wakes.push(watch.wake.clone());
                        }
                        continue;
                    }
                    match watch.push(&bounded, now) {
                        LineOutcome::Batched { first: true } => wakes.push(watch.wake.clone()),
                        LineOutcome::Batched { first: false } | LineOutcome::Ignored => {}
                        LineOutcome::Flooded => flooded = true,
                    }
                    continue;
                }
            };
            if !take_monitor_evaluation_budget(now) {
                break;
            }
            if !regex.is_match(&bounded) {
                continue;
            }
            if !monitor.take_window_event(now) {
                continue;
            }
            if !take_monitor_event_budget(now) {
                break;
            }
            let redacted = redact_text(&bounded);
            let mut output: String = redacted.chars().take(MAX_MONITOR_OUTPUT_CHARS).collect();
            if redacted.chars().count() > MAX_MONITOR_OUTPUT_CHARS {
                output.push_str("...");
            }
            matched.push(MonitorEvent {
                monitor_id: monitor.info.id.clone(),
                task_id: task_id.to_string(),
                stream,
                output,
                timestamp_ms: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_millis() as u64),
            });
        }
    }
    let had_events = !matched.is_empty();
    for event in matched {
        if let Ok(mut events) = MONITOR_EVENTS.write() {
            if events.len() >= MAX_MONITOR_EVENTS {
                events.pop_front();
            }
            events.push_back(event.clone());
        }
        if let Ok(mut history) = MONITOR_HISTORY.write() {
            if history.len() >= MAX_MONITOR_HISTORY {
                history.pop_front();
            }
            history.push_back(event);
        }
    }
    if had_events {
        crate::ui_wake::wake();
    }
    for wake in wakes {
        wake.notify_one();
    }
    if flooded {
        // MONITORS is released: stopping takes TASKS before MONITORS.
        let _ = stop_with_end(task_id, Some(WatchEnd::Flooded));
    }
}

fn remove_task_monitors(task_id: &str) {
    if let Ok(mut monitors) = MONITORS.write() {
        monitors.retain(|_, monitor| monitor.info.task_id != task_id);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Why a line watch ended. Each watch reports exactly one end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchEnd {
    Exited(Option<i32>),
    Expired,
    Flooded,
    Stopped,
}

fn push_monitor_notification(notification: MonitorNotification) {
    let Ok(mut queue) = MONITOR_NOTIFICATIONS.lock() else {
        return;
    };
    if queue.len() >= MAX_MONITOR_NOTIFICATIONS {
        // Shed output before any watch's terminal notice.
        let victim = queue
            .iter()
            .position(|queued| !queued.kind.is_terminal())
            .unwrap_or(0);
        queue.remove(victim);
    }
    queue.push_back(notification);
}

/// Drain the undelivered line-watch notifications owned by `scope`
/// (`None` = legacy local tasks). Other owners' notifications stay queued.
pub fn poll_monitor_notifications(scope: Option<&BackgroundTaskScope>) -> Vec<MonitorNotification> {
    let Ok(mut queue) = MONITOR_NOTIFICATIONS.lock() else {
        return Vec::new();
    };
    let mut owned = Vec::new();
    queue.retain(|notification| {
        if notification.scope.as_ref() == scope {
            owned.push(notification.clone());
            false
        } else {
            true
        }
    });
    owned
}

fn line_notification(
    info: &MonitorInfo,
    watch: &LineWatch,
    kind: MonitorNotificationKind,
) -> MonitorNotification {
    MonitorNotification {
        scope: watch.scope.clone(),
        monitor_id: info.id.clone(),
        task_id: info.task_id.clone(),
        description: watch.description.clone(),
        kind,
        timestamp_ms: now_ms(),
    }
}

/// Mirror a delivered batch into the Operations view's monitor stream.
fn mirror_line_batch(notification: &MonitorNotification) {
    let MonitorNotificationKind::Lines { lines, omitted } = &notification.kind else {
        return;
    };
    let joined = lines.join(" | ");
    let mut output: String = joined.chars().take(MAX_MONITOR_OUTPUT_CHARS).collect();
    if joined.chars().count() > MAX_MONITOR_OUTPUT_CHARS || *omitted > 0 {
        output.push_str("...");
    }
    let event = MonitorEvent {
        monitor_id: notification.monitor_id.clone(),
        task_id: notification.task_id.clone(),
        stream: "stdout",
        output,
        timestamp_ms: notification.timestamp_ms,
    };
    if let Ok(mut events) = MONITOR_EVENTS.write() {
        if events.len() >= MAX_MONITOR_EVENTS {
            events.pop_front();
        }
        events.push_back(event.clone());
    }
    if let Ok(mut history) = MONITOR_HISTORY.write() {
        if history.len() >= MAX_MONITOR_HISTORY {
            history.pop_front();
        }
        history.push_back(event);
    }
}

/// Redact and deliver. Callers must not hold MONITORS: redaction is per line.
fn deliver_line_notification(mut notification: MonitorNotification) {
    if let MonitorNotificationKind::Lines { lines, .. } = &mut notification.kind {
        for line in lines.iter_mut() {
            *line = line_watch::bound_line(&redact_text(line));
        }
    }
    mirror_line_batch(&notification);
    push_monitor_notification(notification);
    crate::ui_wake::wake();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushOutcome {
    Idle,
    Emitted,
    /// A per-monitor or global event budget refused; the batch stays pending.
    Deferred,
    Closed,
}

/// Emit the pending batch of one line watch under the shared event budgets.
fn flush_line_watch(monitor_id: &str, now: Instant) -> FlushOutcome {
    let notification = {
        let Ok(mut monitors) = MONITORS.write() else {
            return FlushOutcome::Closed;
        };
        let Some(monitor) = monitors.get_mut(monitor_id) else {
            return FlushOutcome::Closed;
        };
        let MonitorMatcher::Lines(watch) = &monitor.matcher else {
            return FlushOutcome::Closed;
        };
        if !watch.has_pending() {
            return FlushOutcome::Idle;
        }
        if !monitor.take_window_event(now) || !take_monitor_event_budget(now) {
            return FlushOutcome::Deferred;
        }
        let MonitorMatcher::Lines(watch) = &mut monitor.matcher else {
            return FlushOutcome::Closed;
        };
        let Some(kind) = watch.take_batch() else {
            return FlushOutcome::Idle;
        };
        line_notification(&monitor.info, watch, kind)
    };
    deliver_line_notification(notification);
    FlushOutcome::Emitted
}

/// End every line watch on `task_id`: deliver its last batch, then its one
/// terminal notification. `None` ends silently (a start that was not accepted).
/// Callers must not hold TASKS or MONITORS.
fn close_line_watches(task_id: &str, end: Option<WatchEnd>) {
    end_line_watches(take_line_watches(task_id), end);
}

/// Remove a task's line watches. Whoever removes a watch reports its end, so
/// concurrent exit and stop paths cannot both notify.
/// Lock order: callers may hold TASKS; this takes MONITORS.
fn take_line_watches(task_id: &str) -> Vec<BackgroundMonitor> {
    let Ok(mut monitors) = MONITORS.write() else {
        return Vec::new();
    };
    let ids = monitors
        .iter()
        .filter(|(_, monitor)| {
            monitor.info.task_id == task_id && matches!(monitor.matcher, MonitorMatcher::Lines(_))
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ids.into_iter()
        .filter_map(|id| monitors.remove(&id))
        .collect()
}

fn end_line_watches(closed: Vec<BackgroundMonitor>, end: Option<WatchEnd>) {
    for monitor in closed {
        let MonitorMatcher::Lines(mut watch) = monitor.matcher else {
            continue;
        };
        // The flusher wakes, finds its watch gone, and exits.
        watch.wake.notify_one();
        let Some(end) = end else {
            continue;
        };
        if let Some(kind) = watch.take_batch() {
            deliver_line_notification(line_notification(&monitor.info, &watch, kind));
        }
        let events = watch.events();
        let kind = match end {
            WatchEnd::Exited(exit_code) => MonitorNotificationKind::Exited { exit_code, events },
            WatchEnd::Expired => MonitorNotificationKind::Expired {
                events,
                timeout_ms: watch.timeout_ms,
            },
            WatchEnd::Flooded => MonitorNotificationKind::Flooded { events },
            WatchEnd::Stopped => MonitorNotificationKind::Stopped { events },
        };
        deliver_line_notification(line_notification(&monitor.info, &watch, kind));
    }
}

/// The deadline passed: stop a running script, or end the watch of one that
/// already exited (its output streams were still draining).
fn expire_line_watch(task_id: &str) {
    let exited = TASKS.read().ok().and_then(|tasks| {
        tasks
            .get(task_id)
            .filter(|task| !matches!(task.status, BackgroundTaskStatus::Running))
            .map(|task| task.exit_code)
    });
    match exited {
        Some(exit_code) => close_line_watches(task_id, Some(WatchEnd::Exited(exit_code))),
        None => {
            if stop_with_end(task_id, Some(WatchEnd::Expired)).is_err() {
                close_line_watches(task_id, Some(WatchEnd::Expired));
            }
        }
    }
}

/// One flusher per line watch: batch for the window after the first line,
/// retry deferred batches, and enforce the deadline.
///
/// Cancellation safety: both `select!` arms are re-created each iteration.
/// Dropping `notified()` loses nothing because `notify_one` stores a permit
/// when no waiter is registered; dropping `sleep_until` re-arms the same deadline.
async fn run_line_watch(
    monitor_id: String,
    task_id: String,
    wake: Arc<Notify>,
    deadline: tokio::time::Instant,
) {
    loop {
        tokio::select! {
            () = wake.notified() => {}
            () = tokio::time::sleep_until(deadline) => {
                expire_line_watch(&task_id);
                return;
            }
        }
        loop {
            tokio::time::sleep(line_watch::MONITOR_BATCH_WINDOW).await;
            match flush_line_watch(&monitor_id, Instant::now()) {
                FlushOutcome::Closed => return,
                FlushOutcome::Deferred if tokio::time::Instant::now() < deadline => {}
                FlushOutcome::Deferred | FlushOutcome::Idle | FlushOutcome::Emitted => break,
            }
        }
    }
}

/// Removes a reserved monitor unless the task that owns it was accepted.
struct MonitorReservation(Option<String>);

impl MonitorReservation {
    fn accept(mut self) -> Option<String> {
        self.0.take()
    }
}

impl Drop for MonitorReservation {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            if let Ok(mut monitors) = MONITORS.write() {
                monitors.remove(&id);
            }
        }
    }
}

fn read_env_u64(name: &str, default: u64, min: u64) -> u64 {
    match std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(0) => 0,
        Some(value) if value < min => min,
        Some(value) => value,
        None => default,
    }
}

fn read_env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    match std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(value) if value < min => min,
        Some(value) if value > max => max,
        Some(value) => value,
        None => default,
    }
}

fn log_limits() -> (u64, usize) {
    let bytes = read_env_u64(
        "MAESTRO_BACKGROUND_TASK_LOG_BYTES",
        DEFAULT_LOG_FILE_BYTES,
        MIN_LOG_BYTES,
    );
    let segments = read_env_usize(
        "MAESTRO_BACKGROUND_TASK_LOG_SEGMENTS",
        DEFAULT_LOG_SEGMENTS,
        0,
        MAX_LOG_SEGMENTS,
    );
    (bytes, segments)
}

#[derive(Debug, Clone)]
pub struct LogRotationInfo {
    pub log_path: PathBuf,
    pub archive_path: PathBuf,
    pub rotated_at: SystemTime,
}

#[derive(Debug, Default)]
struct RotationState {
    last_rotation: Option<LogRotationInfo>,
    failure_reason: Option<String>,
}

#[derive(Clone)]
struct LogRotationObserver {
    limit: u64,
    segments: usize,
    state: Arc<Mutex<RotationState>>,
    notify: Arc<Notify>,
}

impl LogRotationObserver {
    async fn wait_for_rotation(&self, timeout: Duration) -> Result<LogRotationInfo, String> {
        if self.segments == 0 || self.limit == 0 {
            return Err("Log rotation is disabled".to_string());
        }

        // Non-blocking snapshot (default): never stall the agent turn.
        // Kimi TaskOutput no longer blocks; match that for waitForRotation.
        if timeout.is_zero() {
            let state = self.state.lock().await;
            if let Some(info) = state.last_rotation.clone() {
                return Ok(info);
            }
            if let Some(reason) = &state.failure_reason {
                return Err(reason.clone());
            }
            return Err("No log rotation yet (non-blocking waitForRotation). \
                 Prefer action=logs/list, or attach_monitor; set timeoutMs>0 only if you must wait."
                .to_string());
        }

        let deadline = Instant::now() + timeout;
        // Cap blocking wait so a high timeoutMs cannot freeze the turn for minutes.
        let cap = Duration::from_secs(2);
        let effective_deadline = Instant::now() + timeout.min(cap);

        loop {
            {
                let state = self.state.lock().await;
                if let Some(info) = state.last_rotation.clone() {
                    return Ok(info);
                }
                if let Some(reason) = &state.failure_reason {
                    return Err(reason.clone());
                }
            }

            let remaining = effective_deadline
                .saturating_duration_since(Instant::now())
                .min(deadline.saturating_duration_since(Instant::now()));
            if remaining.is_zero() {
                return Err(
                    "Timed out waiting for log rotation (max 2s block). Use logs/list instead."
                        .to_string(),
                );
            }

            if tokio::time::timeout(remaining, self.notify.notified())
                .await
                .is_err()
            {
                return Err(
                    "Timed out waiting for log rotation (max 2s block). Use logs/list instead."
                        .to_string(),
                );
            }
        }
    }
}

fn store_rotation_observer(id: &str, observer: LogRotationObserver) {
    if let Ok(mut observers) = ROTATION_OBSERVERS.write() {
        observers.insert(id.to_string(), observer);
    }
}

fn remove_rotation_observer(id: &str) {
    if let Ok(mut observers) = ROTATION_OBSERVERS.write() {
        observers.remove(id);
    }
}
fn get_rotation_observer(id: &str) -> Result<LogRotationObserver, String> {
    let observers = ROTATION_OBSERVERS
        .read()
        .map_err(|_| "Rotation registry unavailable".to_string())?;
    if let Some(observer) = observers.get(id).cloned() {
        return Ok(observer);
    }
    drop(observers);
    let task_known = TASKS
        .read()
        .map(|tasks| tasks.contains_key(id))
        .unwrap_or(false);
    if task_known {
        return Err("Log rotation tracking unavailable for task".to_string());
    }
    Err("Task not found".to_string())
}

fn mark_log_write_failure(id: &str, reason: &str) {
    if let Ok(mut tasks) = TASKS.write() {
        if let Some(task) = tasks.get_mut(id) {
            if !task.log_write_failed {
                task.log_write_failed = true;
                task.log_write_error = Some(reason.to_string());
            }
        }
    }
}
struct RotatingLogWriter {
    log_path: PathBuf,
    limit: u64,
    segments: usize,
    current_size: u64,
    drop_all: bool,
    failed: bool,
    file: Option<tokio::fs::File>,
    observer: LogRotationObserver,
}

impl RotatingLogWriter {
    async fn new(log_path: PathBuf, limit: u64, segments: usize) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(RotationState::default()));
        let notify = Arc::new(Notify::new());
        let observer = LogRotationObserver {
            limit,
            segments,
            state,
            notify,
        };

        let mut writer = Self {
            log_path,
            limit,
            segments,
            current_size: 0,
            drop_all: limit == 0,
            failed: false,
            file: None,
            observer,
        };

        writer.initialize().await?;
        Ok(writer)
    }

    fn observer(&self) -> LogRotationObserver {
        self.observer.clone()
    }

    async fn initialize(&mut self) -> Result<(), String> {
        self.ensure_log_file().await?;
        if self.drop_all {
            return Ok(());
        }

        let existing_size = match tokio::fs::metadata(&self.log_path).await {
            Ok(meta) => meta.len(),
            Err(_) => 0,
        };
        self.current_size = if self.limit > 0 {
            existing_size.min(self.limit)
        } else {
            0
        };

        if self.limit > 0 && self.current_size >= self.limit {
            let _ = self.rotate().await?;
        }

        Ok(())
    }

    async fn ensure_log_file(&mut self) -> Result<(), String> {
        if self.file.is_some() {
            return Ok(());
        }
        if let Some(parent) = self
            .log_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                format!("Failed to create logs directory {}: {e}", parent.display())
            })?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .await
            .map_err(|e| format!("Failed to open log file: {e}"))?;
        self.file = Some(file);
        Ok(())
    }

    async fn append(&mut self, mut chunk: &[u8]) -> Result<(), String> {
        if self.drop_all || self.failed {
            return Ok(());
        }

        while !chunk.is_empty() {
            if self.current_size >= self.limit {
                let rotated = self.rotate().await?;
                if !rotated {
                    return Ok(());
                }
                continue;
            }

            let remaining_capacity = self.limit.saturating_sub(self.current_size);
            if remaining_capacity == 0 {
                return Ok(());
            }

            let to_write = remaining_capacity.min(chunk.len() as u64) as usize;
            let (head, rest) = chunk.split_at(to_write);

            self.ensure_log_file().await?;
            if let Some(file) = &mut self.file {
                file.write_all(head)
                    .await
                    .map_err(|e| format!("Failed to write log: {e}"))?;
            }
            self.current_size += head.len() as u64;
            chunk = rest;
        }

        Ok(())
    }

    async fn finish(&mut self) {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush().await;
        }

        let mut state = self.observer.state.lock().await;
        if state.last_rotation.is_none() && state.failure_reason.is_none() {
            state.failure_reason =
                Some("Log rotation did not occur before stream ended".to_string());
            drop(state);
            self.observer.notify.notify_waiters();
        }
    }

    async fn rotate(&mut self) -> Result<bool, String> {
        if self.segments == 0 {
            return Ok(false);
        }

        if let Some(mut file) = self.file.take() {
            let _ = file.flush().await;
        }

        self.shift_archives().await?;
        let archive_path = self.archive_path(1);

        match tokio::fs::rename(&self.log_path, &archive_path).await {
            Ok(()) => {
                self.ensure_log_file().await?;
                self.current_size = 0;
                let info = LogRotationInfo {
                    log_path: self.log_path.clone(),
                    archive_path: archive_path.clone(),
                    rotated_at: SystemTime::now(),
                };
                self.record_rotation(info).await;
                Ok(true)
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                self.ensure_log_file().await?;
                self.current_size = 0;
                Ok(true)
            }
            Err(err) => Err(format!("Failed to rotate log: {err}")),
        }
    }

    async fn shift_archives(&self) -> Result<(), String> {
        if self.segments == 0 {
            return Ok(());
        }
        for idx in (1..=self.segments).rev() {
            let path = self.archive_path(idx);
            if idx == self.segments {
                let _ = tokio::fs::remove_file(&path).await;
                continue;
            }
            let next = self.archive_path(idx + 1);
            let _ = tokio::fs::rename(&path, &next).await;
        }
        Ok(())
    }

    fn archive_path(&self, index: usize) -> PathBuf {
        PathBuf::from(format!("{}.{}", self.log_path.to_string_lossy(), index))
    }

    async fn record_rotation(&self, info: LogRotationInfo) {
        {
            let mut state = self.observer.state.lock().await;
            state.last_rotation = Some(info);
        }
        self.observer.notify.notify_waiters();
    }

    async fn record_failure(&mut self, reason: &str) {
        if self.failed {
            return;
        }
        self.failed = true;
        {
            let mut state = self.observer.state.lock().await;
            if state.failure_reason.is_none() {
                state.failure_reason = Some(reason.to_string());
            }
        }
        self.observer.notify.notify_waiters();
    }
}

fn logs_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(home) = std::env::var_os("MAESTRO_HOME") {
        return PathBuf::from(home).join("logs");
    }
    dirs::home_dir().map_or_else(
        || std::env::temp_dir().join("composer-logs"),
        |home| home.join(".composer").join("logs"),
    )
}

fn ensure_logs_dir() -> Result<(), String> {
    let dir = logs_dir();
    fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create logs directory {}: {}", dir.display(), e))
}

fn read_last_lines(path: &Path, lines: usize) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("Failed to open log: {e}"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| format!("Failed to read log: {e}"))?;

    let text = String::from_utf8_lossy(&buf);
    let mut collected: Vec<&str> = text.lines().collect();
    if collected.len() > lines {
        collected = collected.split_off(collected.len() - lines);
    }
    Ok(collected.join("\n"))
}

async fn drain_stream<R>(
    mut reader: R,
    writer: Arc<Mutex<RotatingLogWriter>>,
    remaining: Arc<AtomicUsize>,
    task_id: String,
    stream: &'static str,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = [0u8; 8192];
    let mut write_failed = false;
    let mut monitor_buffer = String::new();
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                monitor_buffer.push_str(&String::from_utf8_lossy(&buffer[..count]));
                while let Some(newline) = monitor_buffer.find('\n') {
                    let line = monitor_buffer[..newline].trim_end_matches('\r').to_string();
                    monitor_buffer.drain(..=newline);
                    emit_monitor_matches(&task_id, stream, &line);
                }
                if monitor_buffer.chars().count() > MAX_MONITOR_LINE_CHARS {
                    emit_monitor_matches(&task_id, stream, &monitor_buffer);
                    monitor_buffer.clear();
                }
                if write_failed {
                    continue;
                }
                let mut guard = writer.lock().await;
                if let Err(err) = guard.append(&buffer[..count]).await {
                    guard.record_failure(&err).await;
                    mark_log_write_failure(&task_id, &err);
                    write_failed = true;
                }
            }
            Err(err) => {
                let mut guard = writer.lock().await;
                let reason = format!("Log stream read failed: {err}");
                guard.record_failure(&reason).await;
                mark_log_write_failure(&task_id, &reason);
                break;
            }
        }
    }

    if !monitor_buffer.is_empty() {
        emit_monitor_matches(&task_id, stream, &monitor_buffer);
    }

    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
        let mut guard = writer.lock().await;
        guard.finish().await;
    }
}

/// Start a new background task.
///
/// # Arguments
///
/// * `command` - The command to execute
/// * `cwd` - Working directory for the process
/// * `workspace_dir` - Workspace root for config resolution
/// * `shell` - If true, run through the system shell (enables pipes, redirects)
/// * `env` - Optional additional environment variables
/// * `sandbox_policy` - If `Some`, the process is spawned under the native
///   OS sandbox with this policy (same containment as a sandboxed `bash`
///   call). Sandboxed spawns are not placed in their own process group, so
///   `stop` relies on parent-PID sweeping rather than group kills.
///
/// # Returns
///
/// The created [`BackgroundTask`] with its unique ID, or an error message.
pub async fn start(
    command: String,
    cwd: String,
    workspace_dir: String,
    shell: bool,
    env: Option<HashMap<String, String>>,
    sandbox_policy: Option<crate::sandbox::SandboxPolicy>,
) -> Result<BackgroundTask, String> {
    start_owned(
        command,
        cwd,
        workspace_dir,
        shell,
        env,
        sandbox_policy,
        None,
    )
    .await
}

/// The scope comes only from trusted host configuration, never model arguments.
pub async fn start_owned(
    command: String,
    cwd: String,
    workspace_dir: String,
    shell: bool,
    env: Option<HashMap<String, String>>,
    sandbox_policy: Option<crate::sandbox::SandboxPolicy>,
    scope: Option<BackgroundTaskScope>,
) -> Result<BackgroundTask, String> {
    spawn_task(
        command,
        cwd,
        workspace_dir,
        shell,
        env,
        sandbox_policy,
        scope,
        None,
    )
    .await
    .map(|(task, _)| task)
}

/// A script whose stdout lines become owner-scoped agent notifications.
/// Its scope comes only from trusted host configuration, never model arguments.
#[derive(Debug, Clone)]
pub struct MonitorRequest {
    pub command: String,
    pub description: Option<String>,
    pub timeout_ms: Option<u64>,
    pub cwd: String,
    pub workspace_dir: String,
    pub env: Option<HashMap<String, String>>,
    pub sandbox_policy: Option<crate::sandbox::SandboxPolicy>,
    pub scope: Option<BackgroundTaskScope>,
}

#[derive(Debug, Clone)]
pub struct MonitorStarted {
    pub task: BackgroundTask,
    pub monitor: MonitorInfo,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
struct LineWatchSpec {
    description: String,
    timeout_ms: u64,
}

/// Start a shell script under an all-lines watch with a deadline. The watch is
/// registered before the script's output is read, so no early line is missed.
/// It ends with exactly one terminal notification: exited (with its code),
/// expired, flooded, or stopped.
pub async fn start_monitor(request: MonitorRequest) -> Result<MonitorStarted, String> {
    let timeout_ms = line_watch::effective_timeout_ms(request.timeout_ms);
    let description = redact_text(&line_watch::bounded_description(
        request.description.as_deref(),
        &request.command,
    ));
    let (task, monitor) = spawn_task(
        request.command,
        request.cwd,
        request.workspace_dir,
        true,
        request.env,
        request.sandbox_policy,
        request.scope,
        Some(LineWatchSpec {
            description,
            timeout_ms,
        }),
    )
    .await?;
    let monitor = monitor.ok_or_else(|| "Monitor was not registered".to_string())?;
    Ok(MonitorStarted {
        task,
        monitor,
        timeout_ms,
    })
}

#[allow(clippy::too_many_arguments)] // Internal: the two public entry points name each input.
async fn spawn_task(
    command: String,
    cwd: String,
    workspace_dir: String,
    shell: bool,
    env: Option<HashMap<String, String>>,
    sandbox_policy: Option<crate::sandbox::SandboxPolicy>,
    scope: Option<BackgroundTaskScope>,
    watch: Option<LineWatchSpec>,
) -> Result<(BackgroundTask, Option<MonitorInfo>), String> {
    if scope.as_ref().is_some_and(|scope| !scope.is_valid()) {
        return Err("Invalid background task scope".to_string());
    }
    // Apply the same dangerous-command analysis as the bash tool; background
    // tasks bypass approval flows, so high-severity commands must be blocked
    // here rather than approved.
    if let Some(warning) = BashTool::is_dangerous(&command) {
        return Err(format!("Dangerous command blocked: {warning}"));
    }
    let patterns = check_dangerous_patterns(&command);
    if let Some(pattern) = patterns
        .iter()
        .find(|pattern| pattern.severity == Severity::High)
    {
        return Err(format!(
            "Dangerous command blocked: {}: {}",
            pattern.description, pattern.matched_text
        ));
    }

    let max_running = max_running_tasks();
    let running = running_task_count();
    if running >= max_running {
        return Err(format!(
            "Too many running background tasks ({running}/{max_running}). \
             Stop one with action=stop, or raise MAESTRO_BACKGROUND_MAX_RUNNING_TASKS."
        ));
    }

    ensure_logs_dir()?;
    let id = Uuid::new_v4().to_string();
    // Reserve the watch before the child exists so its first line is seen.
    let mut watch_wake = None;
    let reservation = match watch {
        Some(spec) => {
            let line_watch = LineWatch::new(
                scope.clone(),
                spec.description.clone(),
                spec.timeout_ms,
                Instant::now(),
            );
            watch_wake = Some((line_watch.wake.clone(), spec.timeout_ms));
            let info = insert_monitor(
                &id,
                &spec.description,
                MonitorMatcher::Lines(Box::new(line_watch)),
            )?;
            Some((MonitorReservation(Some(info.id.clone())), info))
        }
        None => None,
    };
    let log_path = logs_dir().join(format!("background-{id}.log"));
    let (log_limit, log_segments) = log_limits();
    let log_writer = RotatingLogWriter::new(log_path.clone(), log_limit, log_segments).await?;
    let log_writer = Arc::new(Mutex::new(log_writer));
    let observer = { log_writer.lock().await.observer() };

    let resolved_env = resolve_shell_environment(Path::new(&workspace_dir), env.as_ref());

    let mut child = if let Some(policy) = &sandbox_policy {
        // Spawn under the native OS sandbox, same as a sandboxed `bash`
        // call: a session advertised as read-only or workspace-write must
        // not be escapable by routing a long-lived command through
        // `background_tasks` instead. `spawn_sandboxed_command` pipes all
        // three stdio streams; stdin is dropped immediately so the child
        // sees EOF, matching the unsandboxed `Stdio::null()` below.
        let argv = if shell {
            let (shell_path, shell_args) =
                resolve_shell_config().map_err(|e| format!("Shell unavailable: {e}"))?;
            let mut argv = vec![shell_path];
            argv.extend(shell_args);
            argv.push(command.clone());
            argv
        } else {
            let parts = shlex::split(&command)
                .ok_or_else(|| "Failed to parse command arguments".to_string())?;
            if parts.is_empty() {
                return Err("Empty command".to_string());
            }
            parts
        };
        let mut child = crate::sandbox::spawn_sandboxed_command(
            argv,
            PathBuf::from(&cwd),
            policy,
            resolved_env,
        )
        .await
        .map_err(|e| format!("Failed to spawn background task: {e}"))?;
        drop(child.stdin.take());
        child
    } else {
        let mut cmd = if shell {
            let (shell_path, shell_args) =
                resolve_shell_config().map_err(|e| format!("Shell unavailable: {e}"))?;
            let mut cmd = Command::new(shell_path);
            cmd.args(shell_args).arg(command.clone());
            cmd
        } else {
            let parts = shlex::split(&command)
                .ok_or_else(|| "Failed to parse command arguments".to_string())?;
            if parts.is_empty() {
                return Err("Empty command".to_string());
            }
            let mut cmd = Command::new(&parts[0]);
            if parts.len() > 1 {
                cmd.args(&parts[1..]);
            }
            cmd
        };

        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.env_clear();
        cmd.envs(resolved_env);

        set_new_process_group(&mut cmd);

        cmd.spawn()
            .map_err(|e| format!("Failed to spawn background task: {e}"))?
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stream_count = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
    let remaining = Arc::new(AtomicUsize::new(stream_count.max(1)));

    let mut drain_handles = Vec::with_capacity(stream_count);
    if let Some(out) = stdout {
        drain_handles.push(tokio::spawn(drain_stream(
            out,
            log_writer.clone(),
            remaining.clone(),
            id.clone(),
            "stdout",
        )));
    }
    if let Some(err) = stderr {
        drain_handles.push(tokio::spawn(drain_stream(
            err,
            log_writer.clone(),
            remaining.clone(),
            id.clone(),
            "stderr",
        )));
    }
    if stream_count == 0 {
        let mut guard = log_writer.lock().await;
        guard.finish().await;
    }

    let pid = child.id();
    if let Some(pid) = pid {
        process_registry::register(pid);
    }

    let task = BackgroundTask {
        scope,
        completion_sequence: 0,
        id: id.clone(),
        pid,
        command: command.clone(),
        cwd,
        log_path: log_path.to_string_lossy().to_string(),
        log_write_failed: false,
        log_write_error: None,
        status: BackgroundTaskStatus::Running,
        started_at: SystemTime::now(),
        finished_at: None,
        exit_code: None,
    };

    if let Ok(mut tasks) = TASKS.write() {
        tasks.insert(id.clone(), task.clone());
        prune_completed_tasks(&mut tasks);
    }
    store_rotation_observer(&id, observer);
    let monitor = reservation.map(|(reservation, info)| {
        // Accepted: from here the task's lifecycle ends the watch.
        let _ = reservation.accept();
        info
    });
    if let (Some(info), Some((wake, timeout_ms))) = (&monitor, watch_wake) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        tokio::spawn(run_line_watch(info.id.clone(), id.clone(), wake, deadline));
    }

    // Track completion — never block the agent turn on waitForRotation; push
    // lifecycle events instead (Kimi TaskOutput non-blocking completion notify).
    let lifecycle_command = command.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        let (exit_code, failed) = match status {
            Ok(status) => (status.code().unwrap_or(-1), !status.success()),
            Err(_) => (-1, true),
        };
        let status_label = if failed { "failed" } else { "exited" };

        if let Ok(mut tasks) = TASKS.write() {
            if let Some(existing) = tasks.get_mut(&id) {
                if existing.scope.is_some() && existing.completion_sequence == 0 {
                    existing.completion_sequence = COMPLETION_SEQUENCE
                        .fetch_add(1, Ordering::SeqCst)
                        .saturating_add(1);
                }
                existing.finished_at = existing.finished_at.or(Some(SystemTime::now()));
                existing.exit_code = Some(exit_code);
                if !matches!(existing.status, BackgroundTaskStatus::Stopped) {
                    existing.status = if failed {
                        BackgroundTaskStatus::Failed
                    } else {
                        BackgroundTaskStatus::Exited
                    };
                }
            }
        }
        if let Ok(mut tasks) = TASKS.write() {
            prune_completed_tasks(&mut tasks);
        }
        emit_task_lifecycle(&id, &lifecycle_command, status_label, Some(exit_code));
        for handle in drain_handles {
            let _ = handle.await;
        }
        // Every stdout line is batched by now; a stopped watch already ended.
        close_line_watches(&id, Some(WatchEnd::Exited(Some(exit_code))));
        remove_task_monitors(&id);
        remove_rotation_observer(&id);

        if let Some(pid) = pid {
            process_registry::unregister(pid);
        }
    });

    if task.scope.is_some() {
        if let Err(error) = persist_task_snapshot() {
            // Not accepted: end any watch without notifying the owner.
            let _ = stop_with_end(&task.id, None);
            return Err(format!(
                "Could not save background task acceptance: {error}"
            ));
        }
    } else {
        persist_running_snapshot();
    }
    Ok((task, monitor))
}

/// List all background tasks.
///
/// Returns a snapshot of all tasks, including completed ones.
pub fn list() -> Vec<BackgroundTask> {
    TASKS
        .read()
        .map(|tasks| tasks.values().cloned().collect())
        .unwrap_or_default()
}

/// Stop a running background task.
///
/// Kills the process and its children, then marks the task as stopped.
///
/// # Arguments
///
/// * `id` - The task ID returned from [`start`]
///
/// # Errors
///
/// Returns an error if the task is not found.
pub fn stop(id: &str) -> Result<BackgroundTask, String> {
    stop_with_end(id, Some(WatchEnd::Stopped))
}

/// Stop a task and end its line watches with `end` (`None` ends them silently).
/// Lock order: TASKS, then MONITORS (inside `take_line_watches`); delivery runs after both are released.
fn stop_with_end(id: &str, end: Option<WatchEnd>) -> Result<BackgroundTask, String> {
    let mut tasks = TASKS
        .write()
        .map_err(|_| "Task registry unavailable".to_string())?;

    let task = tasks
        .get_mut(id)
        .ok_or_else(|| "Task not found".to_string())?;
    // Restored terminal metadata retains a historical PID. Never signal that
    // PID: the operating system may have reused it for an unrelated process.
    if !matches!(task.status, BackgroundTaskStatus::Running) {
        let finished = task.clone();
        drop(tasks);
        // The script already exited while its output drained: that is the end.
        if end.is_some() {
            close_line_watches(id, Some(WatchEnd::Exited(finished.exit_code)));
        }
        return Ok(finished);
    }
    if task.scope.is_some() && task.completion_sequence == 0 {
        task.completion_sequence = COMPLETION_SEQUENCE
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
    }

    if let Some(pid) = task.pid {
        super::process_utils::kill_process_tree(pid);
        process_registry::unregister(pid);
    }
    task.status = BackgroundTaskStatus::Stopped;
    task.finished_at = Some(SystemTime::now());
    let stopped = task.clone();
    prune_completed_tasks(&mut tasks);
    remove_rotation_observer(id);
    let watches = take_line_watches(id);
    remove_task_monitors(id);
    drop(tasks);
    end_line_watches(watches, end);
    emit_task_lifecycle(&stopped.id, &stopped.command, "stopped", stopped.exit_code);

    Ok(stopped)
}

/// Retrieve the last N lines from a task's log file.
///
/// # Arguments
///
/// * `id` - The task ID returned from [`start`]
/// * `lines` - Maximum number of lines to retrieve
///
/// # Errors
///
/// Returns an error if the task is not found or the log file cannot be read.
pub fn logs(id: &str, lines: usize) -> Result<String, String> {
    let tasks = TASKS
        .read()
        .map_err(|_| "Task registry unavailable".to_string())?;
    let task = tasks.get(id).ok_or_else(|| "Task not found".to_string())?;
    read_last_lines(Path::new(&task.log_path), lines)
}

pub async fn wait_for_rotation(id: &str, timeout: Duration) -> Result<LogRotationInfo, String> {
    let observer = get_rotation_observer(id)?;
    observer.wait_for_rotation(timeout).await
}

#[cfg(test)]
mod tests;
