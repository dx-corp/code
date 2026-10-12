//! Local A2A task ledger, backed by SQLite and JSON-boundary compatible.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::{A2ATask, A2ATaskStatus, extract_task_text, is_final_state};
use maestro_local_host::path_utils::{env_path, maestro_home_dir, resolve_env_path};

// Keep these values and metadata names in lockstep with
// runtime-gateway-rs/src/a2a/ledger.rs. Both writers claim the same directory
// atomically, so changing one side alone would reintroduce lost updates.
const A2A_LEDGER_LOCK_RETRY_MS: u64 = 25;
const A2A_LEDGER_LOCK_STALE_MS: u64 = 30_000;
const A2A_LEDGER_LOCK_TIMEOUT_MS: u64 = A2A_LEDGER_LOCK_STALE_MS + A2A_LEDGER_LOCK_RETRY_MS;
const A2A_LEDGER_LOCK_OWNER_FILE: &str = "owner";
const A2A_LEDGER_LOCK_HEARTBEAT_FILE: &str = "heartbeat";
static A2A_LEDGER_LOCK_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Bound on manual symlink following in `resolve_task_ledger_path`, so a
/// cyclic alias reports an error instead of looping forever.
const MAX_TASK_LEDGER_SYMLINK_HOPS: usize = 32;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskLedgerFile {
    #[serde(default)]
    pub tasks: Vec<TaskLedgerEntry>,
    /// Provider-specific delegation bindings are kept alongside the legacy
    /// task projection so a restart can recover accepted Orb work without
    /// replaying the original request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orb_delegations: Vec<OrbDelegationEntry>,
    /// Future Gateway projections and provider extensions are preserved when
    /// the TUI rewrites the shared snapshot. Known fields always remain the
    /// typed fields above; this map is populated only with unknown keys.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// The durable lifecycle projection used by Maestro for an Orb-backed
/// delegation. This is intentionally coarser than either A2A or Orb's
/// provider-local states: it is the state the local recovery loop can safely
/// act on after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrbDelegationState {
    Starting,
    Running,
    Waiting,
    Terminal,
    Unavailable,
}

impl OrbDelegationState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }

    pub const fn is_nonterminal(self) -> bool {
        !self.is_terminal()
    }
}

/// The provider-neutral identity mapping persisted before an Orb mutation is
/// attempted. It contains no prompt, transcript, access token, or secret;
/// `request_digest` is supplied by the caller and must be a digest rather than
/// the request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrbDelegationEntry {
    pub maestro_delegation_id: String,
    pub maestro_session_id: String,
    pub tenant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orb_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orb_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_receipt_id: Option<String>,
    pub operation_id: String,
    pub idempotency_key: String,
    pub request_digest: String,
    pub state: OrbDelegationState,
    #[serde(default)]
    pub observed_revision: u64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<String>,
    /// Provider-specific fields survive future TUI/Gateway round trips.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// The minimum identity needed to create or replay a local Orb binding.
/// Callers persist this record before sending the corresponding remote
/// mutation; a duplicate with the same tenant-scoped idempotency identity is
/// returned as a replay instead of creating a second ledger row.
pub struct OrbDelegationStartInput<'a> {
    pub path: Option<&'a str>,
    pub maestro_delegation_id: &'a str,
    pub maestro_session_id: &'a str,
    pub tenant_id: &'a str,
    pub connection_ref: Option<&'a str>,
    pub orb_thread_id: Option<&'a str>,
    pub orb_task_id: Option<&'a str>,
    pub launch_receipt_id: Option<&'a str>,
    pub operation_id: &'a str,
    pub idempotency_key: &'a str,
    pub request_digest: &'a str,
}

/// Provider state observed during recovery. The mapping deliberately avoids
/// depending on Orb crate types so Maestro can recover from a hosted-service
/// fixture or a Platform observation client without starting a local Orb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrbObservedState {
    LaunchStaged,
    LaunchStarting,
    LaunchReplaying,
    LaunchReady,
    TaskRunning,
    TaskWaiting,
    TaskTerminal,
    LaunchFailed { retryable: bool },
    Unavailable,
}

impl OrbObservedState {
    pub const fn delegation_state(self) -> OrbDelegationState {
        match self {
            Self::LaunchStaged | Self::LaunchStarting | Self::LaunchReplaying => {
                OrbDelegationState::Starting
            }
            Self::LaunchReady | Self::TaskRunning => OrbDelegationState::Running,
            Self::TaskWaiting => OrbDelegationState::Waiting,
            Self::TaskTerminal => OrbDelegationState::Terminal,
            Self::LaunchFailed { retryable: true } | Self::Unavailable => {
                OrbDelegationState::Unavailable
            }
            Self::LaunchFailed { retryable: false } => OrbDelegationState::Terminal,
        }
    }
}

/// A provider observation is keyed to the same tenant and may fill in remote
/// IDs that were not available when the local start record was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrbDelegationObservation {
    pub tenant_id: String,
    pub connection_ref: Option<String>,
    pub orb_thread_id: Option<String>,
    pub orb_task_id: Option<String>,
    pub launch_receipt_id: Option<String>,
    pub state: OrbObservedState,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrbDelegationStartOutcome {
    Created,
    Replayed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrbRecoveryReport {
    pub inspected: usize,
    pub updated: usize,
    pub terminal: usize,
    pub unavailable: usize,
}

/// Recovery is intentionally an observation-only boundary. Implementations
/// may query the hosted Orb/Platform lookup path, but there is no launch or
/// dispatch method here that a retry could accidentally call twice.
pub trait OrbDelegationObserver {
    fn observe(
        &mut self,
        delegation: &OrbDelegationEntry,
    ) -> Result<Option<OrbDelegationObservation>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskLedgerEntry {
    pub id: String,
    pub kind: String,
    pub peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_display_name: Option<String>,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_graph: Option<Value>,
    #[serde(default)]
    pub transcript: Vec<TranscriptEntry>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Gateway task projections not known to this TUI release are retained.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEntry {
    pub at: String,
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

const TASK_LEDGER_FILE_FIELDS: &[&str] = &["tasks", "orbDelegations"];
const ORB_DELEGATION_FIELDS: &[&str] = &[
    "maestroDelegationId",
    "maestroSessionId",
    "tenantId",
    "connectionRef",
    "orbThreadId",
    "orbTaskId",
    "launchReceiptId",
    "operationId",
    "idempotencyKey",
    "requestDigest",
    "state",
    "observedRevision",
    "createdAt",
    "updatedAt",
    "terminalAt",
];
const TASK_LEDGER_ENTRY_FIELDS: &[&str] = &[
    "id",
    "kind",
    "peer",
    "peerDisplayName",
    "taskId",
    "contextId",
    "messageId",
    "text",
    "role",
    "cwd",
    "state",
    "responseText",
    "metadata",
    "workGraph",
    "transcript",
    "createdAt",
    "updatedAt",
    "completedAt",
];
const TRANSCRIPT_FIELDS: &[&str] = &["at", "role", "text", "state", "messageId"];

fn unknown_ledger_fields(
    object: &serde_json::Map<String, Value>,
    known_fields: &[&str],
) -> BTreeMap<String, Value> {
    object
        .iter()
        .filter(|(key, _)| !known_fields.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn get_task_ledger_path(path: Option<&str>) -> Result<PathBuf> {
    resolve_task_ledger_location(path).map(|location| location.path)
}

struct TaskLedgerLocation {
    path: PathBuf,
    secure_new_default_parent: bool,
}

fn resolve_task_ledger_location(path: Option<&str>) -> Result<TaskLedgerLocation> {
    let configured = if let Some(configured) = path
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env_path("MAESTRO_A2A_TASKS_FILE"))
        .or_else(|| env_path("CODEX_A2A_TASKS_FILE"))
    {
        TaskLedgerLocation {
            path: resolve_env_path(&configured.to_string_lossy()).unwrap_or(configured),
            secure_new_default_parent: false,
        }
    } else {
        TaskLedgerLocation {
            path: maestro_home_dir()
                .context("Maestro home is unavailable")?
                .join("a2a")
                .join("tasks.json"),
            secure_new_default_parent: true,
        }
    };
    Ok(TaskLedgerLocation {
        path: resolve_task_ledger_path(&configured.path)?,
        secure_new_default_parent: configured.secure_new_default_parent,
    })
}

/// Resolve aliases before deriving the lock path so every process that names
/// the same ledger (including a symlinked path) contends on one directory.
/// Missing final files are allowed. A symlink whose target does not exist is
/// followed by hand rather than rejected: the SQLite ledger never creates the
/// JSON boundary path, so an alias naming it is normally dangling and must
/// still resolve to the target's lock and database.
fn resolve_task_ledger_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve relative A2A task ledger path")?
            .join(path)
    };
    let mut missing = Vec::<OsString>::new();
    let mut cursor = absolute;
    let mut hops = 0usize;
    loop {
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => match dunce::canonicalize(&cursor) {
                Ok(mut resolved) => {
                    for component in missing.iter().rev() {
                        resolved.push(component);
                    }
                    if missing.is_empty() && metadata.is_dir() {
                        bail!("A2A task ledger path {} is a directory", path.display());
                    }
                    return Ok(resolved);
                }
                Err(error) if error.kind() == ErrorKind::NotFound && metadata.is_symlink() => {
                    hops += 1;
                    if hops > MAX_TASK_LEDGER_SYMLINK_HOPS {
                        bail!(
                            "A2A task ledger path {} exceeds the symlink hop limit",
                            path.display()
                        );
                    }
                    let target = fs::read_link(&cursor).with_context(|| {
                        format!("resolve A2A task ledger path {}", cursor.display())
                    })?;
                    cursor = match cursor.parent() {
                        Some(parent) if target.is_relative() => parent.join(target),
                        _ => target,
                    };
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("resolve A2A task ledger path {}", cursor.display())
                    });
                }
            },
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let component = cursor
                    .file_name()
                    .with_context(|| format!("resolve A2A task ledger path {}", path.display()))?
                    .to_os_string();
                missing.push(component);
                cursor = cursor
                    .parent()
                    .with_context(|| format!("resolve A2A task ledger parent {}", path.display()))?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect A2A task ledger path {}", cursor.display()));
            }
        }
    }
}

pub fn load_task_ledger(path: Option<&str>) -> Result<TaskLedgerFile> {
    let path = get_task_ledger_path(path)?;
    load_task_ledger_at(&path)
}

fn load_task_ledger_at(path: &Path) -> Result<TaskLedgerFile> {
    let parsed = maestro_a2a_ledger::load(path)
        .with_context(|| format!("load A2A task database for {}", path.display()))?;
    task_ledger_from_document(&parsed, path)
}

/// Project a stored ledger document onto the typed snapshot. Rows written
/// before the SQLite move may omit `id`, `kind` or `updatedAt`, and
/// `import_json_once` copies them in verbatim, so every reader normalizes
/// instead of deserializing strictly. The write transaction uses this too,
/// which is what migrates a legacy row on its first mutation.
fn task_ledger_from_document(parsed: &Value, path: &Path) -> Result<TaskLedgerFile> {
    let obj = parsed.as_object().with_context(|| {
        format!(
            "A2A task ledger at {} must be a JSON object",
            path.display()
        )
    })?;
    let tasks = obj
        .get("tasks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut entries = Vec::with_capacity(tasks.len());
    for (index, task) in tasks.iter().enumerate() {
        entries.push(normalize_ledger_entry(task, &format!("tasks[{index}]"))?);
    }
    let mut orb_delegations: Vec<OrbDelegationEntry> = match obj.get("orbDelegations") {
        Some(value) if !value.is_null() => serde_json::from_value(value.clone())
            .with_context(|| format!("parse Computer delegation bindings in {}", path.display()))?,
        _ => Vec::new(),
    };
    for entry in &mut orb_delegations {
        entry
            .extensions
            .retain(|key, _| !ORB_DELEGATION_FIELDS.contains(&key.as_str()));
    }
    Ok(TaskLedgerFile {
        tasks: entries,
        orb_delegations,
        extensions: unknown_ledger_fields(obj, TASK_LEDGER_FILE_FIELDS),
    })
}

/// Seed or replace a complete ledger snapshot while holding the shared lock.
/// Runtime mutations should use the transaction helpers below so the caller
/// cannot accidentally persist a stale in-memory snapshot over another
/// process's rows.
#[cfg(test)]
fn save_task_ledger(ledger: &TaskLedgerFile, path: Option<&str>) -> Result<PathBuf> {
    let location = resolve_task_ledger_location(path)?;
    ensure_ledger_directory(&location.path, location.secure_new_default_parent)?;
    with_task_ledger_lock_at(&location.path, |_, lock| {
        save_task_ledger_at(ledger, &location.path, lock)
    })
}

#[cfg(test)]
fn save_task_ledger_at(
    ledger: &TaskLedgerFile,
    path: &Path,
    lock: &TaskLedgerLock,
) -> Result<PathBuf> {
    ensure_ledger_directory(path, false)?;
    let mut persisted = ledger.clone();
    sanitize_ledger_extensions(&mut persisted);
    ensure_task_ledger_lock_owned(lock)?;
    let value = serde_json::to_value(persisted)?;
    maestro_a2a_ledger::update(path, |document| {
        *document = value;
        Ok(())
    })?;
    Ok(path.to_path_buf())
}

fn sanitize_ledger_extensions(ledger: &mut TaskLedgerFile) {
    ledger
        .extensions
        .retain(|key, _| !TASK_LEDGER_FILE_FIELDS.contains(&key.as_str()));
    for task in &mut ledger.tasks {
        task.extensions
            .retain(|key, _| !TASK_LEDGER_ENTRY_FIELDS.contains(&key.as_str()));
        for transcript in &mut task.transcript {
            transcript
                .extensions
                .retain(|key, _| !TRANSCRIPT_FIELDS.contains(&key.as_str()));
        }
    }
    for delegation in &mut ledger.orb_delegations {
        delegation
            .extensions
            .retain(|key, _| !ORB_DELEGATION_FIELDS.contains(&key.as_str()));
    }
}

fn ensure_ledger_directory(path: &Path, secure_new_default_parent: bool) -> Result<()> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    let mut created_default_parent = false;
    if secure_new_default_parent {
        if let Some(ancestor) = parent.parent() {
            fs::create_dir_all(ancestor).with_context(|| {
                format!("create ledger directory ancestor {}", ancestor.display())
            })?;
        }
        match fs::create_dir(parent) {
            Ok(()) => created_default_parent = true,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create ledger directory {}", parent.display()));
            }
        }
    } else {
        fs::create_dir_all(parent)
            .with_context(|| format!("create ledger directory {}", parent.display()))?;
    }
    #[cfg(unix)]
    if created_default_parent {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!("restrict ledger directory permissions {}", parent.display())
        })?;
    }
    #[cfg(not(unix))]
    let _ = created_default_parent;
    Ok(())
}

fn ledger_lock_path(path: &Path) -> PathBuf {
    let mut lock_name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

fn with_task_ledger_lock_at<T>(
    path: &Path,
    operation: impl FnOnce(&Path, &TaskLedgerLock) -> Result<T>,
) -> Result<T> {
    ensure_ledger_directory(path, false)?;
    let lock = acquire_task_ledger_lock(path)?;
    operation(path, &lock)
}

/// Run one ledger read-modify-write transaction under the shared Gateway/TUI
/// directory lock protocol. The lock is a `<ledger>.lock` directory containing
/// only an owner token and heartbeat timestamp, so a process crash leaves a
/// stale marker that the next writer can reclaim without ever treating a
/// regular file as a compatible lock.
fn with_locked_task_ledger<T>(
    path: Option<&str>,
    operation: impl FnOnce(&mut TaskLedgerFile) -> Result<T>,
) -> Result<T> {
    let location = resolve_task_ledger_location(path)?;
    ensure_ledger_directory(&location.path, location.secure_new_default_parent)?;
    with_locked_task_ledger_at(&location.path, operation)
}

fn with_locked_task_ledger_at<T>(
    path: &Path,
    operation: impl FnOnce(&mut TaskLedgerFile) -> Result<T>,
) -> Result<T> {
    ensure_ledger_directory(path, false)?;
    maestro_a2a_ledger::update(path, |document| {
        let mut ledger = task_ledger_from_document(document, path)?;
        let value = operation(&mut ledger)?;
        sanitize_ledger_extensions(&mut ledger);
        *document = serde_json::to_value(ledger)?;
        Ok(value)
    })
}

struct TaskLedgerLock {
    path: PathBuf,
    token: String,
    /// Advisory lock on the owner inode. A stale reclaimer must acquire this
    /// same fence before removing the directory, so a heartbeat/ownership
    /// check cannot race the writer's atomic replacement.
    owner_file: Option<fs::File>,
    stop: Option<Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
    #[cfg(test)]
    release_probe: Option<TaskLedgerReleaseProbe>,
}

#[cfg(test)]
struct TaskLedgerReleaseProbe {
    checked: Sender<()>,
    resume: mpsc::Receiver<()>,
}

impl Drop for TaskLedgerLock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if task_ledger_lock_is_owned(&self.path, &self.token) {
            #[cfg(test)]
            if let Some(probe) = self.release_probe.take() {
                let _ = probe.checked.send(());
                let _ = probe.resume.recv();
            }
            let _ = fs::remove_dir_all(&self.path);
        }
        // Keep the owner inode fence held until remove_dir_all has completed.
        // Otherwise a waiter can reclaim the old inode, create a new lock at
        // the same path, and have this release remove that new owner's lock.
        drop(self.owner_file.take());
    }
}

fn acquire_task_ledger_lock(path: &Path) -> Result<TaskLedgerLock> {
    ensure_ledger_directory(path, false)?;
    let lock_path = ledger_lock_path(path);
    let token = format!(
        "{}:{}",
        process::id(),
        A2A_LEDGER_LOCK_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let deadline = Instant::now() + Duration::from_millis(A2A_LEDGER_LOCK_TIMEOUT_MS);
    loop {
        match fs::create_dir(&lock_path) {
            Ok(()) => {
                if let Err(error) = restrict_lock_directory(&lock_path)
                    .and_then(|()| write_task_ledger_lock_metadata(&lock_path, &token))
                {
                    let _ = fs::remove_dir_all(&lock_path);
                    return Err(error);
                }
                // A contender can observe the owner file between its
                // creation and the creator's first non-blocking flock. Keep
                // the directory owner and retry rather than deleting a lock
                // whose inode another process may already have fenced.
                let owner_file = loop {
                    match try_acquire_task_ledger_owner_fence(&lock_path) {
                        Ok(Some(owner_file)) => break owner_file,
                        Ok(None) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS));
                        }
                        Ok(None) => {
                            bail!(
                                "timed out acquiring A2A task ledger lock {} owner fence",
                                lock_path.display()
                            );
                        }
                        Err(error) => {
                            let _ = fs::remove_dir_all(&lock_path);
                            return Err(error);
                        }
                    }
                };
                let (stop, receiver) = mpsc::channel();
                let heartbeat_path = lock_path.clone();
                let heartbeat_token = token.clone();
                let heartbeat = thread::spawn(move || {
                    let interval = Duration::from_millis(
                        (A2A_LEDGER_LOCK_STALE_MS / 3)
                            .max(A2A_LEDGER_LOCK_RETRY_MS)
                            .max(1),
                    );
                    loop {
                        match receiver.recv_timeout(interval) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if !task_ledger_lock_is_owned(&heartbeat_path, &heartbeat_token) {
                                    break;
                                }
                                if write_task_ledger_lock_heartbeat(&heartbeat_path).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                return Ok(TaskLedgerLock {
                    path: lock_path,
                    token,
                    owner_file: Some(owner_file),
                    stop: Some(stop),
                    heartbeat: Some(heartbeat),
                    #[cfg(test)]
                    release_probe: None,
                });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let metadata = match fs::symlink_metadata(&lock_path) {
                    Ok(metadata) => metadata,
                    // The owner released the lock between our create_dir
                    // attempt and this probe, so the path is free again.
                    // Retry the atomic create instead of failing the write.
                    Err(metadata_error) if metadata_error.kind() == ErrorKind::NotFound => {
                        if Instant::now() >= deadline {
                            bail!(
                                "timed out waiting for A2A task ledger lock {}",
                                lock_path.display()
                            );
                        }
                        continue;
                    }
                    Err(metadata_error) => {
                        return Err(metadata_error).with_context(|| {
                            format!("inspect A2A task ledger lock {}", lock_path.display())
                        });
                    }
                };
                if !metadata.is_dir() {
                    bail!(
                        "A2A task ledger lock {} is not the shared directory protocol",
                        lock_path.display()
                    );
                }
                let owner_file = try_acquire_task_ledger_owner_fence(&lock_path)?;
                if owner_file.is_none() && lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE).exists() {
                    // Another live writer still holds the inode fence, even
                    // if its heartbeat metadata appears stale. Waiting here
                    // prevents reclaiming it between the final ownership
                    // check and atomic ledger rename.
                    if Instant::now() >= deadline {
                        bail!(
                            "timed out waiting for A2A task ledger lock {}",
                            lock_path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS));
                    continue;
                }
                if task_ledger_lock_is_stale(&lock_path)? {
                    // Keep the acquired owner fence alive until the directory
                    // is removed. A live owner cannot be reclaimed because it
                    // owns this same advisory inode lock.
                    match fs::remove_dir_all(&lock_path) {
                        Ok(()) => continue,
                        Err(error) if error.kind() == ErrorKind::NotFound => continue,
                        Err(error) => {
                            break Err(error).with_context(|| {
                                format!("remove stale A2A task ledger lock {}", lock_path.display())
                            });
                        }
                    }
                }
                if Instant::now() >= deadline {
                    bail!(
                        "timed out waiting for A2A task ledger lock {}",
                        lock_path.display()
                    );
                }
                thread::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("acquire A2A task ledger lock {}", lock_path.display())
                });
            }
        }
    }
}

fn try_acquire_task_ledger_owner_fence(lock_path: &Path) -> Result<Option<fs::File>> {
    let owner_path = lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE);
    let file = match OpenOptions::new().read(true).write(true).open(&owner_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("open A2A task ledger lock fence {}", owner_path.display())
            });
        }
    };
    #[cfg(unix)]
    {
        // `LOCK_NB` is important here: this helper runs in the writer's
        // retry loop and must never block the process while an old owner is
        // still alive. Both TUI and Gateway use this exact owner inode fence.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Some(file));
        }
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK)
        {
            return Ok(None);
        }
        Err(error).with_context(|| {
            format!(
                "acquire A2A task ledger lock fence {}",
                owner_path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        // The directory protocol and token revalidation remain the fallback
        // on platforms without an advisory flock primitive.
        Ok(Some(file))
    }
}

fn restrict_lock_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "restrict A2A task ledger lock permissions {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn write_task_ledger_lock_metadata(lock_path: &Path, token: &str) -> Result<()> {
    write_private_lock_file(
        &lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE),
        &format!("{token}\n"),
    )?;
    write_task_ledger_lock_heartbeat(lock_path)
}

fn write_private_lock_file(path: &Path, content: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options
        .open(path)
        .with_context(|| format!("open A2A task ledger lock metadata {}", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("write A2A task ledger lock metadata {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!("restrict A2A task ledger lock metadata {}", path.display())
        })?;
    }
    Ok(())
}

fn write_task_ledger_lock_heartbeat(lock_path: &Path) -> Result<()> {
    write_private_lock_file(
        &lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE),
        &format!("{}\n", unix_millis_now()),
    )
}

fn task_ledger_lock_is_owned(lock_path: &Path, token: &str) -> bool {
    fs::read_to_string(lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE))
        .map(|owner| owner.trim() == token)
        .unwrap_or(false)
}

#[cfg(test)]
fn ensure_task_ledger_lock_owned(lock: &TaskLedgerLock) -> Result<()> {
    if task_ledger_lock_is_owned(&lock.path, &lock.token) {
        return Ok(());
    }
    bail!(
        "lost A2A task ledger lock ownership before replacing {}",
        lock.path.display()
    );
}

fn task_ledger_lock_is_stale(lock_path: &Path) -> Result<bool> {
    for path in [
        lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE),
        lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE),
        lock_path.to_path_buf(),
    ] {
        match fs::metadata(&path) {
            Ok(metadata) => {
                let modified_at = metadata.modified().with_context(|| {
                    format!("inspect A2A task ledger lock metadata {}", path.display())
                })?;
                return Ok(SystemTime::now()
                    .duration_since(modified_at)
                    .map(|age| age > Duration::from_millis(A2A_LEDGER_LOCK_STALE_MS))
                    .unwrap_or(false));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect A2A task ledger lock metadata {}", path.display())
                });
            }
        }
    }
    Ok(true)
}

fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

pub fn list_task_entries<'a>(
    ledger: &'a TaskLedgerFile,
    peer: Option<&str>,
) -> Vec<&'a TaskLedgerEntry> {
    let peer = peer.map(str::trim).filter(|s| !s.is_empty());
    let mut entries: Vec<_> = ledger
        .tasks
        .iter()
        .filter(|entry| match peer {
            None => true,
            Some(p) => entry.peer == p,
        })
        .collect();
    entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    entries
}

/// Persist the local acceptance record before the caller performs its remote
/// Orb mutation. The same tenant-scoped idempotency identity is a replay and
/// cannot append a second delegation row.
pub fn record_orb_delegation_start(
    input: OrbDelegationStartInput<'_>,
) -> Result<OrbDelegationStartOutcome> {
    with_locked_task_ledger(input.path, |ledger| {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        upsert_orb_delegation(ledger, input, &now)
    })
}

/// In-memory form of [`record_orb_delegation_start`] used by restart/replay
/// tests and by callers that already own a loaded ledger.
pub fn upsert_orb_delegation(
    ledger: &mut TaskLedgerFile,
    input: OrbDelegationStartInput<'_>,
    now: &str,
) -> Result<OrbDelegationStartOutcome> {
    let maestro_delegation_id =
        required_identity(input.maestro_delegation_id, "Maestro delegation id")?;
    let maestro_session_id = required_identity(input.maestro_session_id, "Maestro session id")?;
    let tenant_id = required_identity(input.tenant_id, "Computer tenant id")?;
    let connection_ref = optional_identity(input.connection_ref, "Computer connection reference")?;
    let orb_thread_id = optional_identity(input.orb_thread_id, "Computer thread id")?;
    let orb_task_id = optional_identity(input.orb_task_id, "Computer task id")?;
    let launch_receipt_id =
        optional_identity(input.launch_receipt_id, "Computer launch receipt id")?;
    let operation_id = required_identity(input.operation_id, "Computer operation id")?;
    let idempotency_key = required_identity(input.idempotency_key, "Computer idempotency key")?;
    let request_digest = required_identity(input.request_digest, "Computer request digest")?;
    let now = required_identity(now, "Computer ledger timestamp")?;

    let by_delegation_id = ledger
        .orb_delegations
        .iter()
        .position(|entry| entry.maestro_delegation_id == maestro_delegation_id);
    let by_idempotency = ledger
        .orb_delegations
        .iter()
        .position(|entry| entry.tenant_id == tenant_id && entry.idempotency_key == idempotency_key);
    if let (Some(left), Some(right)) = (by_delegation_id, by_idempotency) {
        if left != right {
            bail!("Computer delegation identity maps to multiple persisted rows; refusing replay");
        }
    }

    let Some(index) = by_delegation_id.or(by_idempotency) else {
        ledger.orb_delegations.push(OrbDelegationEntry {
            maestro_delegation_id,
            maestro_session_id,
            tenant_id,
            connection_ref,
            orb_thread_id,
            orb_task_id,
            launch_receipt_id,
            operation_id,
            idempotency_key,
            request_digest,
            state: OrbDelegationState::Starting,
            observed_revision: 0,
            created_at: now.clone(),
            updated_at: now,
            terminal_at: None,
            extensions: BTreeMap::new(),
        });
        return Ok(OrbDelegationStartOutcome::Created);
    };

    let entry = &mut ledger.orb_delegations[index];
    ensure_same_identity(
        &entry.maestro_delegation_id,
        &maestro_delegation_id,
        "Maestro delegation id",
    )?;
    ensure_same_identity(
        &entry.maestro_session_id,
        &maestro_session_id,
        "Maestro session id",
    )?;
    ensure_same_identity(&entry.tenant_id, &tenant_id, "Computer tenant id")?;
    ensure_same_identity(&entry.operation_id, &operation_id, "Computer operation id")?;
    ensure_same_identity(
        &entry.idempotency_key,
        &idempotency_key,
        "Computer idempotency key",
    )?;
    ensure_same_identity(
        &entry.request_digest,
        &request_digest,
        "Computer request digest",
    )?;

    let mut changed = false;
    changed |= merge_remote_identity(
        &mut entry.connection_ref,
        connection_ref,
        "Computer connection reference",
    )?;
    changed |= merge_remote_identity(
        &mut entry.orb_thread_id,
        orb_thread_id,
        "Computer thread id",
    )?;
    changed |= merge_remote_identity(&mut entry.orb_task_id, orb_task_id, "Computer task id")?;
    changed |= merge_remote_identity(
        &mut entry.launch_receipt_id,
        launch_receipt_id,
        "Computer launch receipt id",
    )?;
    if changed {
        entry.updated_at = now;
    }
    Ok(OrbDelegationStartOutcome::Replayed)
}

/// Apply a hosted Orb/Platform observation to one persisted delegation. The
/// mapping is monotonic by provider revision, and a terminal record dominates
/// delayed nonterminal observations.
pub fn reconcile_orb_delegation(
    ledger: &mut TaskLedgerFile,
    maestro_delegation_id: &str,
    observation: OrbDelegationObservation,
    now: &str,
) -> Result<bool> {
    let maestro_delegation_id = required_identity(maestro_delegation_id, "Maestro delegation id")?;
    let now = required_identity(now, "Computer ledger timestamp")?;
    let tenant_id = required_identity(&observation.tenant_id, "Observed Computer tenant id")?;
    let connection_ref = optional_identity(
        observation.connection_ref.as_deref(),
        "Observed Computer connection reference",
    )?;
    let orb_thread_id = optional_identity(
        observation.orb_thread_id.as_deref(),
        "Observed Computer thread id",
    )?;
    let orb_task_id = optional_identity(
        observation.orb_task_id.as_deref(),
        "Observed Computer task id",
    )?;
    let launch_receipt_id = optional_identity(
        observation.launch_receipt_id.as_deref(),
        "Observed Computer launch receipt id",
    )?;
    let Some(index) = ledger
        .orb_delegations
        .iter()
        .position(|entry| entry.maestro_delegation_id == maestro_delegation_id)
    else {
        bail!("Computer delegation binding is not persisted");
    };

    let entry = &mut ledger.orb_delegations[index];
    ensure_same_identity(&entry.tenant_id, &tenant_id, "Computer tenant id")?;
    let mut changed = false;
    changed |= merge_remote_identity(
        &mut entry.connection_ref,
        connection_ref,
        "Computer connection reference",
    )?;
    changed |= merge_remote_identity(
        &mut entry.orb_thread_id,
        orb_thread_id,
        "Computer thread id",
    )?;
    changed |= merge_remote_identity(&mut entry.orb_task_id, orb_task_id, "Computer task id")?;
    changed |= merge_remote_identity(
        &mut entry.launch_receipt_id,
        launch_receipt_id,
        "Computer launch receipt id",
    )?;

    if entry.state.is_terminal() || observation.revision < entry.observed_revision {
        if changed {
            entry.updated_at = now;
        }
        return Ok(changed);
    }

    let next_state = observation.state.delegation_state();
    if entry.state != next_state {
        entry.state = next_state;
        changed = true;
    }
    if entry.observed_revision != observation.revision {
        entry.observed_revision = observation.revision;
        changed = true;
    }
    if next_state.is_terminal() && entry.terminal_at.is_none() {
        entry.terminal_at = Some(now.clone());
        changed = true;
    }
    if changed {
        entry.updated_at = now;
    }
    Ok(changed)
}

/// Observer-only recovery of every nonterminal Orb binding in a loaded ledger.
/// A missing observation is recorded as `unavailable`; it is never treated as
/// permission to issue a fresh launch.
pub fn recover_orb_delegations<O: OrbDelegationObserver>(
    ledger: &mut TaskLedgerFile,
    observer: &mut O,
    now: &str,
) -> Result<OrbRecoveryReport> {
    let delegation_ids: Vec<String> = ledger
        .orb_delegations
        .iter()
        .filter(|entry| entry.state.is_nonterminal())
        .map(|entry| entry.maestro_delegation_id.clone())
        .collect();
    let mut report = OrbRecoveryReport {
        inspected: delegation_ids.len(),
        ..OrbRecoveryReport::default()
    };

    for delegation_id in delegation_ids {
        let observation = {
            let entry = ledger
                .orb_delegations
                .iter()
                .find(|entry| entry.maestro_delegation_id == delegation_id)
                .with_context(|| {
                    format!("Computer delegation {delegation_id} disappeared during recovery")
                })?;
            observer.observe(entry)?
        };
        let observation = observation.unwrap_or_else(|| {
            let entry = ledger
                .orb_delegations
                .iter()
                .find(|entry| entry.maestro_delegation_id == delegation_id)
                .expect("delegation id was collected from the same ledger");
            OrbDelegationObservation {
                tenant_id: entry.tenant_id.clone(),
                connection_ref: entry.connection_ref.clone(),
                orb_thread_id: entry.orb_thread_id.clone(),
                orb_task_id: entry.orb_task_id.clone(),
                launch_receipt_id: entry.launch_receipt_id.clone(),
                state: OrbObservedState::Unavailable,
                revision: entry.observed_revision.saturating_add(1),
            }
        });
        if reconcile_orb_delegation(ledger, &delegation_id, observation, now)? {
            report.updated += 1;
        }
        if let Some(entry) = ledger
            .orb_delegations
            .iter()
            .find(|entry| entry.maestro_delegation_id == delegation_id)
        {
            match entry.state {
                OrbDelegationState::Terminal => report.terminal += 1,
                OrbDelegationState::Unavailable => report.unavailable += 1,
                OrbDelegationState::Starting
                | OrbDelegationState::Running
                | OrbDelegationState::Waiting => {}
            }
        }
    }
    Ok(report)
}

/// Load, recover, and atomically persist the local ledger for a process
/// restart. This wrapper is the production entry point; the in-memory variant
/// keeps deterministic tests independent of wall-clock time.
pub fn recover_orb_delegations_from_path<O: OrbDelegationObserver>(
    path: Option<&str>,
    observer: &mut O,
) -> Result<OrbRecoveryReport> {
    let location = resolve_task_ledger_location(path)?;
    ensure_ledger_directory(&location.path, location.secure_new_default_parent)?;
    let path = location.path;
    // Snapshot under the shared lock, but never invoke arbitrary observer
    // code while holding it. An observer may legitimately inspect or update
    // the same ledger, and holding the lock across that callback would make a
    // reentrant recovery deadlock until the stale-lock timeout.
    let delegations = with_task_ledger_lock_at(&path, |ledger_path, _lock| {
        let ledger = load_task_ledger_at(ledger_path)?;
        Ok(ledger
            .orb_delegations
            .into_iter()
            .filter(|entry| entry.state.is_nonterminal())
            .collect::<Vec<_>>())
    })?;
    let mut report = OrbRecoveryReport {
        inspected: delegations.len(),
        ..OrbRecoveryReport::default()
    };
    for delegation in delegations {
        let delegation_id = delegation.maestro_delegation_id.clone();
        let observation =
            observer
                .observe(&delegation)?
                .unwrap_or_else(|| OrbDelegationObservation {
                    tenant_id: delegation.tenant_id.clone(),
                    connection_ref: delegation.connection_ref.clone(),
                    orb_thread_id: delegation.orb_thread_id.clone(),
                    orb_task_id: delegation.orb_task_id.clone(),
                    launch_receipt_id: delegation.launch_receipt_id.clone(),
                    state: OrbObservedState::Unavailable,
                    revision: delegation.observed_revision.saturating_add(1),
                });
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let (changed, state) = with_locked_task_ledger_at(&path, |ledger| {
            let changed = reconcile_orb_delegation(ledger, &delegation_id, observation, &now)?;
            let state = ledger
                .orb_delegations
                .iter()
                .find(|entry| entry.maestro_delegation_id == delegation_id)
                .map(|entry| entry.state)
                .with_context(|| {
                    format!("Computer delegation {delegation_id} disappeared during recovery")
                })?;
            Ok((changed, state))
        })?;
        if changed {
            report.updated += 1;
        }
        match state {
            OrbDelegationState::Terminal => report.terminal += 1,
            OrbDelegationState::Unavailable => report.unavailable += 1,
            OrbDelegationState::Starting
            | OrbDelegationState::Running
            | OrbDelegationState::Waiting => {}
        }
    }
    Ok(report)
}

fn required_identity(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} is required");
    }
    Ok(value.to_string())
}

fn optional_identity(value: Option<&str>, label: &str) -> Result<Option<String>> {
    value
        .map(|value| required_identity(value, label))
        .transpose()
}

fn ensure_same_identity<T: PartialEq>(existing: &T, incoming: &T, label: &str) -> Result<()> {
    if existing != incoming {
        bail!("{label} changed for an existing Computer replay identity");
    }
    Ok(())
}

fn merge_remote_identity(
    existing: &mut Option<String>,
    incoming: Option<String>,
    label: &str,
) -> Result<bool> {
    let Some(incoming) = incoming else {
        return Ok(false);
    };
    if let Some(current) = existing.as_ref() {
        ensure_same_identity(current, &incoming, label)?;
        return Ok(false);
    }
    *existing = Some(incoming);
    Ok(true)
}

pub struct RecordTaskStartInput<'a> {
    pub path: Option<&'a str>,
    pub peer: &'a str,
    pub peer_display_name: Option<&'a str>,
    pub task: &'a A2ATask,
    pub text: &'a str,
    pub message_id: Option<&'a str>,
    pub context_id: Option<&'a str>,
    pub kind: &'a str,
    pub metadata: Option<Value>,
}

const PEER_MESSAGE_DISPATCH_INTENT_STATE: &str = "LOCAL_DISPATCH_INTENT";

pub struct PeerMessageIntentInput<'a> {
    pub path: Option<&'a str>,
    pub peer: &'a str,
    pub peer_display_name: Option<&'a str>,
    pub text: &'a str,
    pub message_id: &'a str,
    pub context_id: &'a str,
    pub kind: &'a str,
    pub metadata: Value,
}

pub struct PeerMessageIntent {
    pub message_id: String,
    pub context_id: String,
}

pub fn record_peer_message_intent(input: PeerMessageIntentInput<'_>) -> Result<PeerMessageIntent> {
    with_locked_task_ledger(input.path, |ledger| {
        let task = A2ATask {
            id: input.message_id.to_string(),
            context_id: Some(input.context_id.to_string()),
            status: A2ATaskStatus {
                state: PEER_MESSAGE_DISPATCH_INTENT_STATE.to_string(),
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        record_task_start_in_ledger(
            ledger,
            RecordTaskStartInput {
                path: None,
                peer: input.peer,
                peer_display_name: input.peer_display_name,
                task: &task,
                text: input.text,
                message_id: Some(input.message_id),
                context_id: Some(input.context_id),
                kind: input.kind,
                metadata: Some(input.metadata),
            },
        )?;
        Ok(PeerMessageIntent {
            message_id: input.message_id.to_string(),
            context_id: input.context_id.to_string(),
        })
    })
}

pub fn record_task_start(input: RecordTaskStartInput<'_>) -> Result<()> {
    with_locked_task_ledger(input.path, |ledger| {
        record_task_start_in_ledger(ledger, input)
    })
}

fn record_task_start_in_ledger(
    ledger: &mut TaskLedgerFile,
    input: RecordTaskStartInput<'_>,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let task_id = input.task.id.trim();
    if task_id.is_empty() {
        bail!("A2A task id is required");
    }
    let response_text = extract_task_text(input.task);
    let entry = TaskLedgerEntry {
        id: format!("maestro-a2a-task-{}", uuid::Uuid::new_v4()),
        kind: input.kind.to_string(),
        peer: input.peer.to_string(),
        peer_display_name: input.peer_display_name.map(str::to_string),
        task_id: task_id.to_string(),
        context_id: input
            .context_id
            .map(str::to_string)
            .or_else(|| input.task.context_id.clone()),
        message_id: input.message_id.map(str::to_string),
        text: input.text.to_string(),
        role: None,
        cwd: None,
        state: input.task.status.state.clone(),
        response_text: response_text.clone(),
        metadata: input.metadata,
        work_graph: extract_work_graph(input.task),
        transcript: {
            let mut transcript = vec![TranscriptEntry {
                at: now.clone(),
                role: "user".into(),
                text: input.text.to_string(),
                state: None,
                message_id: input.message_id.map(str::to_string),
                extensions: BTreeMap::new(),
            }];
            if let Some(response) = response_text {
                transcript.push(TranscriptEntry {
                    at: now.clone(),
                    role: "agent".into(),
                    text: response,
                    state: Some(input.task.status.state.clone()),
                    message_id: None,
                    extensions: BTreeMap::new(),
                });
            }
            transcript
        },
        created_at: now.clone(),
        updated_at: now.clone(),
        completed_at: if is_final_state(&input.task.status.state) {
            Some(now)
        } else {
            None
        },
        extensions: BTreeMap::new(),
    };
    if let Some(index) = ledger.tasks.iter().position(|e| {
        e.peer == input.peer
            && (e.task_id == task_id
                || (input.message_id.is_some() && e.message_id.as_deref() == input.message_id))
    }) {
        let previous = &ledger.tasks[index];
        let mut merged = entry;
        merged.id = previous.id.clone();
        merged.created_at = previous.created_at.clone();
        merged.extensions = previous.extensions.clone();
        for (new_item, previous_item) in merged.transcript.iter_mut().zip(&previous.transcript) {
            if new_item.role == previous_item.role
                && new_item.text == previous_item.text
                && new_item.message_id == previous_item.message_id
            {
                new_item.extensions = previous_item.extensions.clone();
            }
        }
        ledger.tasks[index] = merged;
    } else {
        ledger.tasks.push(entry);
    }
    Ok(())
}

pub fn update_task_in_ledger(path: Option<&str>, peer: &str, task: &A2ATask) -> Result<()> {
    with_locked_task_ledger(path, |ledger| {
        update_task_in_ledger_locked(ledger, peer, task)
    })
}

fn update_task_in_ledger_locked(
    ledger: &mut TaskLedgerFile,
    peer: &str,
    task: &A2ATask,
) -> Result<()> {
    let task_id = task.id.trim();
    if task_id.is_empty() {
        bail!("A2A task id is required");
    }
    let Some(index) = ledger
        .tasks
        .iter()
        .position(|e| e.peer == peer && e.task_id == task_id)
    else {
        return record_task_start_in_ledger(
            ledger,
            RecordTaskStartInput {
                path: None,
                peer,
                peer_display_name: None,
                task,
                text: extract_task_text(task).as_deref().unwrap_or(""),
                message_id: None,
                context_id: task.context_id.as_deref(),
                kind: "message",
                metadata: None,
            },
        );
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let previous = &ledger.tasks[index];
    let response_text = extract_task_text(task).or_else(|| previous.response_text.clone());
    let mut entry = previous.clone();
    entry.state = task.status.state.clone();
    if let Some(context_id) = task
        .context_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        entry.context_id = Some(context_id.to_string());
    }
    entry.response_text = response_text.clone();
    if let Some(graph) = extract_work_graph(task) {
        entry.work_graph = Some(graph);
    }
    entry.updated_at = now.clone();
    if is_final_state(&task.status.state) {
        entry.completed_at = entry.completed_at.clone().or(Some(now.clone()));
    } else {
        entry.completed_at = None;
    }
    if let Some(response) = response_text {
        let should_append = !entry
            .transcript
            .iter()
            .rev()
            .any(|item| item.role == "agent" && item.text == response);
        if should_append {
            entry.transcript.push(TranscriptEntry {
                at: now,
                role: "agent".into(),
                text: response,
                state: Some(task.status.state.clone()),
                message_id: None,
                extensions: BTreeMap::new(),
            });
        }
    }
    ledger.tasks[index] = entry;
    Ok(())
}

fn extract_work_graph(task: &A2ATask) -> Option<Value> {
    task.metadata
        .as_ref()
        .and_then(|meta| meta.get("workGraph").cloned())
        .or_else(|| {
            task.metadata
                .as_ref()
                .and_then(|meta| meta.get("evalops").cloned())
                .and_then(|evalops| evalops.get("workGraph").cloned())
        })
}

fn normalize_ledger_entry(input: &Value, label: &str) -> Result<TaskLedgerEntry> {
    let obj = input
        .as_object()
        .with_context(|| format!("{label} must be an object"))?;
    let task_id = obj
        .get("taskId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{label}.taskId is required"))?
        .to_string();
    let peer = obj
        .get("peer")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{label}.peer is required"))?
        .to_string();
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("maestro-a2a-task-{task_id}"));
    let state = obj
        .get("state")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let text = obj
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let created_at = obj
        .get("createdAt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let updated_at = obj
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .unwrap_or(&created_at)
        .to_string();
    let transcript = obj
        .get("transcript")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let entry = item.as_object()?;
                    Some(TranscriptEntry {
                        at: entry
                            .get("at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        role: entry
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("user")
                            .to_string(),
                        text: entry
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        state: entry
                            .get("state")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        message_id: entry
                            .get("messageId")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        extensions: unknown_ledger_fields(entry, TRANSCRIPT_FIELDS),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(TaskLedgerEntry {
        id,
        kind: obj
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("message")
            .to_string(),
        peer,
        peer_display_name: string_field(obj, "peerDisplayName"),
        task_id,
        context_id: string_field(obj, "contextId"),
        message_id: string_field(obj, "messageId"),
        text,
        role: string_field(obj, "role"),
        cwd: string_field(obj, "cwd"),
        state,
        response_text: string_field(obj, "responseText"),
        metadata: obj.get("metadata").cloned(),
        work_graph: obj.get("workGraph").cloned(),
        transcript,
        created_at,
        updated_at,
        completed_at: string_field(obj, "completedAt"),
        extensions: unknown_ledger_fields(obj, TASK_LEDGER_ENTRY_FIELDS),
    })
}

fn string_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Barrier};

    use super::super::client::A2ATaskStatus;
    use super::*;

    fn start_input<'a>(
        path: Option<&'a str>,
        request_digest: &'a str,
    ) -> OrbDelegationStartInput<'a> {
        OrbDelegationStartInput {
            path,
            maestro_delegation_id: "maestro-delegation-1",
            maestro_session_id: "maestro-session-1",
            tenant_id: "tenant-1",
            connection_ref: Some("orb-connection-ref-1"),
            orb_thread_id: Some("orb-thread-1"),
            orb_task_id: None,
            launch_receipt_id: Some("orb-launch-receipt-1"),
            operation_id: "orb-operation-1",
            idempotency_key: "fixture-replay-1",
            request_digest,
        }
    }

    struct FixtureObserver {
        observations: VecDeque<Option<OrbDelegationObservation>>,
        observed_ids: Vec<String>,
    }

    impl OrbDelegationObserver for FixtureObserver {
        fn observe(
            &mut self,
            delegation: &OrbDelegationEntry,
        ) -> anyhow::Result<Option<OrbDelegationObservation>> {
            self.observed_ids
                .push(delegation.maestro_delegation_id.clone());
            Ok(self.observations.pop_front().unwrap_or(None))
        }
    }

    fn waiting_observation(revision: u64) -> OrbDelegationObservation {
        OrbDelegationObservation {
            tenant_id: "tenant-1".into(),
            connection_ref: Some("orb-connection-ref-1".into()),
            orb_thread_id: Some("orb-thread-1".into()),
            orb_task_id: Some("orb-task-1".into()),
            launch_receipt_id: Some("orb-launch-receipt-1".into()),
            state: OrbObservedState::TaskWaiting,
            revision,
        }
    }

    fn fixture_task(task_id: &str) -> A2ATask {
        A2ATask {
            id: task_id.to_string(),
            context_id: Some(format!("context-{task_id}")),
            status: A2ATaskStatus {
                state: "TASK_STATE_WORKING".to_string(),
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    #[test]
    fn orb_start_replay_is_idempotent_and_digest_bound() {
        let mut ledger = TaskLedgerFile::default();
        assert_eq!(
            upsert_orb_delegation(
                &mut ledger,
                start_input(None, "request-digest-a"),
                "2026-08-20T00:00:00.000Z",
            )
            .unwrap(),
            OrbDelegationStartOutcome::Created
        );
        assert_eq!(
            upsert_orb_delegation(
                &mut ledger,
                start_input(None, "request-digest-a"),
                "2026-08-20T00:00:01.000Z",
            )
            .unwrap(),
            OrbDelegationStartOutcome::Replayed
        );
        assert_eq!(ledger.orb_delegations.len(), 1);
        assert_eq!(ledger.orb_delegations[0].request_digest, "request-digest-a");

        let changed_payload = upsert_orb_delegation(
            &mut ledger,
            start_input(None, "request-digest-b"),
            "2026-08-20T00:00:02.000Z",
        );
        assert!(changed_payload.is_err());
        assert_eq!(ledger.orb_delegations.len(), 1);

        let mut other_tenant = start_input(None, "request-digest-a");
        other_tenant.maestro_delegation_id = "maestro-delegation-2";
        other_tenant.maestro_session_id = "maestro-session-2";
        other_tenant.tenant_id = "tenant-2";
        assert_eq!(
            upsert_orb_delegation(&mut ledger, other_tenant, "2026-08-20T00:00:03.000Z",).unwrap(),
            OrbDelegationStartOutcome::Created
        );
        assert_eq!(ledger.orb_delegations.len(), 2);

        let encoded = serde_json::to_string(&ledger).unwrap();
        assert!(encoded.contains("orbDelegations"));
        assert!(encoded.contains("orbThreadId"));
        assert!(!encoded.contains("accessToken"));
        assert!(!encoded.contains("clientSecret"));
        assert!(!encoded.contains("prompt"));
    }

    #[test]
    fn orb_start_path_replay_and_conflict_are_serialized() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap();

        assert_eq!(
            record_orb_delegation_start(start_input(Some(path), "request-digest-a")).unwrap(),
            OrbDelegationStartOutcome::Created
        );
        assert_eq!(
            record_orb_delegation_start(start_input(Some(path), "request-digest-a")).unwrap(),
            OrbDelegationStartOutcome::Replayed
        );

        let conflict = record_orb_delegation_start(start_input(Some(path), "request-digest-b"));
        assert!(conflict.is_err());
        let ledger = load_task_ledger(Some(path)).unwrap();
        assert_eq!(ledger.orb_delegations.len(), 1);
        assert_eq!(ledger.orb_delegations[0].request_digest, "request-digest-a");
    }

    #[test]
    fn orb_start_path_releases_lock_after_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap();

        let mut invalid = start_input(Some(path), "request-digest-a");
        invalid.maestro_delegation_id = " ";
        assert!(record_orb_delegation_start(invalid).is_err());

        assert_eq!(
            record_orb_delegation_start(start_input(Some(path), "request-digest-a")).unwrap(),
            OrbDelegationStartOutcome::Created
        );
        assert_eq!(
            load_task_ledger(Some(path)).unwrap().orb_delegations.len(),
            1
        );
    }

    #[test]
    fn legacy_task_ledger_remains_readable_and_is_preserved_by_orb_start() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap();
        std::fs::write(
            path,
            r#"{
                "tasks": [{
                    "taskId": "legacy-task-1",
                    "peer": "legacy-peer",
                    "state": "working",
                    "text": "legacy text",
                    "createdAt": "2026-08-20T00:00:00.000Z"
                }]
            }"#,
        )
        .unwrap();

        let legacy = load_task_ledger(Some(path)).unwrap();
        assert_eq!(legacy.tasks.len(), 1);
        assert!(legacy.orb_delegations.is_empty());

        record_orb_delegation_start(start_input(Some(path), "request-digest-a")).unwrap();
        let persisted = load_task_ledger(Some(path)).unwrap();
        assert_eq!(persisted.tasks.len(), 1);
        assert_eq!(persisted.tasks[0].task_id, "legacy-task-1");
        assert_eq!(persisted.orb_delegations.len(), 1);
    }

    #[test]
    fn tui_mutations_preserve_gateway_projection_extensions_losslessly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path_string = path.to_str().unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "futureProjection": {
                    "schema": "gateway-future-v1",
                    "owner": "runtime-gateway"
                },
                "tasks": [{
                    "id": "gateway-task-lossless",
                    "kind": "message",
                    "peer": "peer-lossless",
                    "taskId": "task-lossless",
                    "contextId": "context-task-lossless",
                    "text": "lossless task",
                    "state": "TASK_STATE_QUEUED",
                    "createdAt": "2026-08-20T00:00:00.000Z",
                    "updatedAt": "2026-08-20T00:00:00.000Z",
                    "transcript": [{
                        "at": "2026-08-20T00:00:00.000Z",
                        "role": "user",
                        "text": "lossless task",
                        "messageId": "lossless-message",
                        "futureTranscriptField": {"source": "gateway"}
                    }],
                    "futureTaskField": {"revision": 7},
                    "a2aTask": {
                        "id": "task-lossless",
                        "futureStatusProjection": "preserve-me"
                    }
                }],
                "orbDelegations": [{
                    "maestroDelegationId": "maestro-delegation-1",
                    "maestroSessionId": "maestro-session-1",
                    "tenantId": "tenant-1",
                    "connectionRef": "orb-connection-ref-1",
                    "orbThreadId": "orb-thread-1",
                    "operationId": "orb-operation-1",
                    "idempotencyKey": "fixture-replay-1",
                    "requestDigest": "request-digest-a",
                    "state": "starting",
                    "observedRevision": 0,
                    "createdAt": "2026-08-20T00:00:00.000Z",
                    "updatedAt": "2026-08-20T00:00:00.000Z",
                    "futureOrbField": {"revision": 11}
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let task = fixture_task("task-lossless");
        record_task_start(RecordTaskStartInput {
            path: Some(path_string),
            peer: "peer-lossless",
            peer_display_name: None,
            task: &task,
            text: "lossless task",
            message_id: Some("lossless-message"),
            context_id: task.context_id.as_deref(),
            kind: "message",
            metadata: None,
        })
        .unwrap();
        assert_eq!(
            record_orb_delegation_start(start_input(Some(path_string), "request-digest-a"))
                .unwrap(),
            OrbDelegationStartOutcome::Replayed
        );

        let persisted = maestro_a2a_ledger::load(&path).unwrap();
        assert_eq!(
            persisted["futureProjection"],
            serde_json::json!({"schema": "gateway-future-v1", "owner": "runtime-gateway"})
        );
        assert_eq!(
            persisted["tasks"][0]["futureTaskField"],
            serde_json::json!({"revision": 7})
        );
        assert_eq!(
            persisted["tasks"][0]["a2aTask"],
            serde_json::json!({
                "id": "task-lossless",
                "futureStatusProjection": "preserve-me"
            })
        );
        assert_eq!(persisted["tasks"][0]["state"], "TASK_STATE_WORKING");
        assert_eq!(
            persisted["tasks"][0]["transcript"][0]["futureTranscriptField"],
            serde_json::json!({"source": "gateway"})
        );
        assert_eq!(
            persisted["orbDelegations"][0]["futureOrbField"],
            serde_json::json!({"revision": 11})
        );
        assert_eq!(persisted["orbDelegations"][0]["state"], "starting");
    }

    #[test]
    fn concurrent_orb_starts_preserve_distinct_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap().to_owned();
        let barrier = Arc::new(Barrier::new(3));

        std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first_path = &path;
            let first = scope.spawn(move || {
                first_barrier.wait();
                record_orb_delegation_start(OrbDelegationStartInput {
                    path: Some(first_path),
                    maestro_delegation_id: "maestro-delegation-1",
                    maestro_session_id: "maestro-session-1",
                    tenant_id: "tenant-1",
                    connection_ref: Some("orb-connection-ref-1"),
                    orb_thread_id: Some("orb-thread-1"),
                    orb_task_id: None,
                    launch_receipt_id: Some("orb-launch-receipt-1"),
                    operation_id: "orb-operation-1",
                    idempotency_key: "fixture-replay-1",
                    request_digest: "request-digest-a",
                })
            });

            let second_barrier = Arc::clone(&barrier);
            let second_path = &path;
            let second = scope.spawn(move || {
                second_barrier.wait();
                record_orb_delegation_start(OrbDelegationStartInput {
                    path: Some(second_path),
                    maestro_delegation_id: "maestro-delegation-2",
                    maestro_session_id: "maestro-session-2",
                    tenant_id: "tenant-2",
                    connection_ref: Some("orb-connection-ref-2"),
                    orb_thread_id: Some("orb-thread-2"),
                    orb_task_id: None,
                    launch_receipt_id: Some("orb-launch-receipt-2"),
                    operation_id: "orb-operation-2",
                    idempotency_key: "fixture-replay-2",
                    request_digest: "request-digest-b",
                })
            });

            barrier.wait();
            assert_eq!(
                first.join().unwrap().unwrap(),
                OrbDelegationStartOutcome::Created
            );
            assert_eq!(
                second.join().unwrap().unwrap(),
                OrbDelegationStartOutcome::Created
            );
        });

        let ledger = load_task_ledger(Some(&path)).unwrap();
        assert_eq!(ledger.orb_delegations.len(), 2);
        assert!(
            ledger
                .orb_delegations
                .iter()
                .any(|entry| entry.maestro_delegation_id == "maestro-delegation-1")
        );
        assert!(
            ledger
                .orb_delegations
                .iter()
                .any(|entry| entry.maestro_delegation_id == "maestro-delegation-2")
        );
    }

    #[test]
    fn concurrent_task_and_orb_writes_preserve_both_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap().to_owned();
        let barrier = Arc::new(Barrier::new(3));
        let task = fixture_task("task-concurrent");

        std::thread::scope(|scope| {
            let task_barrier = Arc::clone(&barrier);
            let task_path = path.clone();
            let task_ref = &task;
            let task_writer = scope.spawn(move || {
                task_barrier.wait();
                record_task_start(RecordTaskStartInput {
                    path: Some(&task_path),
                    peer: "peer-task",
                    peer_display_name: None,
                    task: task_ref,
                    text: "task text",
                    message_id: Some("message-task"),
                    context_id: task_ref.context_id.as_deref(),
                    kind: "message",
                    metadata: None,
                })
            });

            let orb_barrier = Arc::clone(&barrier);
            let orb_path = path.clone();
            let orb_writer = scope.spawn(move || {
                orb_barrier.wait();
                record_orb_delegation_start(OrbDelegationStartInput {
                    path: Some(&orb_path),
                    maestro_delegation_id: "maestro-delegation-concurrent",
                    maestro_session_id: "maestro-session-concurrent",
                    tenant_id: "tenant-concurrent",
                    connection_ref: Some("orb-connection-concurrent"),
                    orb_thread_id: Some("orb-thread-concurrent"),
                    orb_task_id: None,
                    launch_receipt_id: Some("orb-launch-concurrent"),
                    operation_id: "orb-operation-concurrent",
                    idempotency_key: "fixture-replay-concurrent",
                    request_digest: "request-digest-concurrent",
                })
            });

            barrier.wait();
            task_writer.join().unwrap().unwrap();
            orb_writer.join().unwrap().unwrap();
        });

        let ledger = load_task_ledger(Some(&path)).unwrap();
        assert_eq!(ledger.tasks.len(), 1);
        assert_eq!(ledger.tasks[0].task_id, "task-concurrent");
        assert_eq!(ledger.orb_delegations.len(), 1);
        assert_eq!(
            ledger.orb_delegations[0].maestro_delegation_id,
            "maestro-delegation-concurrent"
        );
    }

    #[test]
    fn gateway_directory_lock_protocol_blocks_tui_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path_string = path.to_str().unwrap().to_owned();
        let canonical_path = get_task_ledger_path(Some(&path_string)).unwrap();
        let lock_path = ledger_lock_path(&canonical_path);
        fs::create_dir(&lock_path).unwrap();
        fs::write(
            lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE),
            "gateway-owner\n",
        )
        .unwrap();
        fs::write(
            lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE),
            format!("{}\n", unix_millis_now()),
        )
        .unwrap();

        let writer_path = path_string.clone();
        let writer = std::thread::spawn(move || {
            record_orb_delegation_start(OrbDelegationStartInput {
                path: Some(&writer_path),
                maestro_delegation_id: "gateway-compat-delegation",
                maestro_session_id: "gateway-compat-session",
                tenant_id: "gateway-compat-tenant",
                connection_ref: Some("gateway-compat-connection"),
                orb_thread_id: Some("gateway-compat-thread"),
                orb_task_id: None,
                launch_receipt_id: Some("gateway-compat-launch"),
                operation_id: "gateway-compat-operation",
                idempotency_key: "gateway-compat-idempotency",
                request_digest: "gateway-compat-digest",
            })
        });
        std::thread::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS * 3));
        assert!(!canonical_path.exists());
        fs::remove_dir_all(&lock_path).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(
            load_task_ledger(Some(&path_string))
                .unwrap()
                .orb_delegations
                .len(),
            1
        );
        assert!(!lock_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn shared_lock_and_database_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let canonical_path = get_task_ledger_path(path.to_str()).unwrap();
        with_task_ledger_lock_at(&canonical_path, |_, _lock| {
            let lock_path = ledger_lock_path(&canonical_path);
            assert_eq!(
                fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            Ok(())
        })
        .unwrap();
        save_task_ledger(&TaskLedgerFile::default(), path.to_str()).unwrap();
        assert_eq!(
            fs::metadata(maestro_a2a_ledger::database_path(&canonical_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_custom_ledger_parent_permissions_are_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let shared_parent = directory.path().join("shared-project-state");
        fs::create_dir(&shared_parent).unwrap();
        fs::set_permissions(&shared_parent, fs::Permissions::from_mode(0o775)).unwrap();
        let path = shared_parent.join("tasks.json");

        save_task_ledger(&TaskLedgerFile::default(), path.to_str()).unwrap();

        assert_eq!(
            fs::metadata(&shared_parent).unwrap().permissions().mode() & 0o777,
            0o775
        );
        assert_eq!(
            fs::metadata(maestro_a2a_ledger::database_path(&path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_default_ledger_parent_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".maestro/a2a/tasks.json");

        ensure_ledger_directory(&path, true).unwrap();

        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn ledger_write_aborts_after_lock_ownership_loss() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let canonical_path = get_task_ledger_path(path.to_str()).unwrap();
        let lock = acquire_task_ledger_lock(&canonical_path).unwrap();
        write_private_lock_file(
            &lock.path.join(A2A_LEDGER_LOCK_OWNER_FILE),
            "reclaimed-by-another-process\n",
        )
        .unwrap();

        let error = save_task_ledger_at(&TaskLedgerFile::default(), &canonical_path, &lock)
            .expect_err("a lock owner that was reclaimed must not replace the ledger");
        assert!(
            error
                .to_string()
                .contains("lost A2A task ledger lock ownership")
        );
        assert!(!canonical_path.exists());
        fs::remove_dir_all(&lock.path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn live_owner_fence_blocks_stale_reclaimer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let canonical_path = get_task_ledger_path(path.to_str()).unwrap();
        let lock = acquire_task_ledger_lock(&canonical_path).unwrap();
        let lock_path = lock.path.clone();
        let contender = std::thread::spawn(move || {
            try_acquire_task_ledger_owner_fence(&lock_path)
                .unwrap()
                .is_none()
        });
        assert!(contender.join().unwrap());
        drop(lock);
    }

    #[cfg(unix)]
    #[test]
    fn release_fence_blocks_aba_reclaim_before_new_owner() {
        let directory = tempfile::tempdir().unwrap();
        let ledger_path =
            get_task_ledger_path(Some(directory.path().join("tasks.json").to_str().unwrap()))
                .unwrap();
        let mut lock = acquire_task_ledger_lock(&ledger_path).unwrap();
        let lock_path = lock.path.clone();
        let (checked_tx, checked_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        lock.release_probe = Some(TaskLedgerReleaseProbe {
            checked: checked_tx,
            resume: resume_rx,
        });
        let release = std::thread::spawn(move || drop(lock));
        checked_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("release should check ownership before removing the lock");

        let (ready_tx, ready_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let contender_lock_path = lock_path.clone();
        let contender_ledger_path = ledger_path.clone();
        let contender = std::thread::spawn(move || {
            let candidate = try_acquire_task_ledger_owner_fence(&contender_lock_path).unwrap();
            if let Some(owner_file) = candidate {
                fs::remove_dir_all(&contender_lock_path).unwrap();
                drop(owner_file);
                let recreated = acquire_task_ledger_lock(&contender_ledger_path).unwrap();
                ready_tx.send(true).unwrap();
                continue_rx.recv().unwrap();
                let survived = contender_lock_path.exists();
                drop(recreated);
                survived
            } else {
                ready_tx.send(false).unwrap();
                continue_rx.recv().unwrap();
                let recreated = acquire_task_ledger_lock(&contender_ledger_path).unwrap();
                let survived = contender_lock_path.exists();
                drop(recreated);
                survived
            }
        });

        let reclaimed_old_inode = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("contender should probe the old owner inode");
        resume_tx.send(()).unwrap();
        release.join().unwrap();
        continue_tx.send(()).unwrap();
        let recreated_lock_survived = contender.join().unwrap();
        assert!(
            !reclaimed_old_inode,
            "a release must retain its owner fence until removing the old lock"
        );
        assert!(
            recreated_lock_survived,
            "a new owner must not be removed by the prior release"
        );
    }

    #[test]
    fn recovery_observer_can_reenter_same_ledger_without_deadlock() {
        struct ReentrantObserver {
            path: String,
            inserted: bool,
        }

        impl OrbDelegationObserver for ReentrantObserver {
            fn observe(
                &mut self,
                _delegation: &OrbDelegationEntry,
            ) -> anyhow::Result<Option<OrbDelegationObservation>> {
                if !self.inserted {
                    self.inserted = true;
                    record_orb_delegation_start(OrbDelegationStartInput {
                        path: Some(&self.path),
                        maestro_delegation_id: "reentrant-delegation",
                        maestro_session_id: "reentrant-session",
                        tenant_id: "reentrant-tenant",
                        connection_ref: Some("reentrant-connection"),
                        orb_thread_id: Some("reentrant-thread"),
                        orb_task_id: None,
                        launch_receipt_id: Some("reentrant-launch"),
                        operation_id: "reentrant-operation",
                        idempotency_key: "reentrant-idempotency",
                        request_digest: "reentrant-digest",
                    })?;
                }
                Ok(Some(waiting_observation(2)))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap().to_owned();
        record_orb_delegation_start(start_input(Some(&path), "request-digest-a")).unwrap();
        let mut observer = ReentrantObserver {
            path: path.clone(),
            inserted: false,
        };
        let report = recover_orb_delegations_from_path(Some(&path), &mut observer).unwrap();
        assert_eq!(report.inspected, 1);
        assert_eq!(report.updated, 1);
        let ledger = load_task_ledger(Some(&path)).unwrap();
        assert_eq!(ledger.orb_delegations.len(), 2);
        assert_eq!(ledger.orb_delegations[0].state, OrbDelegationState::Waiting);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_resolve_to_one_ledger_transaction() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("tasks.json");
        let alias = directory.path().join("alias.json");
        record_orb_delegation_start(start_input(target.to_str(), "request-digest-a")).unwrap();
        symlink(&target, &alias).unwrap();
        assert_eq!(
            get_task_ledger_path(target.to_str()).unwrap(),
            get_task_ledger_path(alias.to_str()).unwrap()
        );
        let alias_task = fixture_task("alias-task");
        record_task_start(RecordTaskStartInput {
            path: alias.to_str(),
            peer: "peer-alias",
            peer_display_name: None,
            task: &alias_task,
            text: "alias task",
            message_id: None,
            context_id: None,
            kind: "message",
            metadata: None,
        })
        .unwrap();
        let ledger = load_task_ledger(target.to_str()).unwrap();
        assert_eq!(ledger.tasks.len(), 1);
        assert_eq!(ledger.orb_delegations.len(), 1);
    }

    #[test]
    fn ledger_parent_errors_fail_closed_before_lock_creation() {
        let directory = tempfile::tempdir().unwrap();
        let parent_file = directory.path().join("not-a-directory");
        fs::write(&parent_file, "not a directory").unwrap();
        let path = parent_file.join("tasks.json");
        let error = record_orb_delegation_start(start_input(path.to_str(), "request-digest-a"))
            .expect_err("a regular parent must not be treated as a ledger directory");
        assert!(error.to_string().contains("ledger"));
    }

    #[test]
    fn orb_restart_rehydrates_remote_ids_and_waiting_state_without_launching() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap();
        let mut ledger = TaskLedgerFile::default();
        upsert_orb_delegation(
            &mut ledger,
            start_input(Some(path), "request-digest-a"),
            "2026-08-20T00:00:00.000Z",
        )
        .unwrap();
        save_task_ledger(&ledger, Some(path)).unwrap();

        let mut rehydrated = load_task_ledger(Some(path)).unwrap();
        assert_eq!(rehydrated.orb_delegations.len(), 1);
        assert_eq!(
            rehydrated.orb_delegations[0].launch_receipt_id.as_deref(),
            Some("orb-launch-receipt-1")
        );
        assert_eq!(
            rehydrated.orb_delegations[0].state,
            OrbDelegationState::Starting
        );

        let mut observer = FixtureObserver {
            observations: VecDeque::from([Some(waiting_observation(2))]),
            observed_ids: Vec::new(),
        };
        let report =
            recover_orb_delegations(&mut rehydrated, &mut observer, "2026-08-20T00:00:03.000Z")
                .unwrap();
        assert_eq!(report.inspected, 1);
        assert_eq!(report.updated, 1);
        assert_eq!(report.unavailable, 0);
        assert_eq!(observer.observed_ids, vec!["maestro-delegation-1"]);
        assert_eq!(rehydrated.orb_delegations.len(), 1);
        assert_eq!(
            rehydrated.orb_delegations[0].orb_task_id.as_deref(),
            Some("orb-task-1")
        );
        assert_eq!(
            rehydrated.orb_delegations[0].state,
            OrbDelegationState::Waiting
        );
        save_task_ledger(&rehydrated, Some(path)).unwrap();

        let reloaded = load_task_ledger(Some(path)).unwrap();
        assert_eq!(reloaded.orb_delegations.len(), 1);
        assert_eq!(
            reloaded.orb_delegations[0].idempotency_key,
            "fixture-replay-1"
        );
        assert_eq!(
            reloaded.orb_delegations[0].orb_thread_id.as_deref(),
            Some("orb-thread-1")
        );
    }

    #[test]
    fn orb_recovery_path_persists_observation_under_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let path = path.to_str().unwrap();
        record_orb_delegation_start(start_input(Some(path), "request-digest-a")).unwrap();

        let mut observer = FixtureObserver {
            observations: VecDeque::from([Some(waiting_observation(2))]),
            observed_ids: Vec::new(),
        };
        let report = recover_orb_delegations_from_path(Some(path), &mut observer).unwrap();
        assert_eq!(report.inspected, 1);
        assert_eq!(report.updated, 1);
        assert_eq!(observer.observed_ids, vec!["maestro-delegation-1"]);

        let ledger = load_task_ledger(Some(path)).unwrap();
        assert_eq!(ledger.orb_delegations[0].state, OrbDelegationState::Waiting);
        assert_eq!(ledger.orb_delegations[0].observed_revision, 2);
        assert_eq!(
            ledger.orb_delegations[0].orb_task_id.as_deref(),
            Some("orb-task-1")
        );
    }

    #[test]
    fn terminal_state_dominates_delayed_progress_and_mapping_is_typed() {
        assert_eq!(
            OrbObservedState::LaunchStarting.delegation_state(),
            OrbDelegationState::Starting
        );
        assert_eq!(
            OrbObservedState::TaskRunning.delegation_state(),
            OrbDelegationState::Running
        );
        assert_eq!(
            OrbObservedState::TaskWaiting.delegation_state(),
            OrbDelegationState::Waiting
        );
        assert_eq!(
            OrbObservedState::TaskTerminal.delegation_state(),
            OrbDelegationState::Terminal
        );
        assert_eq!(
            OrbObservedState::Unavailable.delegation_state(),
            OrbDelegationState::Unavailable
        );

        let mut ledger = TaskLedgerFile::default();
        upsert_orb_delegation(
            &mut ledger,
            start_input(None, "request-digest-a"),
            "2026-08-20T00:00:00.000Z",
        )
        .unwrap();
        assert!(
            reconcile_orb_delegation(
                &mut ledger,
                "maestro-delegation-1",
                OrbDelegationObservation {
                    state: OrbObservedState::TaskRunning,
                    revision: 1,
                    ..waiting_observation(1)
                },
                "2026-08-20T00:00:01.000Z",
            )
            .unwrap()
        );
        assert!(
            reconcile_orb_delegation(
                &mut ledger,
                "maestro-delegation-1",
                OrbDelegationObservation {
                    state: OrbObservedState::TaskTerminal,
                    revision: 2,
                    ..waiting_observation(2)
                },
                "2026-08-20T00:00:02.000Z",
            )
            .unwrap()
        );
        assert!(
            !reconcile_orb_delegation(
                &mut ledger,
                "maestro-delegation-1",
                OrbDelegationObservation {
                    state: OrbObservedState::TaskRunning,
                    revision: 3,
                    ..waiting_observation(3)
                },
                "2026-08-20T00:00:03.000Z",
            )
            .unwrap()
        );
        assert_eq!(
            ledger.orb_delegations[0].state,
            OrbDelegationState::Terminal
        );
    }

    #[test]
    fn missing_remote_observation_becomes_unavailable_without_duplicate_rows() {
        let mut ledger = TaskLedgerFile::default();
        upsert_orb_delegation(
            &mut ledger,
            start_input(None, "request-digest-a"),
            "2026-08-20T00:00:00.000Z",
        )
        .unwrap();
        let mut observer = FixtureObserver {
            observations: VecDeque::new(),
            observed_ids: Vec::new(),
        };

        let first = recover_orb_delegations(&mut ledger, &mut observer, "2026-08-20T00:00:01.000Z")
            .unwrap();
        let second =
            recover_orb_delegations(&mut ledger, &mut observer, "2026-08-20T00:00:02.000Z")
                .unwrap();
        assert_eq!(first.unavailable, 1);
        assert_eq!(second.unavailable, 1);
        assert_eq!(ledger.orb_delegations.len(), 1);
        assert_eq!(observer.observed_ids.len(), 2);
        assert_eq!(
            ledger.orb_delegations[0].state,
            OrbDelegationState::Unavailable
        );
    }
}
