//! Durable, provider-neutral child-agent delegation.
//!
//! A subagent is a real [`NativeAgent`] with its own provider conversation,
//! session journal, and optional git worktree. The parent only receives a
//! compact handle from `spawn_subagent`; the other lifecycle tools read the
//! durable record and can therefore observe or resume a child from another
//! `ToolExecutor` in the same process.

use self::factory::{ChildAgentFactory, ChildLaunchRequest, LocalChildAgentFactory};
use maestro_runtime::agent::NativeAgent;
use maestro_runtime_contracts::tool_wire;

pub mod factory;
mod handoff;
mod lifecycle;

use crate::model_dynamics::{ModelChoice, TaskDifficulty};
use crate::session::ThinkingLevel;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use maestro_runtime::{DelegationControlAction, DelegationEvent, DelegationLifecycleState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    CredentialVault, FromAgent, NativeAgentConfig, PromptKind, SteerSignal, ToolResult,
};
use crate::config::InboundControlPolicy;
use crate::headless::{FromAgentMessage, SessionRecorder, ToAgentMessage};
use crate::hooks::{HookResult, IntegratedHookSystem};
use crate::mailbox::{MailboxControlMode as ControlMode, MailboxLifecycleStatus};
use crate::orb_connection::HostedOrbOwnerBinding;
use crate::sandbox::SandboxPolicy;
use crate::session::{SessionLock, sanitize_path_for_dirname};
use crate::state::ApprovalMode;
use crate::tools::ToolRegistry;
use crate::worktree::WorktreeSession;

use crate::tools::orb_delegation::{
    OrbConsoleAction, OrbDelegateRequest, OrbDelegationAdapter, OrbDelegationConfig,
    OrbSpawnSettings, deterministic_idempotency_key, normalize_orb_controls,
    normalize_orb_lifecycle, orb_delegation_event,
};

/// Built-in tools which belong to this lifecycle surface and must not be
/// advertised to a child. Without this guard a child could recursively spawn
/// an unbounded tree of agents.
pub(crate) const SUBAGENT_TOOL_NAMES: [&str; 9] = [
    "spawn_subagent",
    "list_subagents",
    "get_subagent",
    "wait_subagent",
    "resume_subagent",
    "cancel_subagent",
    "control_subagent",
    "inspect_subagent",
    "cleanup_subagent",
];

const MAX_TASK_BYTES: usize = 64 * 1024;
const MAX_WAIT_MS: u64 = 300_000;
const TERMINAL_SNAPSHOT_WAIT: Duration = Duration::from_millis(500);
const DEFAULT_CHILD_MAX_TOKENS: u32 = 16_384;
const MAX_CHILD_MAX_TOKENS: u32 = 131_072;
const DEFAULT_CHILD_TIMEOUT_MS: u64 = 7_200_000;
const MAX_CHILD_TIMEOUT_MS: u64 = 86_400_000;
const DEFAULT_MAX_RUNNING_SUBAGENTS: usize = 4;
const MAX_RUNNING_SUBAGENTS: usize = 32;
const RUNTIME_CONTROL_CAPACITY: usize = 64;
#[cfg(not(test))]
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(test)]
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::ZERO;
#[cfg(not(test))]
const LIFECYCLE_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(test)]
const LIFECYCLE_RECONCILIATION_INTERVAL: Duration = Duration::ZERO;

fn terminal_checkpoint_ready(terminal_seen: bool, semantic_snapshot_seen: bool) -> bool {
    terminal_seen && semantic_snapshot_seen
}

fn orb_status_allows_resume(
    lifecycle: DelegationLifecycleState,
    available_commands: &[String],
) -> bool {
    if matches!(
        lifecycle,
        DelegationLifecycleState::Paused | DelegationLifecycleState::NeedsAttention
    ) {
        return true;
    }

    let controls = normalize_orb_controls(available_commands);
    controls.contains(&DelegationControlAction::Resume)
        || controls.contains(&DelegationControlAction::Retry)
}

/// Characters of streamed assistant text counted as one output token when the
/// runtime reports no usage.
///
/// Four characters per token is the common English approximation. Code and
/// other dense text run closer to three, so this under-counts them: the
/// estimate reaches the budget later than the true token count would, never
/// earlier, and the run is cut off late rather than prematurely.
const CHILD_OUTPUT_CHARS_PER_TOKEN: u64 = 4;

/// Estimate output tokens from a count of streamed characters.
fn estimate_output_tokens(streamed_chars: u64) -> u64 {
    streamed_chars.div_ceil(CHILD_OUTPUT_CHARS_PER_TOKEN)
}

/// Whether the run has spent its cumulative output allowance, counting text
/// streamed since the last response boundary at the estimated rate.
///
/// Checked while a response streams so one long turn cannot overrun the budget
/// and only be noticed at the boundary that follows it. Only meaningful for a
/// runtime that reports no usage: see [`child_output_is_metered`].
fn child_output_budget_exhausted(used_tokens: u64, streamed_chars: u64, max_tokens: u32) -> bool {
    used_tokens.saturating_add(estimate_output_tokens(streamed_chars)) >= u64::from(max_tokens)
}

/// Characters of model-produced output carried by one tool call.
///
/// The tool name and its serialized arguments are tokens the model generated,
/// so they count against an unmetered run's budget exactly like assistant text.
/// A tool call whose arguments cannot be serialized contributes only its name;
/// under-counting is the safe direction, matching the estimate itself.
fn tool_call_output_chars(tool: &str, args: &serde_json::Value) -> u64 {
    let argument_chars = serde_json::to_string(args)
        .map(|json| json.chars().count())
        .unwrap_or(0);
    (tool.chars().count() as u64).saturating_add(argument_chars as u64)
}

/// Whether the child's runtime reports exact output-token usage.
///
/// Everything except the Codex app-server path returns usage on every
/// `ResponseEnd`, and the runner clamps each request to the unspent budget from
/// that exact count, so those runs are already bounded. Applying the
/// four-characters-per-token estimate on top of an exact count can only make
/// the parent cancel a response the budget still allowed, so the estimate is
/// confined to the unmetered path.
fn child_output_is_metered(model: &str) -> bool {
    !crate::codex_auth::model_uses_openai_codex(model)
}

/// Tracks whether a child's runtime actually reports output-token usage.
///
/// The model name is not a reliable answer. An OpenAI-compatible endpoint may
/// omit the usage chunk on any turn, in which case `ResponseEnd` carries no
/// usage even though the model is not Codex, and a name-based rule charged that
/// turn nothing at all. Observation decides instead: a turn that reports usage
/// is charged exactly, and a turn that reports none is charged the estimate.
#[derive(Debug, Default, Clone, Copy)]
struct ChildOutputMetering {
    /// True for the Codex app-server path, which reports no usage by design and
    /// also accepts no per-request `max_tokens`.
    known_unmetered: bool,
    /// Set once a turn has ended without a usage report.
    observed_missing_usage: bool,
}

impl ChildOutputMetering {
    fn for_model(model: &str) -> Self {
        Self {
            known_unmetered: !child_output_is_metered(model),
            observed_missing_usage: false,
        }
    }

    /// Characters to charge for a turn that reported `usage`, if any.
    ///
    /// `None` means "do not estimate": the provider reported real numbers, so
    /// the exact count is used and the streamed characters are discarded rather
    /// than charged on top of it.
    fn estimate_for_turn(&mut self, usage_reported: bool, streamed_chars: u64) -> Option<u64> {
        if usage_reported {
            return None;
        }
        self.observed_missing_usage = true;
        Some(streamed_chars)
    }

    /// Whether to police the budget from streamed text before a turn ends.
    ///
    /// Only where an overrun would otherwise go unbounded. The Codex path sends
    /// no per-request `max_tokens`, so it is policed from the first turn. Every
    /// other runtime is bounded per request by the runner's clamp, so
    /// mid-stream estimation waits until a turn has actually failed to report
    /// usage -- estimating before that could cancel a response a metering
    /// provider would have allowed.
    fn enforces_mid_stream(self) -> bool {
        self.known_unmetered || self.observed_missing_usage
    }
}

/// Add one response's output tokens to the run total and report whether the
/// cumulative budget is spent.
///
/// The Codex app-server runtime reports no usage: `turn/start`
/// (`codex_app_server.rs`) carries no token fields in either direction, so
/// `ResponseEnd` arrives with `usage: None` and an unestimated run was never
/// charged for anything it produced. `estimate_chars` carries the assistant
/// text observed for that response and is `None` on a metered runtime, where an
/// absent usage report is a provider anomaly rather than the norm and guessing
/// would risk cancelling a response the budget still allowed.
fn record_child_output_tokens(
    used_tokens: &mut u64,
    usage: Option<&crate::agent::TokenUsage>,
    estimate_chars: Option<u64>,
    max_tokens: u32,
) -> bool {
    let spent = match (usage, estimate_chars) {
        (Some(usage), _) => usage.output_tokens,
        (None, Some(chars)) => estimate_output_tokens(chars),
        (None, None) => 0,
    };
    *used_tokens = used_tokens.saturating_add(spent);
    *used_tokens >= u64::from(max_tokens)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentBackend {
    #[default]
    Native,
    Orb,
}

impl SubagentBackend {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value
            .unwrap_or("native")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "native" | "local" => Ok(Self::Native),
            // `orb` is the persisted and wire compatibility spelling. New
            // model-facing requests should use the product name, Computer.
            "computer" | "orb" => Ok(Self::Orb),
            other => Err(format!(
                "backend must be either native or computer (orb is a compatibility alias); got {other}"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct OrbSubagentRef {
    pub thread_id: String,
    #[serde(default)]
    pub receipt_id: Option<String>,
    pub start_idempotency_key: String,
    pub config: OrbDelegationConfig,
    /// The opaque tenant and managed-connection identity captured before the
    /// first remote mutation. These fields intentionally exclude credentials.
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub connection_ref: Option<String>,
    #[serde(default)]
    pub managed_generation: Option<u64>,
    #[serde(default)]
    pub lifecycle_state: Option<String>,
    /// Provider-advertised controls are persisted only as an internal raw
    /// snapshot. Native surfaces render the typed projection instead.
    #[serde(default)]
    pub available_commands: Vec<String>,
}

fn admitting_orb_subagent_ref(
    config: OrbDelegationConfig,
    owner_binding: HostedOrbOwnerBinding,
    start_idempotency_key: String,
) -> OrbSubagentRef {
    OrbSubagentRef {
        // The atomic hosted-run path does not expose a thread id until
        // admission succeeds. Persist the operation key and owner before
        // dispatch so a crash cannot leave an unbound durable task.
        thread_id: String::new(),
        receipt_id: None,
        start_idempotency_key,
        config,
        organization_id: Some(owner_binding.organization_id),
        workspace_id: Some(owner_binding.workspace_id),
        connection_ref: Some(owner_binding.connection_ref),
        managed_generation: Some(owner_binding.managed_generation),
        lifecycle_state: Some("admitting".to_string()),
        available_commands: Vec::new(),
    }
}

impl SubagentStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Interrupted
        )
    }
}

impl From<MailboxLifecycleStatus> for SubagentStatus {
    fn from(status: MailboxLifecycleStatus) -> Self {
        match status {
            MailboxLifecycleStatus::Queued => Self::Queued,
            MailboxLifecycleStatus::Running => Self::Running,
            MailboxLifecycleStatus::Completed => Self::Completed,
            MailboxLifecycleStatus::Failed => Self::Failed,
            MailboxLifecycleStatus::Cancelled => Self::Cancelled,
            MailboxLifecycleStatus::TimedOut => Self::TimedOut,
            MailboxLifecycleStatus::Interrupted => Self::Interrupted,
        }
    }
}

impl From<SubagentStatus> for MailboxLifecycleStatus {
    fn from(status: SubagentStatus) -> Self {
        match status {
            SubagentStatus::Queued => Self::Queued,
            SubagentStatus::Running => Self::Running,
            SubagentStatus::Completed => Self::Completed,
            SubagentStatus::Failed => Self::Failed,
            SubagentStatus::Cancelled => Self::Cancelled,
            SubagentStatus::TimedOut => Self::TimedOut,
            SubagentStatus::Interrupted => Self::Interrupted,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentRole {
    Explore,
    Plan,
    Code,
    Review,
}

impl SubagentRole {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("code").trim().to_ascii_lowercase().as_str() {
            "explore" | "explorer" => Ok(Self::Explore),
            "plan" | "planner" => Ok(Self::Plan),
            "code" | "coder" | "implement" => Ok(Self::Code),
            "review" | "reviewer" => Ok(Self::Review),
            other => Err(format!(
                "role must be one of explore, plan, code, or review; got {other}"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::Code => "code",
            Self::Review => "review",
        }
    }

    /// Whether a child in this role is allowed to change anything.
    ///
    /// `Code` is the only role that writes. The others are advertised to the
    /// user as read-only, so the sandbox policy and the native-approval gate
    /// both have to enforce that, not just the advertised tool list.
    fn can_mutate(self) -> bool {
        matches!(self, Self::Code)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentIsolation {
    Shared,
    Worktree,
}

impl SubagentIsolation {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value
            .unwrap_or("worktree")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "shared" | "workspace" => Ok(Self::Shared),
            "worktree" | "isolated" => Ok(Self::Worktree),
            other => Err(format!(
                "isolation must be either shared or worktree; got {other}"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SubagentResult {
    pub output: String,
    pub files_modified: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentLifecycleEvent {
    pub mailbox_message_id: String,
    pub subagent_id: String,
    pub attempt: u32,
    pub parent_scope_id: String,
    pub parent_call_id: String,
    pub status: SubagentStatus,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub finished_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinationSnapshot {
    pub subagent_id: String,
    pub agent_ref: String,
    pub parent_scope_id: String,
    pub role: String,
    pub status: String,
    pub attempt: u32,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub lifecycle_published: bool,
    pub last_control_id: Option<String>,
    pub last_control_mode: Option<String>,
    pub last_control_state: Option<String>,
    pub held_control_id: Option<String>,
    pub error: Option<String>,
}

fn displayed_coordination_control<'a>(
    controls: &'a [&'a crate::mailbox::MailboxMessage],
) -> (
    Option<&'a crate::mailbox::MailboxMessage>,
    Option<&'a crate::mailbox::MailboxMessage>,
) {
    let newer = |left: &&&crate::mailbox::MailboxMessage,
                 right: &&&crate::mailbox::MailboxMessage| {
        left.created_at_unix
            .cmp(&right.created_at_unix)
            .then_with(|| left.id.cmp(&right.id))
    };
    let latest = controls.iter().max_by(newer).copied();
    let held = controls
        .iter()
        .filter(|message| message.delivery_state == crate::mailbox::MailboxDeliveryState::Held)
        .max_by(newer)
        .copied();
    (held.or(latest), held)
}

pub fn coordination_snapshots(cwd: &Path) -> Result<Vec<CoordinationSnapshot>, String> {
    let manager = SubagentManager::new(cwd.to_path_buf());
    let entries = std::fs::read_dir(&manager.root)
        .map_err(|error| format!("read subagent registry: {error}"))?;
    let mailbox = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
        .map_err(|error| format!("load coordination mailbox: {error:#}"))?;
    let mut snapshots = Vec::new();
    for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(record) = manager.load_record(&id) else {
            continue;
        };
        let reference = agent_ref(&record);
        let controls = mailbox
            .messages
            .iter()
            .filter(|message| {
                message.recipient == reference
                    && matches!(
                        message.payload,
                        crate::mailbox::MailboxPayload::SubagentControl { .. }
                    )
            })
            .collect::<Vec<_>>();
        let (displayed_control, held_control) = displayed_coordination_control(&controls);
        let last_control_mode = displayed_control.and_then(|message| match message.payload {
            crate::mailbox::MailboxPayload::SubagentControl { mode } => {
                Some(mode.label().to_string())
            }
            _ => None,
        });
        snapshots.push(CoordinationSnapshot {
            subagent_id: record.id.clone(),
            agent_ref: reference,
            parent_scope_id: record.last_parent_scope_id.clone(),
            role: record.role.label().to_string(),
            status: status_label(record.status).to_string(),
            attempt: record.attempt,
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            finished_at_ms: record.finished_at_ms,
            lifecycle_published: record.lifecycle_notification_published,
            last_control_id: displayed_control.map(|message| message.id.clone()),
            last_control_mode,
            last_control_state: displayed_control
                .map(|message| format!("{:?}", message.delivery_state).to_ascii_lowercase()),
            held_control_id: held_control.map(|message| message.id.clone()),
            error: record.error.clone(),
        });
    }
    snapshots.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| left.subagent_id.cmp(&right.subagent_id))
    });
    Ok(snapshots)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SubagentRecord {
    pub id: String,
    pub parent_scope_id: String,
    pub parent_call_id: String,
    pub last_parent_scope_id: String,
    pub last_call_id: String,
    pub task: String,
    pub current_prompt: String,
    /// Source user messages carried by the parent; historical context, never approval.
    #[serde(default)]
    pub parent_requests: Vec<String>,
    pub role: SubagentRole,
    #[serde(default)]
    pub backend: SubagentBackend,
    #[serde(default)]
    pub orb: Option<OrbSubagentRef>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub profile_prompt: Option<String>,
    #[serde(default)]
    pub profile_tools: Option<Vec<String>>,
    pub model: Option<String>,
    #[serde(default)]
    pub thinking: Option<ThinkingLevel>,
    #[serde(default)]
    pub difficulty: TaskDifficulty,
    #[serde(default = "default_child_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_child_max_tokens")]
    pub max_tokens: u32,
    pub isolation: SubagentIsolation,
    pub cwd: String,
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub worktree_cleaned: bool,
    #[serde(default)]
    pub initial_files: Vec<String>,
    #[serde(default)]
    pub initial_file_fingerprints: HashMap<String, String>,
    #[serde(default)]
    pub initial_head: Option<String>,
    pub session_dir: String,
    pub status: SubagentStatus,
    pub attempt: u32,
    #[serde(default)]
    pub snapshot_attempt: Option<u32>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub result: Option<SubagentResult>,
    pub error: Option<String>,
    #[serde(default)]
    pub lifecycle_notification_published: bool,
}

/// Secret-free native projection of one hosted Computer task. The MCP server name,
/// endpoint, and credential references are deliberately absent; only durable
/// task identity and owner-authoritative controls cross into the console.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrbConsoleTask {
    pub id: String,
    pub agent_ref: String,
    pub task: String,
    pub attempt: u32,
    pub event: DelegationEvent,
    pub thread_id: Option<String>,
    pub receipt_id: Option<String>,
    pub recoverable: bool,
    pub result: Option<SubagentResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum OrbConsoleControl {
    Pause,
    Cancel,
}

impl OrbConsoleControl {
    fn label(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Cancel => "cancelled",
        }
    }

    fn idempotency_kind(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone)]
struct SpawnRequest {
    task: String,
    role: SubagentRole,
    backend: SubagentBackend,
    orb: OrbDelegationConfig,
    profile: Option<String>,
    profile_prompt: Option<String>,
    profile_tools: Option<Vec<String>>,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    difficulty: TaskDifficulty,
    timeout_ms: u64,
    max_tokens: u32,
    run_in_background: bool,
    isolation: SubagentIsolation,
    worktree_name: Option<String>,
}

struct ChildLaunch {
    lease: Option<SessionLock>,
    credential_vault: CredentialVault,
    parent_credential_vault: CredentialVault,
    parent_credential_generation: u64,
    parent_cancel: Option<CancellationToken>,
}

struct ChildRun {
    prompt: String,
    history: Option<Vec<crate::ai::Message>>,
    sandbox_policy: Option<SandboxPolicy>,
    token: CancellationToken,
    control_rx: mpsc::Receiver<RuntimeControlRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildControlOutcome {
    Continue,
    Interrupted,
    Cancelled,
}

struct ParentCredentialScope<'a> {
    vault: &'a CredentialVault,
    generation: u64,
}

struct WorktreeSetupGuard {
    session: Option<WorktreeSession>,
}

impl WorktreeSetupGuard {
    fn new(session: WorktreeSession) -> Self {
        Self {
            session: Some(session),
        }
    }

    fn copy_changes_from(&self, source: &Path) -> anyhow::Result<()> {
        self.session
            .as_ref()
            .expect("worktree setup guard must own a session")
            .copy_changes_from(source)
    }

    fn path_for(&self, source: &Path) -> anyhow::Result<PathBuf> {
        self.session
            .as_ref()
            .expect("worktree setup guard must own a session")
            .path_for(source)
    }

    fn path(&self) -> &Path {
        self.session
            .as_ref()
            .expect("worktree setup guard must own a session")
            .path()
    }

    fn disarm(mut self) {
        self.session.take();
    }
}

impl Drop for WorktreeSetupGuard {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.abort();
        }
    }
}

struct RuntimeRegistry {
    cancellation: Mutex<HashMap<String, CancellationToken>>,
    controls: Mutex<HashMap<String, mpsc::Sender<RuntimeControlRequest>>>,
    credential_scopes: Mutex<HashMap<String, CredentialVault>>,
    concurrency: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
struct RuntimeControlRequest {
    mailbox_id: String,
    recipient: String,
    mode: ControlMode,
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DurableControlReceipt {
    mailbox_message_id: String,
    #[serde(default)]
    queue_id: u64,
    mode: ControlMode,
    body: String,
    attempt: u32,
    accepted_at_ms: u64,
    #[serde(default)]
    acceptance_sequence: u64,
    #[serde(default)]
    state: DurableControlReceiptState,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DurableControlReceiptState {
    #[default]
    Accepted,
    Applied,
}

fn control_queue_id(mailbox_message_id: &str) -> u64 {
    let digest = Sha256::digest(mailbox_message_id.as_bytes());
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(prefix) & (u64::MAX >> 1)).max(1)
}

fn control_receipt_needs_replay(
    receipt: &DurableControlReceipt,
    snapshot_attempt: Option<u32>,
    processed_queue_ids: &HashSet<u64>,
) -> bool {
    !processed_queue_ids.contains(&receipt.queue_id)
        && snapshot_attempt.is_none_or(|attempt| receipt.attempt >= attempt)
}

impl RuntimeRegistry {
    fn new() -> Self {
        let configured = std::env::var("MAESTRO_MAX_RUNNING_SUBAGENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_RUNNING_SUBAGENTS)
            .clamp(1, MAX_RUNNING_SUBAGENTS);
        Self::with_capacity(configured)
    }

    fn with_capacity(max_running: usize) -> Self {
        Self {
            cancellation: Mutex::new(HashMap::new()),
            controls: Mutex::new(HashMap::new()),
            credential_scopes: Mutex::new(HashMap::new()),
            concurrency: Arc::new(Semaphore::new(max_running.max(1))),
        }
    }

    async fn acquire_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        self.concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| format!("subagent scheduler closed: {error}"))
    }

    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.concurrency.available_permits()
    }

    fn insert(
        &self,
        id: &str,
        token: CancellationToken,
        control: mpsc::Sender<RuntimeControlRequest>,
    ) {
        self.cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string(), token);
        self.controls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string(), control);
    }

    fn remove(&self, id: &str) {
        self.cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        self.controls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    fn get(&self, id: &str) -> Option<CancellationToken> {
        self.cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    fn send_control(&self, id: &str, request: RuntimeControlRequest) -> bool {
        self.controls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .is_some_and(|sender| sender.try_send(request).is_ok())
    }

    fn set_credential_scope(&self, id: &str, vault: CredentialVault) {
        self.credential_scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_string(), vault);
    }

    fn credential_scope(&self, id: &str) -> Option<CredentialVault> {
        self.credential_scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    fn running_ids(&self) -> Vec<String> {
        self.controls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

fn runtime_registry() -> Arc<RuntimeRegistry> {
    static REGISTRY: OnceLock<Arc<RuntimeRegistry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Arc::new(RuntimeRegistry::new()))
        .clone()
}

/// Manager shared by one or more tool executors for a workspace.
#[derive(Clone)]
pub(crate) struct SubagentManager {
    cwd: PathBuf,
    root: PathBuf,
    /// Scope that owns the children this manager spawns and the lifecycle
    /// events it drains.
    ///
    /// Shared and mutable because the tool executor holding this manager lives
    /// behind an `Arc` for the whole process while the conversation it serves
    /// does not: a new or resumed session rotates the scope in place so a child
    /// started by an earlier conversation cannot report into a later one.
    parent_scope_id: Arc<Mutex<String>>,
    parent_model: Arc<Mutex<Option<ModelChoice>>>,
    parent_requests: Arc<Mutex<Vec<String>>>,
    runtime: Arc<RuntimeRegistry>,
    child_factory: Arc<dyn ChildAgentFactory>,
    mailbox_path: PathBuf,
    last_lifecycle_poll: Arc<Mutex<Instant>>,
    last_lifecycle_reconciliation: Arc<Mutex<Instant>>,
    observed_lifecycle_records: Arc<Mutex<HashMap<String, LifecycleRecordObservation>>>,
    pending_lifecycle: Arc<Mutex<HashSet<String>>>,
    /// Acceptance reads actual execution output, never agent-writable record files.
    coding_validator_receipts: Arc<Mutex<HashMap<String, Option<SubagentRecord>>>>,
    orb_adapter: Arc<RwLock<Option<OrbDelegationAdapter>>>,
    /// The runner's "a steering message is queued" signal, when this manager
    /// belongs to a runner that owns a message queue.
    ///
    /// `None` for executors built outside a conversation (tests, one-shot
    /// tooling); those keep the old sleep-until-timeout behavior because
    /// there is no queue for a user message to land in.
    steer_signal: Arc<Mutex<Option<Arc<SteerSignal>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LifecycleRecordObservation {
    modified: Option<SystemTime>,
    len: u64,
    file_id: Option<u64>,
    attempt: u32,
    published: bool,
}

#[cfg(unix)]
fn lifecycle_file_id(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn lifecycle_file_id(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

impl SubagentManager {
    pub(crate) fn with_child_agent_factory(mut self, factory: Arc<dyn ChildAgentFactory>) -> Self {
        self.child_factory = factory;
        self
    }

    pub(crate) fn child_agent_factory(&self) -> Arc<dyn ChildAgentFactory> {
        Arc::clone(&self.child_factory)
    }

    pub(crate) fn set_parent_model(&self, choice: ModelChoice) {
        *self.parent_model.lock().expect("parent model mutex") = Some(choice);
    }

    pub(crate) fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let root = default_root(&cwd);
        Self::with_root_parent_and_mailbox(
            cwd,
            root,
            uuid::Uuid::new_v4().to_string(),
            crate::mailbox::default_path(),
        )
    }

    #[cfg(test)]
    fn with_root(cwd: PathBuf, root: PathBuf) -> Self {
        let mailbox_path = root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("mailbox.json");
        Self::with_root_parent_and_mailbox(
            cwd,
            root,
            uuid::Uuid::new_v4().to_string(),
            mailbox_path,
        )
    }

    pub(crate) fn with_parent_scope(
        cwd: impl Into<PathBuf>,
        parent_scope_id: impl Into<String>,
    ) -> Self {
        let cwd = cwd.into();
        let root = default_root(&cwd);
        Self::with_root_parent_and_mailbox(
            cwd,
            root,
            parent_scope_id.into(),
            crate::mailbox::default_path(),
        )
    }

    #[cfg(test)]
    fn with_root_and_parent_scope(cwd: PathBuf, root: PathBuf, parent_scope_id: String) -> Self {
        let mailbox_path = root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("mailbox.json");
        Self::with_root_parent_and_mailbox(cwd, root, parent_scope_id, mailbox_path)
    }

    fn with_root_parent_and_mailbox(
        cwd: PathBuf,
        root: PathBuf,
        parent_scope_id: String,
        mailbox_path: PathBuf,
    ) -> Self {
        Self {
            cwd,
            root,
            parent_scope_id: Arc::new(Mutex::new(parent_scope_id)),
            parent_model: Arc::new(Mutex::new(None)),
            parent_requests: Arc::new(Mutex::new(Vec::new())),
            runtime: runtime_registry(),
            child_factory: Arc::new(LocalChildAgentFactory),
            mailbox_path,
            last_lifecycle_poll: Arc::new(Mutex::new(
                Instant::now()
                    .checked_sub(LIFECYCLE_POLL_INTERVAL)
                    .unwrap_or_else(Instant::now),
            )),
            last_lifecycle_reconciliation: Arc::new(Mutex::new(
                Instant::now()
                    .checked_sub(LIFECYCLE_RECONCILIATION_INTERVAL)
                    .unwrap_or_else(Instant::now),
            )),
            observed_lifecycle_records: Arc::new(Mutex::new(HashMap::new())),
            pending_lifecycle: Arc::new(Mutex::new(HashSet::new())),
            coding_validator_receipts: Arc::new(Mutex::new(HashMap::new())),
            orb_adapter: Arc::new(RwLock::new(None)),
            steer_signal: Arc::new(Mutex::new(None)),
        }
    }

    /// Point this manager at the runner's steering signal.
    ///
    /// Blocking waits consult it so a user message ends the wait instead of
    /// sitting behind it.
    pub(crate) fn set_steer_signal(&self, signal: Arc<SteerSignal>) {
        let mut current = self
            .steer_signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = Some(signal);
    }

    fn steer_signal(&self) -> Option<Arc<SteerSignal>> {
        self.steer_signal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) async fn set_orb_adapter(&self, adapter: OrbDelegationAdapter) {
        *self.orb_adapter.write().await = Some(adapter);
    }

    async fn orb_adapter(&self) -> Option<OrbDelegationAdapter> {
        self.orb_adapter.read().await.clone()
    }

    fn validate_orb_owner_binding(
        record: &SubagentRecord,
        adapter: &OrbDelegationAdapter,
    ) -> Result<(), String> {
        if record.backend != SubagentBackend::Orb {
            return Ok(());
        }
        let Some(orb) = record.orb.as_ref() else {
            return Err(
                "Hosted Computer task has no durable owner binding; it cannot be safely resumed after a restart or connection change"
                    .to_string(),
            );
        };
        let Some(current) = adapter.owner_binding() else {
            return Err(
                "Hosted Computer owner binding is unavailable; refusing the remote operation until the managed account, workspace, and connection identity are available"
                    .to_string(),
            );
        };
        let (Some(organization_id), Some(workspace_id), Some(connection_ref), Some(generation)) = (
            orb.organization_id.as_deref(),
            orb.workspace_id.as_deref(),
            orb.connection_ref.as_deref(),
            orb.managed_generation,
        ) else {
            return Err(
                "Hosted Computer task has no durable owner binding; it cannot be safely resumed after a restart or connection change"
                    .to_string(),
            );
        };
        let expected = HostedOrbOwnerBinding {
            organization_id: organization_id.trim().to_string(),
            workspace_id: workspace_id.trim().to_string(),
            connection_ref: connection_ref.trim().to_string(),
            managed_generation: generation,
        };
        if expected.organization_id.is_empty()
            || expected.workspace_id.is_empty()
            || expected.connection_ref.is_empty()
            || expected.managed_generation == 0
        {
            return Err(
                "Hosted Computer task has an incomplete durable owner binding; it cannot be safely resumed"
                    .to_string(),
            );
        }
        if &expected != current {
            return Err(
                "Hosted Computer owner binding changed with the active account, workspace, or managed connection; remote operation refused"
                    .to_string(),
            );
        }
        Ok(())
    }

    async fn orb_adapter_for_record(
        &self,
        record: &SubagentRecord,
    ) -> Result<OrbDelegationAdapter, String> {
        let Some(adapter) = self.orb_adapter().await else {
            return Err(
                "Computer backend is not configured; connect the managed Computer MCP server first"
                    .to_string(),
            );
        };
        Self::validate_orb_owner_binding(record, &adapter).map(|()| adapter)
    }

    pub(crate) fn uses_orb_backend(&self, args: &serde_json::Value) -> bool {
        if args
            .get("backend")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|backend| {
                backend.eq_ignore_ascii_case("computer") || backend.eq_ignore_ascii_case("orb")
            })
        {
            return true;
        }
        let id = subagent_id(args).map(str::to_string).or_else(|| {
            args.get("agent_ref")
                .or_else(|| args.get("agentRef"))
                .and_then(serde_json::Value::as_str)
                .and_then(|reference| parse_agent_ref(reference).ok())
                .map(|(id, _)| id)
        });
        id.and_then(|id| self.load_record(&id).ok())
            .is_some_and(|record| record.backend == SubagentBackend::Orb)
    }

    pub(crate) fn has_orb_records(&self) -> bool {
        std::fs::read_dir(&self.root)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter_map(|id| self.load_record(&id).ok())
            .any(|record| record.backend == SubagentBackend::Orb)
    }

    pub(crate) fn parent_scope_id(&self) -> String {
        self.parent_scope_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Point this manager at a different scope.
    ///
    /// Children already running keep the scope they were spawned under -- the
    /// record snapshots it -- so their completions stay addressed to the
    /// conversation that started them and are drained only by a manager holding
    /// that same scope.
    pub(crate) fn set_parent_scope_id(&self, parent_scope_id: String) {
        let mut current = self
            .parent_scope_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current != parent_scope_id {
            self.set_parent_requests(Vec::new());
        }
        *current = parent_scope_id;
    }

    pub(crate) fn poll_lifecycle_events(&self) -> Vec<SubagentLifecycleEvent> {
        let now = Instant::now();
        let mut last_poll = self
            .last_lifecycle_poll
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.duration_since(*last_poll) < LIFECYCLE_POLL_INTERVAL {
            return Vec::new();
        }
        *last_poll = now;
        drop(last_poll);

        self.retry_terminal_lifecycle_notifications();

        let parent_scope = self.parent_scope_id();
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&self.mailbox_path);
        let mut events = Vec::new();
        loop {
            let message = match mailbox.claim_typed(&parent_scope, |payload| {
                matches!(
                    payload,
                    crate::mailbox::MailboxPayload::SubagentLifecycle { .. }
                )
            }) {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => break,
            };
            let crate::mailbox::MailboxPayload::SubagentLifecycle {
                subagent_id,
                parent_call_id,
                attempt,
                status,
                summary,
                error,
                finished_at_ms,
            } = message.payload
            else {
                continue;
            };
            events.push(SubagentLifecycleEvent {
                mailbox_message_id: message.id,
                subagent_id,
                attempt,
                parent_scope_id: parent_scope.clone(),
                parent_call_id,
                status: status.into(),
                summary,
                error,
                finished_at_ms,
            });
        }
        events
    }

    pub(crate) fn acknowledge_lifecycle_event(
        &self,
        event: &SubagentLifecycleEvent,
    ) -> Result<(), String> {
        crate::mailbox::MailboxStore::with_path(&self.mailbox_path)
            .complete_delivery(&event.mailbox_message_id, &event.parent_scope_id, None)
            .map(|_| ())
            .map_err(|error| format!("acknowledge subagent lifecycle event: {error:#}"))
    }

    pub(crate) fn set_parent_requests(&self, requests: Vec<String>) {
        *self
            .parent_requests
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = requests;
    }

    fn parent_request_snapshot(&self) -> Result<Vec<String>, String> {
        let parent_requests: Vec<String> = self
            .parent_requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|text| {
                crate::agent::credential_store::redact_credentials_in_json(
                    &serde_json::Value::String(text.clone()),
                )
                .as_str()
                .unwrap_or_default()
                .to_owned()
            })
            .collect();
        if serde_json::to_vec(&parent_requests)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_TASK_BYTES
        {
            return Err("Parent task context exceeds the worker context limit; start a scoped task before delegating".into());
        }
        Ok(parent_requests)
    }

    /// Project local activity from the live registry and durable control owner.
    pub(crate) fn worker_activity(&self) -> Result<(usize, usize), String> {
        let scope = self.parent_scope_id();
        let mut running = 0;
        for id in self.runtime.running_ids() {
            if let Ok(record) = self.load_record(&id) {
                if record.last_parent_scope_id == scope {
                    running += 1;
                }
            }
        }
        let mailbox = crate::mailbox::MailboxStore::load_from_path(&self.mailbox_path)
            .map_err(|error| format!("read worker controls: {error}"))?;
        let waiting = mailbox
            .messages
            .iter()
            .filter(|message| {
                message.delivery_state == crate::mailbox::MailboxDeliveryState::Held
                    && matches!(
                        message.payload,
                        crate::mailbox::MailboxPayload::SubagentControl { .. }
                    )
            })
            .filter_map(|message| parse_agent_ref(&message.recipient).ok())
            .filter_map(|(id, attempt)| {
                self.load_record(&id).ok().filter(|record| {
                    record.attempt == attempt && record.last_parent_scope_id == scope
                })
            })
            .map(|record| record.id)
            .collect::<HashSet<_>>()
            .len();
        Ok((running, waiting))
    }

    pub(crate) fn active_mailbox_recipients(&self) -> Vec<String> {
        let mut recipients = self
            .runtime
            .running_ids()
            .into_iter()
            .filter_map(|id| self.load_record(&id).ok())
            .map(|record| agent_ref(&record))
            .collect::<Vec<_>>();
        recipients.sort();
        recipients.dedup();
        recipients
    }

    fn retry_terminal_lifecycle_notifications(&self) {
        self.reconcile_lifecycle_records();

        let pending: Vec<_> = self
            .pending_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect();
        for id in pending {
            let published = self.retry_lifecycle_notification(&id);
            if published {
                self.pending_lifecycle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
            }
        }
    }

    fn reconcile_lifecycle_records(&self) {
        let now = Instant::now();
        let mut last_scan = self
            .last_lifecycle_reconciliation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.duration_since(*last_scan) < LIFECYCLE_RECONCILIATION_INTERVAL {
            return;
        }
        *last_scan = now;
        drop(last_scan);

        if let Ok(entries) = std::fs::read_dir(&self.root) {
            let mut observed = self
                .observed_lifecycle_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut pending = self
                .pending_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut present = HashSet::new();
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(id) = entry.file_name().to_str() {
                        present.insert(id.to_string());
                        let record_path = entry.path().join("record.json");
                        let metadata = std::fs::metadata(&record_path).ok();
                        let modified = metadata.as_ref().and_then(|value| value.modified().ok());
                        let len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
                        let file_id = metadata.as_ref().and_then(lifecycle_file_id);
                        if modified.is_some()
                            && file_id.is_some()
                            && observed.get(id).is_some_and(|state| {
                                state.modified == modified
                                    && state.len == len
                                    && state.file_id == file_id
                            })
                        {
                            continue;
                        }
                        if let Ok(record) = self.load_record(id) {
                            let state = LifecycleRecordObservation {
                                modified,
                                len,
                                file_id,
                                attempt: record.attempt,
                                published: record.lifecycle_notification_published,
                            };
                            if observed.get(id) != Some(&state) {
                                observed.insert(id.to_string(), state);
                                if !record.lifecycle_notification_published {
                                    pending.insert(id.to_string());
                                }
                            }
                        }
                    }
                }
            }
            observed.retain(|id, _| present.contains(id));
        }
    }

    fn retry_lifecycle_notification(&self, id: &str) -> bool {
        let Ok(initial) = self.load_record(id) else {
            return false;
        };
        if initial.lifecycle_notification_published {
            return true;
        }
        if !initial.status.is_terminal() {
            return false;
        }
        // `resume` cannot acquire its execution lease when the session
        // directory is absent, so `None` is also mutually exclusive with a
        // running attempt. Otherwise this takes the exact same sidecar lock.
        let Ok(_lease) = Self::acquire_cleanup_lease(&initial) else {
            return false;
        };
        let Ok(mut current) = self.load_record(id) else {
            return false;
        };
        if current.lifecycle_notification_published || !current.status.is_terminal() {
            return true;
        }
        self.publish_lifecycle_notification(&mut current).is_ok()
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root.join(id).join("record.json")
    }

    fn validate_id(id: &str) -> Result<String, String> {
        let id = id.trim();
        uuid::Uuid::parse_str(id)
            .map(|_| id.to_string())
            .map_err(|_| format!("invalid subagent_id `{id}`"))
    }

    fn write_record(&self, record: &SubagentRecord) -> Result<(), String> {
        let path = self.record_path(&record.id);
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| format!("serialize subagent record: {error}"))?;
        crate::fs_atomic::write_atomic(path, bytes)
            .map_err(|error| format!("persist subagent record: {error}"))
    }

    fn load_record(&self, id: &str) -> Result<SubagentRecord, String> {
        let id = Self::validate_id(id)?;
        let path = self.record_path(&id);
        let bytes =
            std::fs::read(&path).map_err(|error| format!("read subagent {id} record: {error}"))?;
        let mut record: SubagentRecord = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse subagent {id} record: {error}"))?;
        if record.backend != SubagentBackend::Orb
            && matches!(
                record.status,
                SubagentStatus::Queued | SubagentStatus::Running
            )
            && self.runtime.get(&record.id).is_none()
            && !Self::execution_lease_is_held(&record)
        {
            record.status = SubagentStatus::Interrupted;
            record.finished_at_ms = Some(now_millis());
            record.error = Some(
                "Deixic Code restarted while this subagent was queued or running; resume it to continue"
                    .to_string(),
            );
            self.write_record(&record)?;
        }
        Ok(record)
    }

    /// Whether some process still holds the execution lease for `record`.
    ///
    /// `self.runtime` only knows about children this process launched, so a
    /// second Maestro sharing the workspace sees every live child of the first
    /// one as orphaned. The lease — the advisory lock the run path holds on
    /// the child's timeline file for as long as the child runs (see `spawn`,
    /// `resume`, and `run_child`) — is the cross-process signal, so consult it
    /// before rewriting a record to `Interrupted`. `Interrupted` is terminal,
    /// and `cleanup` force-removes the worktree of any terminal child.
    ///
    /// Anything other than a clean "nobody holds it" answer counts as held.
    /// A false positive leaves a genuinely dead record `Running` until the
    /// next load; a false negative deletes a live child's worktree underneath
    /// it.
    fn execution_lease_is_held(record: &SubagentRecord) -> bool {
        match SessionLock::is_held(&Self::timeline_path(record)) {
            Ok(held) => held,
            Err(error) => {
                eprintln!(
                    "subagent {}: could not read the execution lease ({error}); treating the child as still running",
                    record.id
                );
                true
            }
        }
    }

    /// Take the execution lease for `record` so a cleanup cannot run
    /// concurrently with a resume.
    ///
    /// Returns `Ok(None)` when the child's session directory is gone: the lock
    /// file lives in that directory, so nothing can be holding a lease and no
    /// `resume` could start one either — it opens the same directory and would
    /// fail first. This mirrors [`SessionLock::is_held`], which reports a
    /// missing directory as "not held", and keeps cleanup usable for a record
    /// whose transcript has already been pruned.
    ///
    /// Every other failure — including an unreadable lock file — is reported
    /// as "in use". A false positive leaves a worktree on disk for the user to
    /// remove; a false negative deletes the worktree of a running child.
    /// Re-read `id` under a held execution lease and confirm it can still run.
    ///
    /// `cleanup` takes the same lease around its own check and removal, so once
    /// this lease is held the record on disk is settled: a cleanup either
    /// completed before it or cannot start until it is released. A child whose
    /// worktree has been removed cannot be resumed, and saying so leaves the
    /// record untouched rather than relaunching into a directory that is gone.
    fn revalidate_resumable_under_lease(&self, id: &str) -> Result<SubagentRecord, String> {
        let record = self.load_record(id)?;
        if record.worktree_cleaned {
            return Err(format!(
                "subagent {id} cannot be resumed: its worktree was cleaned up; spawn a new child instead"
            ));
        }
        let cwd = deserialize_repository_path(&record.cwd);
        if !cwd.exists() {
            return Err(format!(
                "subagent {id} cannot be resumed: its working directory {} no longer exists",
                cwd.display()
            ));
        }
        Ok(record)
    }

    fn acquire_cleanup_lease(record: &SubagentRecord) -> Result<Option<SessionLock>, String> {
        if !Self::session_dir(record).exists() {
            return Ok(None);
        }
        SessionLock::acquire(&Self::timeline_path(record))
            .map(Some)
            .map_err(|error| {
                format!(
                    "subagent {} is being run by another process; not removing its worktree ({error})",
                    record.id
                )
            })
    }

    fn session_dir(record: &SubagentRecord) -> PathBuf {
        deserialize_repository_path(&record.session_dir)
    }

    fn timeline_path(record: &SubagentRecord) -> PathBuf {
        Self::session_dir(record).join(format!("{}.jsonl", record.id))
    }

    pub(crate) async fn spawn(
        &self,
        args: &serde_json::Value,
        parent_call_id: &str,
        sandbox_policy: Option<SandboxPolicy>,
        credential_vault: CredentialVault,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        self.spawn_internal(
            args,
            parent_call_id,
            sandbox_policy,
            credential_vault,
            cancel,
            None,
        )
        .await
    }

    pub(crate) async fn spawn_coding_validator(
        &self,
        args: &serde_json::Value,
        parent_call_id: &str,
        sandbox_policy: Option<SandboxPolicy>,
        credential_vault: CredentialVault,
        cancel: Option<&CancellationToken>,
        role: crate::agents_cli::BuiltinValidatorRole,
    ) -> ToolResult {
        self.spawn_internal(
            args,
            parent_call_id,
            sandbox_policy,
            credential_vault,
            cancel,
            Some(role),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_internal(
        &self,
        args: &serde_json::Value,
        parent_call_id: &str,
        sandbox_policy: Option<SandboxPolicy>,
        credential_vault: CredentialVault,
        cancel: Option<&CancellationToken>,
        validator: Option<crate::agents_cli::BuiltinValidatorRole>,
    ) -> ToolResult {
        let mut request = match parse_spawn_request(args) {
            Ok(request) => request,
            Err(error) => return ToolResult::failure(error),
        };
        let parent_scope_id = self.parent_scope_id();
        // Orb provisions the isolated workspace on the hosted control plane;
        // a local Maestro worktree would create a second, unrelated isolation
        // boundary and cannot be sent to the hosted agent.
        if request.backend == SubagentBackend::Orb {
            request.isolation = SubagentIsolation::Shared;
        }
        let child_credential_vault = credential_vault.fork();

        if let Err(error) = apply_subagent_start_hook(&mut request, &self.cwd, &parent_scope_id) {
            return ToolResult::failure(error);
        }
        if let Some(role) = validator {
            let profile = crate::agents_cli::trusted_builtin_validator_profile(role);
            request.profile = Some(profile.name);
            request.profile_prompt = Some(profile.prompt);
            request.profile_tools = profile.tools;
            request.backend = SubagentBackend::Native;
            request.role = match role {
                crate::agents_cli::BuiltinValidatorRole::CodingReviewer => SubagentRole::Review,
                crate::agents_cli::BuiltinValidatorRole::CodingFlowValidator => SubagentRole::Code,
            };
            request.isolation = SubagentIsolation::Worktree;
            request.task = args
                .get("task")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
        } else if let Err(error) = resolve_spawn_profile(&mut request, &self.cwd) {
            return ToolResult::failure(error);
        }
        if request.backend == SubagentBackend::Native {
            let parent = self
                .parent_model
                .lock()
                .expect("parent model mutex")
                .clone()
                .unwrap_or_else(|| ModelChoice {
                    model: crate::codex_auth::resolve_default_model(),
                    thinking: ThinkingLevel::Off,
                });
            let resolved = crate::config::model_dynamics_config().resolve_child(
                request.difficulty,
                request.model.as_deref(),
                request.thinking,
                &parent,
            );
            request.model = Some(resolved.model.clone());
            request.thinking = Some(crate::model_dynamics::normalize_thinking(
                &resolved.model,
                resolved.thinking,
            ));
        }
        if request.backend == SubagentBackend::Orb {
            if let Err(error) = infer_hosted_computer_context(&mut request.orb, &self.cwd) {
                return ToolResult::failure(error);
            }
            apply_orb_delegation_policy(&mut request);
        }
        let parent_requests = match self.parent_request_snapshot() {
            Ok(requests) => requests,
            Err(error) => return ToolResult::failure(error),
        };
        // Local children can resolve and re-vault parent credentials. Hosted
        // Orb cannot consume Maestro's local references, and forwarding the
        // resolved plaintext would violate the hosted-credential boundary.
        if request.backend == SubagentBackend::Orb {
            if CredentialVault::has_references(&request.task) {
                return ToolResult::failure(
                    "Computer delegation cannot forward local credential references; provide a credential-free task",
                );
            }
        } else {
            // Resolve through the current parent scope first, then re-vault into
            // the child scope so the durable prompt never contains plaintext or
            // an unresolvable parent-only reference.
            let parent_resolved_task = credential_vault.resolve_all(&request.task);
            request.task = child_credential_vault.vault_in_text(&parent_resolved_task);
        }

        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return cancelled_result("spawn_subagent cancelled before launch");
        }

        // Resolve the owner before creating any durable Orb record. The
        // admitting record must never exist as a generic `backend=orb` record
        // with `orb: null`, because a crash in that window would leave a
        // restart unable to distinguish an unlaunched task from a task that
        // may safely be resumed.
        let orb_owner_binding = if request.backend == SubagentBackend::Orb {
            let Some(adapter) = self.orb_adapter().await else {
                return ToolResult::failure(
                    "Computer backend is not configured; connect the managed Computer MCP server first",
                );
            };
            let Some(owner_binding) = adapter.owner_binding().cloned() else {
                return ToolResult::failure(
                    "Hosted Computer owner binding is unavailable; connect the active managed account, workspace, and connection before launching",
                );
            };
            Some(owner_binding)
        } else {
            None
        };

        let id = uuid::Uuid::new_v4().to_string();
        let start_idempotency_key =
            deterministic_idempotency_key("start", &[&parent_scope_id, parent_call_id]);
        let orb = orb_owner_binding.map(|owner_binding| {
            admitting_orb_subagent_ref(request.orb.clone(), owner_binding, start_idempotency_key)
        });
        let (child_cwd, worktree_path, mut worktree_setup) = match request.isolation {
            SubagentIsolation::Shared => (self.cwd.clone(), None, None),
            SubagentIsolation::Worktree => {
                if !worktree_setup_allowed(sandbox_policy.as_ref()) {
                    return ToolResult::failure(
                        "worktree isolation requires an unrestricted sandbox because host-side git worktree setup writes repository metadata; use isolation=shared or danger-full-access",
                    );
                }
                let name = request
                    .worktree_name
                    .as_deref()
                    .unwrap_or("maestro-subagent");
                let name = format!("{name}-{id}");
                let session = match WorktreeSession::create_in(&self.cwd, &name) {
                    Ok(session) => session,
                    Err(error) => {
                        return ToolResult::failure(format!("create subagent worktree: {error:#}"));
                    }
                };
                let setup = WorktreeSetupGuard::new(session);
                if let Err(error) = setup.copy_changes_from(&self.cwd) {
                    return ToolResult::failure(format!(
                        "copy parent changes into subagent worktree: {error:#}"
                    ));
                }
                let child_cwd = match setup.path_for(&self.cwd) {
                    Ok(path) => path,
                    Err(error) => {
                        return ToolResult::failure(format!(
                            "map parent directory into subagent worktree: {error:#}"
                        ));
                    }
                };
                let worktree_path = setup.path().to_path_buf();
                (child_cwd, Some(worktree_path), Some(setup))
            }
        };
        let (initial_paths, initial_fingerprints) = changed_file_baseline(&child_cwd);
        let (initial_files, initial_file_fingerprints) =
            serialize_file_baseline(initial_paths, initial_fingerprints);
        let initial_head = git_repository_head(&child_cwd);

        let session_dir = self.root.join(&id).join("session");
        if let Err(error) = crate::fs_atomic::create_dir_all_synced(&session_dir) {
            return ToolResult::failure(format!("create subagent session directory: {error}"));
        }

        let now = now_millis();
        let cwd = serialize_repository_path(&child_cwd);
        let record = SubagentRecord {
            parent_requests,
            id: id.clone(),
            parent_scope_id: parent_scope_id.clone(),
            parent_call_id: parent_call_id.to_string(),
            last_parent_scope_id: parent_scope_id,
            last_call_id: parent_call_id.to_string(),
            task: request.task.clone(),
            current_prompt: request.task.clone(),
            role: request.role,
            backend: request.backend,
            orb,
            profile: request.profile.clone(),
            profile_prompt: request.profile_prompt.clone(),
            profile_tools: request.profile_tools.clone(),
            model: request.model.clone(),
            thinking: request.thinking,
            difficulty: request.difficulty,
            timeout_ms: request.timeout_ms,
            max_tokens: request.max_tokens,
            isolation: request.isolation,
            cwd,
            worktree_path: worktree_path
                .as_ref()
                .map(|path| serialize_repository_path(path)),
            worktree_cleaned: false,
            initial_files,
            initial_file_fingerprints,
            initial_head,
            session_dir: serialize_repository_path(&session_dir),
            status: SubagentStatus::Queued,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: now,
            started_at_ms: None,
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };

        let lease = match SessionLock::acquire(&Self::timeline_path(&record)) {
            Ok(lease) => lease,
            Err(error) => {
                return ToolResult::failure(format!(
                    "acquire subagent {} execution lease: {error}",
                    record.id
                ));
            }
        };

        let mut recorder = match SessionRecorder::with_id(&session_dir, &id) {
            Ok(recorder) => recorder,
            Err(error) => {
                return ToolResult::failure(format!("create subagent transcript: {error}"));
            }
        };
        if let Err(error) = recorder.record_sent(&ToAgentMessage::Prompt {
            content: request.task.clone(),
            attachments: None,
            managed_inference_authorization: None,
        }) {
            return ToolResult::failure(format!("record subagent prompt: {error}"));
        }
        if let Err(error) = recorder.flush_checkpoint() {
            return ToolResult::failure(format!("flush subagent prompt: {error}"));
        }
        drop(recorder);

        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }

        if request.backend == SubagentBackend::Orb {
            if let Some(setup) = worktree_setup.take() {
                setup.disarm();
            }
            return self
                .spawn_orb(record, request, parent_call_id, cancel)
                .await;
        }

        let token = CancellationToken::new();
        if validator.is_some() {
            self.coding_validator_receipts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.clone(), None);
        }
        let (control_tx, control_rx) = mpsc::channel(RUNTIME_CONTROL_CAPACITY);
        self.runtime
            .set_credential_scope(&id, child_credential_vault.clone());
        self.runtime.insert(&id, token.clone(), control_tx);
        let manager = self.clone();
        let launch_record = record.clone();
        let launch_policy = sandbox_policy;
        let launch_token = token.clone();
        let run_in_background = request.run_in_background;
        let parent_cancel = if run_in_background {
            None
        } else {
            cancel.cloned()
        };
        let parent_credential_generation = credential_vault.generation();
        let launch = ChildLaunch {
            lease: Some(lease),
            credential_vault: child_credential_vault,
            parent_credential_vault: credential_vault,
            parent_credential_generation,
            parent_cancel,
        };
        let role = request.role;
        let task = request.task.clone();
        let launch_id = id.clone();
        let launch_error_id = id.clone();
        let launch = async move {
            let cancellation_link = launch.parent_cancel.clone().map(|parent| {
                let child_token = launch_token.clone();
                tokio::spawn(async move {
                    parent.cancelled().await;
                    child_token.cancel();
                })
            });
            let result = manager
                .run_child(
                    launch_record,
                    ChildRun {
                        prompt: task,
                        history: None,
                        sandbox_policy: launch_policy,
                        token: launch_token,
                        control_rx,
                    },
                    launch,
                )
                .await;
            if let Some(cancellation_link) = cancellation_link {
                cancellation_link.abort();
            }
            manager.runtime.remove(&launch_id);
            result
        };

        if let Some(setup) = worktree_setup.take() {
            setup.disarm();
        }

        if run_in_background {
            let launch_error_id = launch_error_id.clone();
            tokio::spawn(async move {
                if let Err(error) = launch.await {
                    eprintln!("subagent {launch_error_id} failed: {error}");
                }
            });
            return ToolResult::success(format!(
                "Spawned {} subagent {} in the background",
                role.label(),
                id
            ))
            .with_details(record_details(&record));
        }

        match launch.await {
            Ok(record) => tool_result_for_record(record),
            Err(error) => ToolResult::failure(error),
        }
    }

    async fn spawn_orb(
        &self,
        mut record: SubagentRecord,
        request: SpawnRequest,
        _parent_call_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        let start_idempotency_key = {
            let Some(orb) = record.orb.as_ref() else {
                return self.finish_orb_record(
                    record,
                    "Hosted Computer task has no durable owner binding; refusing to launch",
                );
            };
            if !orb.thread_id.trim().is_empty() {
                return self.finish_orb_record(
                    record,
                    "Hosted Computer task already has a remote thread; refusing duplicate launch",
                );
            }
            orb.start_idempotency_key.clone()
        };
        let token = cancel.cloned().unwrap_or_else(CancellationToken::new);
        // The admitting record is the recovery fence for the remote mutation.
        // Persist it immediately before resolving the adapter or dispatching,
        // so cancellation or an uncertain response retains the same launch key.
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) => return self.finish_orb_record(record, error),
        };
        let handle = match adapter
            .delegate(
                &OrbDelegateRequest {
                    prompt: record.task.clone(),
                    project: request.orb.project.clone(),
                    profile: request.orb.profile.clone(),
                    settings: request.orb.settings.clone(),
                    start_idempotency_key: start_idempotency_key.clone(),
                },
                &token,
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => return self.finish_orb_record(record, error.to_string()),
        };
        if let Some(orb) = record.orb.as_mut() {
            orb.thread_id = handle.thread_id;
            orb.receipt_id = Some(handle.receipt_id);
            orb.lifecycle_state = Some("active".to_string());
        }
        record.started_at_ms = Some(now_millis());
        record.status = SubagentStatus::Running;
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        if request.run_in_background {
            return ToolResult::success(format!(
                "Spawned hosted Computer subagent {} in the background",
                record.id
            ))
            .with_details(record_details(&record));
        }
        self.wait_orb_until_terminal(&record.id, request.timeout_ms, cancel)
            .await
    }

    fn finish_orb_record(
        &self,
        mut record: SubagentRecord,
        error: impl Into<String>,
    ) -> ToolResult {
        let error = error.into();
        record.status = SubagentStatus::Failed;
        record.finished_at_ms = Some(now_millis());
        record.error = Some(error.clone());
        if let Err(persist_error) = self.write_record(&record) {
            return ToolResult::failure(format!(
                "{error}; persist Computer record: {persist_error}"
            ));
        }
        ToolResult::failure(error).with_details(record_details(&record))
    }

    async fn refresh_orb_record(
        &self,
        mut record: SubagentRecord,
        cancel: Option<&CancellationToken>,
    ) -> Result<SubagentRecord, String> {
        if record.backend != SubagentBackend::Orb {
            return Ok(record);
        }
        let Some(orb) = record.orb.clone() else {
            return Err(
                "Hosted Computer task has no durable owner binding; refusing status or collect until its launch is recovered"
                    .to_string(),
            );
        };
        if orb.thread_id.trim().is_empty() {
            return Err(
                "Hosted Computer task has no remote thread; refusing status or collect until its launch is recovered"
                    .to_string(),
            );
        }
        let token = cancel.cloned().unwrap_or_else(CancellationToken::new);
        let adapter = self.orb_adapter_for_record(&record).await?;
        let status = adapter
            .status(&orb.thread_id, &token)
            .await
            .map_err(|error| error.to_string())?;
        let adapter = self.orb_adapter_for_record(&record).await?;
        let report = adapter
            .collect(&orb.thread_id, &token)
            .await
            .map_err(|error| error.to_string())?;
        let lifecycle_state = status.lifecycle_state.clone();
        let mapped_status = match normalize_orb_lifecycle(&lifecycle_state) {
            DelegationLifecycleState::Queued => SubagentStatus::Queued,
            DelegationLifecycleState::Active | DelegationLifecycleState::Resumed => {
                SubagentStatus::Running
            }
            DelegationLifecycleState::Paused => SubagentStatus::Interrupted,
            DelegationLifecycleState::Cancelled => SubagentStatus::Cancelled,
            DelegationLifecycleState::Completed => SubagentStatus::Completed,
            DelegationLifecycleState::NeedsAttention
            | DelegationLifecycleState::ApprovalRequired
            | DelegationLifecycleState::Failed => SubagentStatus::Failed,
            DelegationLifecycleState::Unavailable => record.status,
        };
        record.status = mapped_status;
        if let Some(output) = report
            .latest_assistant_message()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            record.result = Some(SubagentResult {
                output: output.to_string(),
                files_modified: Vec::new(),
            });
        } else if record.status == SubagentStatus::Failed {
            if let Some(outcome) = status
                .controller
                .as_ref()
                .and_then(|controller| controller.outcome.as_ref())
            {
                record.result = Some(SubagentResult {
                    output: serde_json::to_string(outcome)
                        .unwrap_or_else(|_| "Computer task failed".to_string()),
                    files_modified: Vec::new(),
                });
            }
        }
        if let Some(orb) = record.orb.as_mut() {
            orb.lifecycle_state = Some(lifecycle_state.clone());
            orb.available_commands = status.available_commands.clone();
        }
        if record.status == SubagentStatus::Failed && record.error.is_none() {
            record.error = Some(format!("Computer task entered {lifecycle_state}"));
        }
        if record.status.is_terminal() {
            record.finished_at_ms.get_or_insert_with(now_millis);
        }
        self.write_record(&record)?;
        Ok(record)
    }

    async fn wait_orb_until_terminal(
        &self,
        id: &str,
        timeout_ms: u64,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        self.wait(
            &serde_json::json!({
                "subagent_id": id,
                "timeout_ms": timeout_ms.min(MAX_WAIT_MS),
            }),
            cancel,
        )
        .await
    }

    pub(crate) async fn list(&self) -> ToolResult {
        let mut records = Vec::new();
        let mut errors = Vec::new();
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::success("No subagents").with_details(serde_json::json!({
                    "snapshotId": snapshot_id,
                    "complete": true,
                    "count": 0,
                    "subagents": [],
                    "errors": []
                }));
            }
            Err(error) => return ToolResult::failure(format!("list subagents: {error}")),
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    errors.push(serde_json::json!({
                        "entry": null,
                        "error": error.to_string(),
                    }));
                    continue;
                }
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            match self.load_record(&id) {
                Ok(record) => records.push(record),
                Err(error) => errors.push(serde_json::json!({
                    "entry": id,
                    "error": error,
                })),
            }
        }
        if self.orb_adapter().await.is_some() {
            let token = CancellationToken::new();
            for record in &mut records {
                if record.backend == SubagentBackend::Orb {
                    match self.refresh_orb_record(record.clone(), Some(&token)).await {
                        Ok(refreshed) => *record = refreshed,
                        Err(error) => errors.push(serde_json::json!({
                            "entry": record.id,
                            "error": error,
                        })),
                    }
                }
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
        let lines = records
            .iter()
            .map(|record| {
                format!(
                    "{} {} [steer: {}] {}",
                    record.id,
                    status_label(record.status),
                    agent_ref(record),
                    record.task.replace(['\n', '\r'], " ")
                )
            })
            .collect::<Vec<_>>();
        let details = serde_json::json!({
            "snapshotId": snapshot_id,
            "complete": errors.is_empty(),
            "count": records.len(),
            "subagents": records.iter().map(record_details).collect::<Vec<_>>(),
            "errors": errors,
        });
        ToolResult::success(if lines.is_empty() {
            "No subagents".to_string()
        } else {
            lines.join("\n")
        })
        .with_details(details)
    }

    /// Runtime-held records use the same byte-preserving path encoding as persisted children.
    pub(crate) fn coding_validator_working_directory(record: &SubagentRecord) -> PathBuf {
        deserialize_repository_path(&record.cwd)
    }

    pub(crate) fn coding_validator_record(&self, id: &str) -> Result<SubagentRecord, String> {
        self.coding_validator_receipts
            .lock()
            .map_err(|_| "Coding validator receipt storage is unavailable".to_owned())?
            .get(id)
            .and_then(Clone::clone)
            .ok_or_else(|| "Coding validator has no terminal receipt from this runtime; wait or launch fresh validation".into())
    }

    /// Drive the real terminal producer without contacting a model in dispatcher tests.
    #[cfg(test)]
    pub(crate) fn finish_coding_validator_for_test(
        &self,
        record: SubagentRecord,
        output: String,
    ) -> Result<SubagentRecord, String> {
        self.coding_validator_receipts
            .lock()
            .unwrap()
            .insert(record.id.clone(), None);
        let vault = CredentialVault::new();
        let scope = ParentCredentialScope {
            vault: &vault,
            generation: vault.generation(),
        };
        self.finish_record(
            record,
            SubagentStatus::Completed,
            Some(SubagentResult {
                output,
                files_modified: vec![],
            }),
            None,
            &CredentialVault::new(),
            &scope,
        )
    }

    pub(crate) fn clear_coding_validator_records(&self) {
        self.coding_validator_receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn seal_coding_validator_record(&self, record: &SubagentRecord) {
        if let Some(receipt) = self
            .coding_validator_receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&record.id)
        {
            if receipt.is_none() {
                *receipt = Some(record.clone());
            }
        }
    }

    pub(crate) fn get(&self, args: &serde_json::Value) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        match self.load_record(id) {
            Ok(record) => tool_result_for_record(record),
            Err(error) => ToolResult::failure(error),
        }
    }

    pub(crate) async fn get_remote(
        &self,
        args: &serde_json::Value,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        let record = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        if record.backend != SubagentBackend::Orb {
            return tool_result_for_record(record);
        }
        match self.refresh_orb_record(record, cancel).await {
            Ok(record) => tool_result_for_record(record),
            Err(error) => ToolResult::failure(error),
        }
    }

    /// Run a native hosted Computer operation. This is the product-facing
    /// projection over the existing durable subagent record and the Computer
    /// runtime's `orb` compatibility adapter;
    /// it never creates a second launch path or a second ledger.
    pub(crate) async fn orb_console(
        &self,
        action: OrbConsoleAction,
        provider_error: Option<String>,
    ) -> ToolResult {
        if provider_error.is_some() {
            return self.orb_console_unavailable(action).await;
        }
        match action {
            OrbConsoleAction::List => self.orb_console_list().await,
            OrbConsoleAction::Status { id } | OrbConsoleAction::Collect { id } => {
                self.orb_console_status(&id).await
            }
            OrbConsoleAction::Followup { id, prompt } => {
                self.orb_console_followup(&id, &prompt).await
            }
            OrbConsoleAction::Pause { id } => self.orb_console_pause(&id).await,
            OrbConsoleAction::Resume { id } => self.orb_console_resume(&id).await,
            OrbConsoleAction::Cancel { id } => self.orb_console_cancel(&id).await,
            OrbConsoleAction::HandoffCreate {
                source_id,
                target_thread_id,
                files,
                artifact_ids,
                include_diff,
            } => {
                self.orb_console_handoff_create(
                    &source_id,
                    &target_thread_id,
                    &files,
                    &artifact_ids,
                    include_diff,
                )
                .await
            }
            OrbConsoleAction::HandoffList { target_thread_id } => {
                self.orb_console_handoff_list(&target_thread_id).await
            }
            OrbConsoleAction::HandoffRead {
                target_thread_id,
                package_id,
            } => {
                self.orb_console_handoff_read(&target_thread_id, &package_id)
                    .await
            }
        }
    }

    fn orb_records(&self) -> Result<Vec<SubagentRecord>, String> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("list hosted Computer tasks: {error}")),
        };
        let mut records = Vec::new();
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Ok(record) = self.load_record(&id) {
                if record.backend == SubagentBackend::Orb {
                    records.push(record);
                }
            }
        }
        records.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(records)
    }

    async fn orb_console_list(&self) -> ToolResult {
        let records = match self.orb_records() {
            Ok(records) => records,
            Err(error) => return ToolResult::failure(error),
        };
        let mut tasks = Vec::with_capacity(records.len());
        let mut errors = Vec::new();
        for record in records {
            match self.refresh_orb_record(record.clone(), None).await {
                Ok(record) => tasks.push(orb_console_task(&record, None)),
                Err(error) => {
                    let reason_code = if is_orb_owner_binding_error(&error) {
                        "owner_binding_mismatch"
                    } else {
                        "provider_unavailable"
                    };
                    tasks.push(orb_console_task(&record, Some(reason_code)));
                    errors.push(serde_json::json!({
                        "taskId": record.id,
                        "reasonCode": reason_code
                    }));
                }
            }
        }
        let lines = tasks
            .iter()
            .map(|task| {
                format!(
                    "{}  {}  {}",
                    task.id,
                    task.event.lifecycle_state.as_str(),
                    task.task.replace(['\n', '\r'], " ")
                )
            })
            .collect::<Vec<_>>();
        let details = serde_json::json!({
            "schemaVersion": maestro_runtime::DELEGATION_PROJECTION_SCHEMA_VERSION,
            "complete": errors.is_empty(),
            "count": tasks.len(),
            "tasks": tasks,
            "errors": errors,
        });
        ToolResult::success(if lines.is_empty() {
            "No hosted Computer tasks".to_string()
        } else {
            lines.join("\n")
        })
        .with_details(details)
    }

    async fn orb_console_status(&self, id: &str) -> ToolResult {
        let record = match self.load_record(id) {
            Ok(record) if record.backend == SubagentBackend::Orb => record,
            Ok(_) => {
                return ToolResult::failure(format!("subagent {id} is not a hosted Computer task"));
            }
            Err(error) => return ToolResult::failure(error),
        };
        match self.refresh_orb_record(record.clone(), None).await {
            Ok(record) => {
                let task = orb_console_task(&record, None);
                orb_console_task_result(task, format!("Hosted Computer task {id}"))
            }
            Err(error) if is_orb_owner_binding_error(&error) => {
                self.orb_owner_binding_failure(&record, error)
            }
            Err(_) => self.orb_unavailable_result(&record),
        }
    }

    async fn orb_console_followup(&self, id: &str, prompt: &str) -> ToolResult {
        let mut record = match self.load_record(id) {
            Ok(record) if record.backend == SubagentBackend::Orb => record,
            Ok(_) => {
                return ToolResult::failure(format!("subagent {id} is not a hosted Computer task"));
            }
            Err(error) => return ToolResult::failure(error),
        };
        if prompt.trim().is_empty() {
            return ToolResult::failure("followup requires a non-empty prompt");
        }
        if CredentialVault::has_references(prompt) {
            return ToolResult::failure(
                "Computer delegation cannot forward local credential references; provide a credential-free follow-up",
            );
        }
        let Some(orb) = record.orb.clone() else {
            return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
        };
        if orb.thread_id.trim().is_empty() {
            return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
        }
        let token = CancellationToken::new();
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) => return self.orb_operation_failure(&record, error),
        };
        let remote_status = match adapter.status(&orb.thread_id, &token).await {
            Ok(status) => status,
            Err(error) => return self.orb_operation_failure(&record, error.to_string()),
        };
        let remote_lifecycle = normalize_orb_lifecycle(&remote_status.lifecycle_state);
        if remote_lifecycle == DelegationLifecycleState::Unavailable {
            return self.orb_unavailable_result(&record);
        }
        let can_resume =
            orb_status_allows_resume(remote_lifecycle, &remote_status.available_commands);
        if !can_resume
            && matches!(
                remote_lifecycle,
                DelegationLifecycleState::Completed
                    | DelegationLifecycleState::Cancelled
                    | DelegationLifecycleState::Failed
            )
        {
            let task = orb_console_task(&record, None);
            return orb_console_task_failure(
                task,
                format!(
                    "Hosted Computer task {id} cannot receive a follow-up in its terminal state"
                ),
            );
        }
        let attempt = record.attempt.to_string();
        let key = deterministic_idempotency_key("followup", &[id, &attempt, prompt.trim()]);
        if can_resume {
            let adapter = match self.orb_adapter_for_record(&record).await {
                Ok(adapter) => adapter,
                Err(error) => return self.orb_operation_failure(&record, error),
            };
            if let Err(error) = adapter.resume(&orb.thread_id, &key, &token).await {
                return self.orb_operation_failure(&record, error.to_string());
            }
        }
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) => return self.orb_operation_failure(&record, error),
        };
        if let Err(error) = adapter
            .follow_up(&orb.thread_id, prompt.trim(), &key, &token)
            .await
        {
            return self.orb_operation_failure(&record, error.to_string());
        }
        record.attempt = record.attempt.saturating_add(1);
        record.current_prompt = prompt.trim().to_string();
        record.last_parent_scope_id = self.parent_scope_id();
        record.last_call_id = format!("orb-console:followup:{id}");
        record.status = SubagentStatus::Running;
        record.started_at_ms = Some(now_millis());
        record.finished_at_ms = None;
        record.result = None;
        record.error = None;
        record.snapshot_attempt = None;
        if let Some(orb) = record.orb.as_mut() {
            orb.lifecycle_state = Some("active".to_string());
            orb.available_commands = remote_status.available_commands;
        }
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        let task = orb_console_task(&record, None);
        orb_console_task_result(task, format!("Follow-up sent to hosted Computer task {id}"))
    }

    async fn orb_console_handoff_create(
        &self,
        source_id: &str,
        target_thread_id: &str,
        files: &[String],
        artifact_ids: &[String],
        include_diff: bool,
    ) -> ToolResult {
        let record = match self.load_record(source_id) {
            Ok(record) if record.backend == SubagentBackend::Orb => record,
            Ok(_) => {
                return ToolResult::failure(format!(
                    "subagent {source_id} is not a hosted Computer task"
                ));
            }
            Err(error) => return ToolResult::failure(error),
        };
        let Some(orb) = record.orb.as_ref() else {
            return ToolResult::failure(format!(
                "Computer subagent {source_id} has no remote thread"
            ));
        };
        if orb.thread_id.trim().is_empty() {
            return ToolResult::failure(format!(
                "Computer subagent {source_id} has no remote thread"
            ));
        }
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) => return self.orb_operation_failure(&record, error),
        };
        let cancel = CancellationToken::new();
        let package = match adapter
            .create_handoff_package(
                &orb.thread_id,
                target_thread_id,
                files,
                artifact_ids,
                include_diff,
                &cancel,
            )
            .await
        {
            Ok(package) => package,
            Err(crate::tools::orb_delegation::OrbDelegationError::InvalidHandoffSelection(
                error,
            )) => return ToolResult::failure(error),
            Err(error) => return self.orb_operation_failure(&record, error.to_string()),
        };
        ToolResult::success(handoff_package_summary(&package)).with_details(serde_json::json!({
            "schemaVersion": "evalops.maestro.orb-handoff.v1",
            "handoffPackage": package,
            "sourceTaskId": source_id,
            "sourceThreadId": orb.thread_id.clone(),
            "targetThreadId": target_thread_id,
        }))
    }

    async fn orb_console_handoff_list(&self, target_thread_id: &str) -> ToolResult {
        let Some(adapter) = self.orb_adapter().await else {
            return ToolResult::failure(
                "Hosted Computer is unavailable; handoff packages were not listed",
            )
            .with_details(serde_json::json!({
                "schemaVersion": "evalops.maestro.orb-handoff.v1",
                "reasonCode": "provider_unavailable",
            }));
        };
        let cancel = CancellationToken::new();
        let packages = match adapter
            .list_handoff_packages(target_thread_id, &cancel)
            .await
        {
            Ok(packages) => packages,
            Err(error) => {
                return ToolResult::failure(format!("Hosted Computer handoff failed: {error}"));
            }
        };
        ToolResult::success(handoff_package_list_summary(&packages)).with_details(
            serde_json::json!({
                "schemaVersion": "evalops.maestro.orb-handoff.v1",
                "targetThreadId": target_thread_id,
                "handoffPackages": packages,
            }),
        )
    }

    async fn orb_console_handoff_read(
        &self,
        target_thread_id: &str,
        package_id: &str,
    ) -> ToolResult {
        let Some(adapter) = self.orb_adapter().await else {
            return ToolResult::failure(
                "Hosted Computer is unavailable; the handoff package was not read",
            )
            .with_details(serde_json::json!({
                "schemaVersion": "evalops.maestro.orb-handoff.v1",
                "reasonCode": "provider_unavailable",
            }));
        };
        let cancel = CancellationToken::new();
        let package = match adapter
            .read_handoff_package(target_thread_id, package_id, &cancel)
            .await
        {
            Ok(package) => package,
            Err(error) => {
                return ToolResult::failure(format!("Hosted Computer handoff failed: {error}"));
            }
        };
        ToolResult::success(handoff_package_summary(&package)).with_details(serde_json::json!({
            "schemaVersion": "evalops.maestro.orb-handoff.v1",
            "targetThreadId": target_thread_id,
            "handoffPackage": package,
        }))
    }

    async fn orb_console_pause(&self, id: &str) -> ToolResult {
        self.orb_console_control(id, OrbConsoleControl::Pause).await
    }

    async fn orb_console_resume(&self, id: &str) -> ToolResult {
        let mut record = match self.load_record(id) {
            Ok(record) if record.backend == SubagentBackend::Orb => record,
            Ok(_) => {
                return ToolResult::failure(format!("subagent {id} is not a hosted Computer task"));
            }
            Err(error) => return ToolResult::failure(error),
        };
        let Some(orb) = record.orb.clone() else {
            return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
        };
        if orb.thread_id.trim().is_empty() {
            return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
        }
        let token = CancellationToken::new();
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) => return self.orb_operation_failure(&record, error),
        };
        let status = match adapter.status(&orb.thread_id, &token).await {
            Ok(status) => status,
            Err(error) => return self.orb_operation_failure(&record, error.to_string()),
        };
        let lifecycle = normalize_orb_lifecycle(&status.lifecycle_state);
        if lifecycle == DelegationLifecycleState::Unavailable {
            return self.orb_unavailable_result(&record);
        }
        if matches!(
            lifecycle,
            DelegationLifecycleState::Completed | DelegationLifecycleState::Cancelled
        ) {
            let task = orb_console_task(&record, None);
            return orb_console_task_failure(
                task,
                format!("Hosted Computer task {id} is terminal"),
            );
        }
        let key = deterministic_idempotency_key("resume", &[id]);
        if orb_status_allows_resume(lifecycle, &status.available_commands) {
            let adapter = match self.orb_adapter_for_record(&record).await {
                Ok(adapter) => adapter,
                Err(error) => return self.orb_operation_failure(&record, error),
            };
            let status = match adapter.resume(&orb.thread_id, &key, &token).await {
                Ok(status) => status,
                Err(error) => return self.orb_operation_failure(&record, error.to_string()),
            };
            if let Some(orb) = record.orb.as_mut() {
                orb.lifecycle_state = Some(status.lifecycle_state.clone());
                orb.available_commands = status.available_commands.clone();
            }
        } else if lifecycle == DelegationLifecycleState::Active {
            if let Some(orb) = record.orb.as_mut() {
                orb.lifecycle_state = Some(status.lifecycle_state.clone());
                orb.available_commands = status.available_commands.clone();
            }
        } else {
            let task = orb_console_task(&record, None);
            return orb_console_task_failure(
                task,
                format!(
                    "Hosted Computer task {id} cannot be resumed from {}",
                    lifecycle.as_str()
                ),
            );
        }
        record.status = SubagentStatus::Running;
        record.started_at_ms = Some(now_millis());
        record.finished_at_ms = None;
        record.error = None;
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        let task = orb_console_task(&record, None);
        orb_console_task_result(task, format!("Hosted Computer task {id} resumed"))
    }

    async fn orb_console_cancel(&self, id: &str) -> ToolResult {
        self.orb_console_control(id, OrbConsoleControl::Cancel)
            .await
    }

    async fn orb_console_control(&self, id: &str, control: OrbConsoleControl) -> ToolResult {
        let mut record = match self.load_record(id) {
            Ok(record) if record.backend == SubagentBackend::Orb => record,
            Ok(_) => {
                return ToolResult::failure(format!("subagent {id} is not a hosted Computer task"));
            }
            Err(error) => return ToolResult::failure(error),
        };
        let Some(orb) = record.orb.clone() else {
            return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
        };
        if orb.thread_id.trim().is_empty() {
            return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
        }
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) => return self.orb_operation_failure(&record, error),
        };
        let token = CancellationToken::new();
        let key_kind = control.idempotency_kind();
        let key = deterministic_idempotency_key(key_kind, &[id]);
        let status = match control {
            OrbConsoleControl::Pause => adapter.pause(&orb.thread_id, &key, &token).await,
            OrbConsoleControl::Cancel => adapter.cancel(&orb.thread_id, &key, &token).await,
        };
        let status = match status {
            Ok(status) => status,
            Err(error) => return self.orb_operation_failure(&record, error.to_string()),
        };
        if let Some(orb) = record.orb.as_mut() {
            orb.lifecycle_state = Some(status.lifecycle_state.clone());
            orb.available_commands = status.available_commands.clone();
        }
        match control {
            OrbConsoleControl::Pause => record.status = SubagentStatus::Interrupted,
            OrbConsoleControl::Cancel => {
                record.status = SubagentStatus::Cancelled;
                record.finished_at_ms = Some(now_millis());
            }
        }
        record.error = None;
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        let task = orb_console_task(&record, None);
        orb_console_task_result(
            task,
            format!("Hosted Computer task {id} {}", control.label()),
        )
    }

    async fn orb_console_unavailable(&self, action: OrbConsoleAction) -> ToolResult {
        match action {
            OrbConsoleAction::List => {
                let records = self.orb_records().unwrap_or_default();
                let tasks = records
                    .iter()
                    .map(|record| orb_console_task(record, Some("provider_unavailable")))
                    .collect::<Vec<_>>();
                let lines = tasks
                    .iter()
                    .map(|task| {
                        format!(
                            "{}  {}  {}",
                            task.id,
                            task.event.lifecycle_state.as_str(),
                            task.task.replace(['\n', '\r'], " ")
                        )
                    })
                    .collect::<Vec<_>>();
                ToolResult::success(if lines.is_empty() {
                    "Hosted Computer is unavailable; no durable Computer tasks found".to_string()
                } else {
                    lines.join("\n")
                })
                .with_details(serde_json::json!({
                    "schemaVersion": maestro_runtime::DELEGATION_PROJECTION_SCHEMA_VERSION,
                    "complete": false,
                    "count": tasks.len(),
                    "tasks": tasks,
                    "reasonCode": "provider_unavailable"
                }))
            }
            action => {
                let id = match action {
                    OrbConsoleAction::Status { id }
                    | OrbConsoleAction::Followup { id, .. }
                    | OrbConsoleAction::Pause { id }
                    | OrbConsoleAction::Resume { id }
                    | OrbConsoleAction::Cancel { id }
                    | OrbConsoleAction::Collect { id } => id,
                    OrbConsoleAction::HandoffCreate { source_id, .. } => source_id,
                    OrbConsoleAction::HandoffList { .. } | OrbConsoleAction::HandoffRead { .. } => {
                        return ToolResult::failure(
                            "Hosted Computer is unavailable; the handoff operation was not executed",
                        )
                        .with_details(serde_json::json!({
                            "schemaVersion": "evalops.maestro.orb-handoff.v1",
                            "reasonCode": "provider_unavailable",
                        }));
                    }
                    OrbConsoleAction::List => unreachable!(),
                };
                match self.load_record(&id) {
                    Ok(record) if record.backend == SubagentBackend::Orb => {
                        self.orb_unavailable_result(&record)
                    }
                    Ok(_) => {
                        ToolResult::failure(format!("subagent {id} is not a hosted Computer task"))
                    }
                    Err(error) => ToolResult::failure(error),
                }
            }
        }
    }

    fn orb_unavailable_result(&self, record: &SubagentRecord) -> ToolResult {
        let task = orb_console_task(record, Some("provider_unavailable"));
        orb_console_task_failure(
            task,
            "Hosted Computer is unavailable; durable task identity was retained".to_string(),
        )
    }

    fn orb_owner_binding_failure(
        &self,
        record: &SubagentRecord,
        error: impl Into<String>,
    ) -> ToolResult {
        let task = orb_console_task(record, Some("owner_binding_mismatch"));
        ToolResult::failure(error.into()).with_details(serde_json::json!({
            "schemaVersion": maestro_runtime::DELEGATION_PROJECTION_SCHEMA_VERSION,
            "task": task,
            "reasonCode": "owner_binding_mismatch",
        }))
    }

    fn orb_operation_failure(
        &self,
        record: &SubagentRecord,
        error: impl Into<String>,
    ) -> ToolResult {
        let error = error.into();
        if is_orb_owner_binding_error(&error) {
            self.orb_owner_binding_failure(record, error)
        } else {
            self.orb_unavailable_result(record)
        }
    }

    pub(crate) fn inspect(&self, args: &serde_json::Value) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        let record = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        let Some(serialized_path) = record.worktree_path.as_deref() else {
            return ToolResult::success(format!("Subagent {id} does not have a worktree"))
                .with_details(serde_json::json!({
                    "subagentId": record.id,
                    "worktreePath": null,
                    "worktreeCleaned": record.worktree_cleaned,
                }));
        };
        let path = deserialize_repository_path(serialized_path);
        let exists = path.exists();
        let status = if exists {
            git_worktree_command(&path, ["status", "--short"], None)
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_else(|error| format!("unable to inspect status: {error}"))
        } else {
            "worktree path does not exist".to_string()
        };
        let diff_stat = if exists {
            git_worktree_command(&path, ["diff", "--stat"], None)
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let details = serde_json::json!({
            "subagentId": record.id,
            "worktreePath": display_repository_path(&path),
            "worktreeCleaned": record.worktree_cleaned,
            "exists": exists,
            "status": status,
            "diffStat": diff_stat,
            "filesModified": record.result.as_ref().map(|result| &result.files_modified),
        });
        ToolResult::success(format!("Inspected worktree for subagent {id}")).with_details(details)
    }

    pub(crate) fn cleanup(&self, args: &serde_json::Value) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        // First load: learn where this child's lease lives, and let the
        // orphan reclassification in `load_record` run while no lease is held,
        // so a child left `Running` by a crashed process settles to
        // `Interrupted` before the check below rather than blocking cleanup.
        let record = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };

        // Take the execution lease before reading the status that decides
        // whether to delete, and hold it through the removal. A one-shot check
        // followed by an unguarded `git worktree remove --force` could pass
        // while another process was acquiring the lease in `resume`; cleanup
        // then deleted the worktree out from under a child that had just been
        // restarted. The lease is what `resume` and `run_child` hold for a
        // child's whole run, so taking it here makes the two mutually
        // exclusive: whoever gets it first wins, and the loser does nothing.
        let _lease = match Self::acquire_cleanup_lease(&record) {
            Ok(lease) => lease,
            Err(error) => return ToolResult::failure(error),
        };

        // Second load, now under the lease. A resume that beat us to the lease
        // has already rewritten this record, and one that arrives after cannot
        // start until we release it, so this status cannot change underneath
        // the removal below.
        let mut record = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        if !record.status.is_terminal() {
            return ToolResult::failure(format!(
                "subagent {id} must be terminal before its worktree can be cleaned up"
            ));
        }
        if record.worktree_cleaned {
            return ToolResult::success(format!("Worktree for subagent {id} is already cleaned"))
                .with_details(record_details(&record));
        }
        // Shared children never create a worktree. Leave `worktree_cleaned`
        // unset so a later `resume_subagent` is not rejected for a cleanup that
        // removed nothing.
        let Some(serialized_path) = record.worktree_path.as_deref() else {
            return ToolResult::success(format!("Subagent {id} did not have a worktree"))
                .with_details(record_details(&record));
        };
        let path = deserialize_repository_path(serialized_path);
        if path == self.cwd {
            return ToolResult::failure(
                "refusing to clean the parent workspace as a subagent worktree",
            );
        }
        if path.exists() {
            if !git_worktree_is_registered(&self.cwd, &path) {
                return ToolResult::failure(format!(
                    "refusing to remove unregistered worktree path {}",
                    path.display()
                ));
            }
            let output = match git_worktree_command(
                &self.cwd,
                ["worktree", "remove", "--force"],
                Some(&path),
            ) {
                Ok(output) => output,
                Err(error) => return ToolResult::failure(error),
            };
            if !output.status.success() {
                return ToolResult::failure(format!(
                    "remove subagent worktree {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            let _ = git_worktree_command(&self.cwd, ["worktree", "prune"], None);
        }
        record.worktree_cleaned = true;
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        ToolResult::success(format!("Cleaned worktree for subagent {id}"))
            .with_details(record_details(&record))
    }

    pub(crate) async fn wait(
        &self,
        args: &serde_json::Value,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        let timeout_ms = match parse_wait_timeout(args) {
            Ok(timeout_ms) => timeout_ms,
            Err(error) => return ToolResult::failure(error),
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let steer_signal = self.steer_signal();

        loop {
            let record = match self.load_record(id) {
                Ok(record) => record,
                Err(error) => return ToolResult::failure(error),
            };
            if record.status.is_terminal() || timeout_ms == 0 || Instant::now() >= deadline {
                return tool_result_for_record(record);
            }
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return cancelled_result("wait_subagent cancelled");
            }
            let (record, slice) = if record.backend == SubagentBackend::Orb {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let refresh_cancel =
                    cancel.map_or_else(CancellationToken::new, CancellationToken::child_token);
                let refresh = self.refresh_orb_record(record.clone(), Some(&refresh_cancel));
                let record = match tokio::time::timeout(remaining, refresh).await {
                    Ok(Ok(record)) => record,
                    Ok(Err(error)) => return ToolResult::failure(error),
                    Err(_) => {
                        refresh_cancel.cancel();
                        return tool_result_for_record(record);
                    }
                };
                (record, Duration::from_millis(250))
            } else {
                (record, Duration::from_millis(50))
            };
            if record.status.is_terminal() || Instant::now() >= deadline {
                return tool_result_for_record(record);
            }
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return cancelled_result("wait_subagent cancelled");
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            match sleep_or_release(remaining.min(slice), cancel, steer_signal.as_deref()).await {
                WaitWake::Elapsed => {}
                WaitWake::Cancelled => return cancelled_result("wait_subagent cancelled"),
                WaitWake::Steered => return steer_released_result(&record),
            }
        }
    }

    pub(crate) async fn resume(
        &self,
        args: &serde_json::Value,
        parent_call_id: &str,
        sandbox_policy: Option<SandboxPolicy>,
        credential_vault: CredentialVault,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        let prompt = args
            .get("task")
            .or_else(|| args.get("follow_up"))
            .or_else(|| args.get("followUp"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_string);
        let Some(prompt) = prompt else {
            return ToolResult::failure("task or follow_up is required to resume a subagent");
        };
        if prompt.len() > MAX_TASK_BYTES {
            return ToolResult::failure(format!(
                "subagent follow-up exceeds the {} byte limit",
                MAX_TASK_BYTES
            ));
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return cancelled_result("resume_subagent cancelled before launch");
        }
        if self.runtime.get(id).is_some() {
            return ToolResult::failure(format!("subagent {id} is still running"));
        }

        let initial = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        if initial.backend == SubagentBackend::Orb {
            return self
                .resume_orb(initial, prompt, args, parent_call_id, cancel)
                .await;
        }
        let child_credential_vault = self
            .runtime
            .credential_scope(id)
            .unwrap_or_else(|| credential_vault.fork());
        let mut request = SpawnRequest {
            task: prompt,
            role: initial.role,
            backend: initial.backend,
            orb: initial
                .orb
                .as_ref()
                .map(|orb| orb.config.clone())
                .unwrap_or_default(),
            profile: initial.profile.clone(),
            profile_prompt: initial.profile_prompt.clone(),
            profile_tools: initial.profile_tools.clone(),
            model: initial.model.clone(),
            thinking: initial.thinking,
            difficulty: initial.difficulty,
            timeout_ms: initial.timeout_ms,
            max_tokens: initial.max_tokens,
            run_in_background: args
                .get("run_in_background")
                .or_else(|| args.get("runInBackground"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            isolation: initial.isolation,
            worktree_name: None,
        };
        if let Err(error) =
            apply_subagent_start_hook(&mut request, &self.cwd, &self.parent_scope_id())
        {
            return ToolResult::failure(error);
        }
        // The retained child scope may predate credentials imported into the
        // parent after the previous attempt. Resolve through the current
        // parent scope, then re-vault into the child scope before persisting
        // or launching the follow-up.
        let parent_resolved_task = credential_vault.resolve_all(&request.task);
        request.task = child_credential_vault.vault_in_text(&parent_resolved_task);
        let prompt = request.task;
        let run_in_background = request.run_in_background;
        let session_dir = Self::session_dir(&initial);
        let lease = match SessionLock::acquire(&Self::timeline_path(&initial)) {
            Ok(lease) => lease,
            Err(error) => {
                return ToolResult::failure(format!(
                    "acquire subagent {id} execution lease: {error}"
                ));
            }
        };

        // Reload under the lease. Everything above was decided from a record
        // read before the lease existed: another process can have completed a
        // full resume attempt in that window, and `cleanup` can have removed
        // the worktree. Using the pre-lease copy would rewrite the attempt
        // counter and transcript sequence backward, or relaunch into a
        // deleted directory.
        let mut record = match Self::revalidate_resumable_under_lease(self, id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        let previous_prompt = record.current_prompt.clone();
        let next_attempt = record.attempt.saturating_add(1);
        record.role = request.role;
        record.profile = request.profile.clone();
        record.profile_prompt = request.profile_prompt.clone();
        record.profile_tools = request.profile_tools.clone();
        record.model = request.model.clone();
        record.thinking = request.thinking;
        record.difficulty = request.difficulty;
        record.timeout_ms = request.timeout_ms;
        record.max_tokens = request.max_tokens;

        let mut recorder = match SessionRecorder::resume(&session_dir, id) {
            Ok(recorder) => recorder,
            Err(error) => {
                return ToolResult::failure(format!("resume subagent transcript: {error}"));
            }
        };
        let snapshot_attempt = recorder
            .semantic_conversation_attempt()
            .or(record.snapshot_attempt);
        let history = restore_history_with_prompt(
            recorder.replay().semantic_conversation,
            &previous_prompt,
            record.attempt,
            snapshot_attempt,
        );
        if let Err(error) = recorder.record_sent(&ToAgentMessage::Prompt {
            content: prompt.clone(),
            attachments: None,
            managed_inference_authorization: None,
        }) {
            return ToolResult::failure(format!("record subagent follow-up: {error}"));
        }
        if let Err(error) = recorder.flush_checkpoint() {
            return ToolResult::failure(format!("flush subagent follow-up: {error}"));
        }
        drop(recorder);

        record.status = SubagentStatus::Queued;
        record.current_prompt = prompt.clone();
        record.attempt = next_attempt;
        record.last_parent_scope_id = self.parent_scope_id();
        record.last_call_id = parent_call_id.to_string();
        record.started_at_ms = None;
        record.finished_at_ms = None;
        record.result = None;
        record.error = None;
        record.lifecycle_notification_published = false;
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }

        let token = CancellationToken::new();
        let (control_tx, control_rx) = mpsc::channel(RUNTIME_CONTROL_CAPACITY);
        self.runtime.insert(id, token.clone(), control_tx);
        let manager = self.clone();
        let launch_record = record.clone();
        let launch_id = id.to_string();
        let launch_error_id = id.to_string();
        let launch_policy = sandbox_policy;
        let launch_token = token.clone();
        let parent_cancel = if run_in_background {
            None
        } else {
            cancel.cloned()
        };
        self.runtime
            .set_credential_scope(id, child_credential_vault.clone());
        let parent_credential_generation = credential_vault.generation();
        let launch = ChildLaunch {
            lease: Some(lease),
            credential_vault: child_credential_vault,
            parent_credential_vault: credential_vault,
            parent_credential_generation,
            parent_cancel,
        };
        let launch = async move {
            let cancellation_link = launch.parent_cancel.clone().map(|parent| {
                let child_token = launch_token.clone();
                tokio::spawn(async move {
                    parent.cancelled().await;
                    child_token.cancel();
                })
            });
            let result = manager
                .run_child(
                    launch_record,
                    ChildRun {
                        prompt,
                        history,
                        sandbox_policy: launch_policy,
                        token: launch_token,
                        control_rx,
                    },
                    launch,
                )
                .await;
            if let Some(cancellation_link) = cancellation_link {
                cancellation_link.abort();
            }
            manager.runtime.remove(&launch_id);
            result
        };

        if run_in_background {
            let launch_error_id = launch_error_id.clone();
            tokio::spawn(async move {
                if let Err(error) = launch.await {
                    eprintln!("subagent {launch_error_id} failed: {error}");
                }
            });
            return ToolResult::success(format!("Resumed subagent {id} in the background"))
                .with_details(record_details(&record));
        }

        match launch.await {
            Ok(record) => tool_result_for_record(record),
            Err(error) => ToolResult::failure(error),
        }
    }

    async fn resume_orb(
        &self,
        mut record: SubagentRecord,
        prompt: String,
        args: &serde_json::Value,
        parent_call_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        if CredentialVault::has_references(&prompt) {
            return ToolResult::failure(
                "Computer delegation cannot forward local credential references; provide a credential-free follow-up",
            );
        }
        let Some(orb) = record.orb.clone() else {
            return ToolResult::failure(format!(
                "Computer subagent {} has no remote thread",
                record.id
            ));
        };
        if orb.thread_id.trim().is_empty() {
            return ToolResult::failure(format!(
                "Computer subagent {} has no remote thread",
                record.id
            ));
        }
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) if is_orb_owner_binding_error(&error) => {
                return self.orb_owner_binding_failure(&record, error);
            }
            Err(error) => return ToolResult::failure(error).with_details(record_details(&record)),
        };
        let token = cancel.cloned().unwrap_or_else(CancellationToken::new);
        if token.is_cancelled() {
            return cancelled_result("resume_subagent cancelled before Computer follow-up");
        }
        let remote_status = match adapter.status(&orb.thread_id, &token).await {
            Ok(status) => status,
            Err(error) => {
                let error = error.to_string();
                if is_orb_owner_binding_error(&error) {
                    return self.orb_owner_binding_failure(&record, error);
                }
                return ToolResult::failure(error).with_details(record_details(&record));
            }
        };
        let remote_lifecycle = normalize_orb_lifecycle(&remote_status.lifecycle_state);
        let can_resume =
            orb_status_allows_resume(remote_lifecycle, &remote_status.available_commands);
        let key = args
            .get("idempotency_key")
            .or_else(|| args.get("idempotencyKey"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                deterministic_idempotency_key("resume", &[record.id.as_str(), parent_call_id])
            });
        if can_resume {
            let adapter = match self.orb_adapter_for_record(&record).await {
                Ok(adapter) => adapter,
                Err(error) if is_orb_owner_binding_error(&error) => {
                    return self.orb_owner_binding_failure(&record, error);
                }
                Err(error) => {
                    return ToolResult::failure(error).with_details(record_details(&record));
                }
            };
            if let Err(error) = adapter.resume(&orb.thread_id, &key, &token).await {
                let error = error.to_string();
                if is_orb_owner_binding_error(&error) {
                    return self.orb_owner_binding_failure(&record, error);
                }
                return ToolResult::failure(error).with_details(record_details(&record));
            }
        } else if matches!(
            remote_lifecycle,
            DelegationLifecycleState::Completed
                | DelegationLifecycleState::Cancelled
                | DelegationLifecycleState::Failed
        ) {
            return ToolResult::failure(format!(
                "Computer task {} is {}; it cannot be resumed",
                record.id, remote_status.lifecycle_state
            ))
            .with_details(record_details(&record));
        }
        let adapter = match self.orb_adapter_for_record(&record).await {
            Ok(adapter) => adapter,
            Err(error) if is_orb_owner_binding_error(&error) => {
                return self.orb_owner_binding_failure(&record, error);
            }
            Err(error) => return ToolResult::failure(error).with_details(record_details(&record)),
        };
        if let Err(error) = adapter
            .follow_up(&orb.thread_id, &prompt, &key, &token)
            .await
        {
            let error = error.to_string();
            if is_orb_owner_binding_error(&error) {
                return self.orb_owner_binding_failure(&record, error);
            }
            return ToolResult::failure(error).with_details(record_details(&record));
        }
        record.attempt = record.attempt.saturating_add(1);
        record.current_prompt = prompt;
        record.last_parent_scope_id = self.parent_scope_id();
        record.last_call_id = parent_call_id.to_string();
        record.status = SubagentStatus::Running;
        record.started_at_ms = Some(now_millis());
        record.finished_at_ms = None;
        record.result = None;
        record.error = None;
        record.snapshot_attempt = None;
        if let Some(orb) = record.orb.as_mut() {
            orb.lifecycle_state = Some("active".to_string());
        }
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        let run_in_background = args
            .get("run_in_background")
            .or_else(|| args.get("runInBackground"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        if run_in_background {
            ToolResult::success(format!(
                "Sent follow-up to hosted Computer subagent {}",
                record.id
            ))
            .with_details(record_details(&record))
        } else {
            self.wait_orb_until_terminal(&record.id, record.timeout_ms, cancel)
                .await
        }
    }

    pub(crate) async fn cancel(
        &self,
        args: &serde_json::Value,
        cancel: Option<&CancellationToken>,
    ) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        let record = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        if record.backend == SubagentBackend::Orb {
            let Some(orb) = record.orb.clone() else {
                return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
            };
            if orb.thread_id.trim().is_empty() {
                return ToolResult::failure(format!("Computer subagent {id} has no remote thread"));
            }
            let adapter = match self.orb_adapter_for_record(&record).await {
                Ok(adapter) => adapter,
                Err(error) if is_orb_owner_binding_error(&error) => {
                    return self.orb_owner_binding_failure(&record, error);
                }
                Err(error) => return ToolResult::failure(error),
            };
            let token = cancel.cloned().unwrap_or_else(CancellationToken::new);
            let key = deterministic_idempotency_key("cancel", &[id]);
            match adapter.cancel(&orb.thread_id, &key, &token).await {
                Ok(status) => {
                    let mut updated = record;
                    updated.status = SubagentStatus::Cancelled;
                    updated.finished_at_ms = Some(now_millis());
                    updated.error = Some("Computer task cancellation requested".to_string());
                    if let Some(orb) = updated.orb.as_mut() {
                        orb.lifecycle_state = Some(status.lifecycle_state);
                    }
                    if let Err(error) = self.write_record(&updated) {
                        return ToolResult::failure(error);
                    }
                    return tool_result_for_record(updated);
                }
                Err(error) => {
                    let error = error.to_string();
                    if is_orb_owner_binding_error(&error) {
                        return self.orb_owner_binding_failure(&record, error);
                    }
                    return ToolResult::failure(error);
                }
            }
        }
        self.cancel_native(args)
    }

    pub(crate) fn cancel_native(&self, args: &serde_json::Value) -> ToolResult {
        let Some(id) = subagent_id(args) else {
            return ToolResult::failure("subagent_id is required");
        };
        let record = match self.load_record(id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        if record.backend == SubagentBackend::Orb {
            return ToolResult::failure(
                "Computer subagents require the asynchronous cancel_subagent tool",
            );
        }
        if record.status.is_terminal() {
            return tool_result_for_record(record);
        }
        let Some(token) = self.runtime.get(id) else {
            return ToolResult::failure(format!(
                "subagent {id} is not running in this Deixic Code process; resume it to restart"
            ));
        };
        token.cancel();
        ToolResult::success(format!("Cancellation requested for subagent {id}"))
            .with_details(record_details(&record))
    }

    pub(crate) async fn control(
        &self,
        args: &serde_json::Value,
        call_id: &str,
        parent_credential_vault: CredentialVault,
    ) -> ToolResult {
        let Some(reference) = args
            .get("agent_ref")
            .or_else(|| args.get("agentRef"))
            .and_then(serde_json::Value::as_str)
        else {
            return ToolResult::failure("agent_ref is required");
        };
        let (id, attempt) = match parse_agent_ref(reference) {
            Ok(parsed) => parsed,
            Err(error) => return ToolResult::failure(error),
        };
        let record = match self.load_record(&id) {
            Ok(record) => record,
            Err(error) => return ToolResult::failure(error),
        };
        if record.attempt != attempt {
            return ToolResult::failure(format!(
                "stale agent_ref: subagent {} is now on attempt {}",
                record.id, record.attempt
            ));
        }
        let mode = match ControlMode::parse(args.get("mode").and_then(serde_json::Value::as_str)) {
            Ok(mode) => mode,
            Err(error) => return ToolResult::failure(error),
        };
        if record.backend == SubagentBackend::Orb {
            return self
                .control_orb(record, mode, args, call_id, parent_credential_vault)
                .await;
        }
        if mode == ControlMode::Collect {
            return tool_result_for_record(record);
        }
        if record.status.is_terminal() {
            return ToolResult::failure(format!(
                "subagent {} is {}; use resume_subagent for a new attempt",
                record.id,
                status_label(record.status)
            ));
        }
        let body = args
            .get("message")
            .or_else(|| args.get("task"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| mode.label());
        if matches!(mode, ControlMode::Steer | ControlMode::Followup) && body == mode.label() {
            return ToolResult::failure("message is required for steer and followup controls");
        }
        if body.len() > MAX_TASK_BYTES {
            return ToolResult::failure(format!(
                "subagent control exceeds the {} byte limit",
                MAX_TASK_BYTES
            ));
        }

        let body = match prepare_control_body(
            &parent_credential_vault,
            self.runtime.credential_scope(&record.id).as_ref(),
            body,
        ) {
            Ok(body) => body,
            Err(error) => return ToolResult::failure(error),
        };

        let sender = self.parent_scope_id();
        let same_scope = sender == record.last_parent_scope_id;
        let delivery_state = if same_scope {
            crate::mailbox::MailboxDeliveryState::Queued
        } else {
            // The sender cannot decide the receiver's policy. Cross-session
            // controls stay held until the receiving process evaluates its
            // own configuration or a user explicitly approves the message.
            crate::mailbox::MailboxDeliveryState::Held
        };
        let recipient = agent_ref(&record);
        let idempotency_key = args
            .get("idempotency_key")
            .or_else(|| args.get("idempotencyKey"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("control:{call_id}:{recipient}:{}", mode.label()));
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&self.mailbox_path);
        let mailbox_id = match mailbox.send_typed(
            sender,
            recipient.clone(),
            body.clone(),
            crate::mailbox::MailboxPayload::SubagentControl { mode },
            delivery_state,
            Some(idempotency_key),
        ) {
            Ok(id) => id,
            Err(error) => return ToolResult::failure(format!("queue subagent control: {error}")),
        };

        if delivery_state == crate::mailbox::MailboxDeliveryState::Queued {
            self.runtime.send_control(
                &record.id,
                RuntimeControlRequest {
                    mailbox_id: mailbox_id.clone(),
                    recipient: recipient.clone(),
                    mode,
                    body,
                },
            );
        }
        let details = serde_json::json!({
            "messageId": mailbox_id,
            "agentRef": recipient,
            "mode": mode.label(),
            "deliveryState": delivery_state,
            "sameScope": same_scope,
        });
        match delivery_state {
            crate::mailbox::MailboxDeliveryState::Held => ToolResult::success(format!(
                "Control queued for recipient policy evaluation for subagent {}",
                record.id
            ))
            .with_details(details),
            _ => ToolResult::success(format!(
                "{} control queued for subagent {}",
                mode.label(),
                record.id
            ))
            .with_details(details),
        }
    }

    async fn control_orb(
        &self,
        mut record: SubagentRecord,
        mode: ControlMode,
        args: &serde_json::Value,
        call_id: &str,
        _parent_credential_vault: CredentialVault,
    ) -> ToolResult {
        let Some(orb) = record.orb.clone() else {
            return ToolResult::failure(format!(
                "Computer subagent {} has no remote thread",
                record.id
            ));
        };
        if orb.thread_id.trim().is_empty() {
            return ToolResult::failure(format!(
                "Computer subagent {} has no remote thread",
                record.id
            ));
        }
        if mode == ControlMode::Collect {
            return match self.refresh_orb_record(record, None).await {
                Ok(record) => tool_result_for_record(record),
                Err(error) => ToolResult::failure(error),
            };
        }
        let body = args
            .get("message")
            .or_else(|| args.get("task"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| mode.label());
        if matches!(mode, ControlMode::Steer | ControlMode::Followup) && body == mode.label() {
            return ToolResult::failure("message is required for steer and followup controls");
        }
        if body.len() > MAX_TASK_BYTES {
            return ToolResult::failure(format!(
                "subagent control exceeds the {} byte limit",
                MAX_TASK_BYTES
            ));
        }
        if CredentialVault::has_references(body) {
            return ToolResult::failure(
                "Computer delegation cannot forward local credential references; provide a credential-free control message",
            );
        }
        let token = CancellationToken::new();
        let key = args
            .get("idempotency_key")
            .or_else(|| args.get("idempotencyKey"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                deterministic_idempotency_key(
                    "control",
                    &[record.id.as_str(), call_id, mode.label()],
                )
            });
        let remote_status = match mode {
            ControlMode::Steer => {
                let adapter = match self.orb_adapter_for_record(&record).await {
                    Ok(adapter) => adapter,
                    Err(error) if is_orb_owner_binding_error(&error) => {
                        return self.orb_owner_binding_failure(&record, error);
                    }
                    Err(error) => return ToolResult::failure(error),
                };
                match adapter
                    .direct_task(&orb.thread_id, &key, body, &token)
                    .await
                {
                    Ok(status) => Some(status),
                    Err(error) => {
                        let error = error.to_string();
                        if is_orb_owner_binding_error(&error) {
                            return self.orb_owner_binding_failure(&record, error);
                        }
                        return ToolResult::failure(error);
                    }
                }
            }
            ControlMode::Followup => {
                let adapter = match self.orb_adapter_for_record(&record).await {
                    Ok(adapter) => adapter,
                    Err(error) if is_orb_owner_binding_error(&error) => {
                        return self.orb_owner_binding_failure(&record, error);
                    }
                    Err(error) => return ToolResult::failure(error),
                };
                if let Err(error) = adapter.follow_up(&orb.thread_id, body, &key, &token).await {
                    let error = error.to_string();
                    if is_orb_owner_binding_error(&error) {
                        return self.orb_owner_binding_failure(&record, error);
                    }
                    return ToolResult::failure(error);
                }
                None
            }
            ControlMode::Interrupt => {
                let adapter = match self.orb_adapter_for_record(&record).await {
                    Ok(adapter) => adapter,
                    Err(error) if is_orb_owner_binding_error(&error) => {
                        return self.orb_owner_binding_failure(&record, error);
                    }
                    Err(error) => return ToolResult::failure(error),
                };
                match adapter.pause(&orb.thread_id, &key, &token).await {
                    Ok(status) => Some(status),
                    Err(error) => {
                        let error = error.to_string();
                        if is_orb_owner_binding_error(&error) {
                            return self.orb_owner_binding_failure(&record, error);
                        }
                        return ToolResult::failure(error);
                    }
                }
            }
            ControlMode::Cancel => {
                let adapter = match self.orb_adapter_for_record(&record).await {
                    Ok(adapter) => adapter,
                    Err(error) if is_orb_owner_binding_error(&error) => {
                        return self.orb_owner_binding_failure(&record, error);
                    }
                    Err(error) => return ToolResult::failure(error),
                };
                match adapter.cancel(&orb.thread_id, &key, &token).await {
                    Ok(status) => Some(status),
                    Err(error) => {
                        let error = error.to_string();
                        if is_orb_owner_binding_error(&error) {
                            return self.orb_owner_binding_failure(&record, error);
                        }
                        return ToolResult::failure(error);
                    }
                }
            }
            ControlMode::Collect => unreachable!("collect is handled above"),
        };
        record.last_parent_scope_id = self.parent_scope_id();
        record.last_call_id = call_id.to_string();
        match mode {
            ControlMode::Steer | ControlMode::Followup => {
                record.current_prompt = body.to_string();
                record.status = SubagentStatus::Running;
            }
            ControlMode::Interrupt => {
                record.status = SubagentStatus::Interrupted;
            }
            ControlMode::Cancel => {
                record.status = SubagentStatus::Cancelled;
                record.finished_at_ms = Some(now_millis());
            }
            ControlMode::Collect => unreachable!("collect is handled above"),
        }
        if let Some(status) = remote_status {
            if let Some(orb) = record.orb.as_mut() {
                orb.lifecycle_state = Some(status.lifecycle_state);
            }
        }
        if let Err(error) = self.write_record(&record) {
            return ToolResult::failure(error);
        }
        ToolResult::success(format!(
            "{} control applied to hosted Computer subagent {}",
            mode.label(),
            record.id
        ))
        .with_details(serde_json::json!({
            "mode": mode.label(),
            "deliveryState": "remote_applied",
            "idempotencyKey": key,
            "record": record_details(&record),
        }))
    }

    pub(crate) fn inspect_control(
        &self,
        message_id: &str,
    ) -> Result<crate::mailbox::MailboxMessage, String> {
        let loaded = crate::mailbox::MailboxStore::load_from_path(&self.mailbox_path)
            .map_err(|error| format!("load mailbox for inspection: {error:#}"))?;
        let message = loaded
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .cloned()
            .ok_or_else(|| format!("mailbox message '{message_id}' was not found"))?;
        if !matches!(
            &message.payload,
            crate::mailbox::MailboxPayload::SubagentControl { .. }
        ) {
            return Err(format!(
                "mailbox message '{message_id}' is not a subagent control"
            ));
        }
        let (id, attempt) = parse_agent_ref(&message.recipient)?;
        let record = self.load_record(&id)?;
        if record.attempt != attempt || record.status.is_terminal() {
            return Err(if record.attempt != attempt {
                format!(
                    "control expired: subagent {id} is now on attempt {}",
                    record.attempt
                )
            } else {
                format!(
                    "control expired: subagent {id} is {}",
                    status_label(record.status)
                )
            });
        }
        Ok(message)
    }

    pub(crate) fn approve_held_control(
        &self,
        message_id: &str,
    ) -> Result<crate::mailbox::MailboxMessage, String> {
        let loaded = crate::mailbox::MailboxStore::load_from_path(&self.mailbox_path)
            .map_err(|error| format!("load mailbox for approval: {error:#}"))?;
        let message = loaded
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .cloned()
            .ok_or_else(|| format!("mailbox message '{message_id}' was not found"))?;
        if message.delivery_state != crate::mailbox::MailboxDeliveryState::Held {
            return Err(format!("mailbox message '{message_id}' is not held"));
        }
        if !matches!(
            &message.payload,
            crate::mailbox::MailboxPayload::SubagentControl { .. }
        ) {
            return Err(format!(
                "mailbox message '{message_id}' is not a subagent control"
            ));
        }
        let (id, attempt) = parse_agent_ref(&message.recipient)?;
        let record = self.load_record(&id)?;
        if record.attempt != attempt || record.status.is_terminal() {
            let reason = if record.attempt != attempt {
                format!(
                    "control expired: subagent {id} is now on attempt {}",
                    record.attempt
                )
            } else {
                format!(
                    "control expired: subagent {id} is {}",
                    status_label(record.status)
                )
            };
            let mut mailbox = crate::mailbox::MailboxStore::with_path(&self.mailbox_path);
            let _ = mailbox.deny_held(message_id, reason.clone());
            return Err(reason);
        }

        let mut mailbox = crate::mailbox::MailboxStore::with_path(&self.mailbox_path);
        let approved = mailbox
            .approve_held(message_id)
            .map_err(|error| format!("approve mailbox control: {error:#}"))?;
        let latest = self.load_record(&id)?;
        if latest.attempt != attempt || latest.status.is_terminal() {
            let reason = if latest.attempt != attempt {
                format!(
                    "control expired during approval: subagent {id} is now on attempt {}",
                    latest.attempt
                )
            } else {
                format!(
                    "control expired during approval: subagent {id} is {}",
                    status_label(latest.status)
                )
            };
            let _ = mailbox.deny_pending(message_id, reason.clone());
            return Err(reason);
        }
        Ok(approved)
    }

    async fn claim_durable_control(
        &self,
        record: &SubagentRecord,
    ) -> Option<RuntimeControlRequest> {
        let manager = self.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || manager.claim_durable_control_blocking(&record))
            .await
            .ok()
            .flatten()
    }

    fn claim_durable_control_blocking(
        &self,
        record: &SubagentRecord,
    ) -> Option<RuntimeControlRequest> {
        let recipient = agent_ref(record);
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&self.mailbox_path);
        let policy = crate::config::load_config(&self.cwd, None)
            .subagent_inbound_control
            .unwrap_or_default();
        let _ = mailbox.resolve_held_controls(
            &recipient,
            &record.last_parent_scope_id,
            policy == InboundControlPolicy::Allow,
            policy == InboundControlPolicy::Deny,
        );
        let message = mailbox
            .claim_typed(&recipient, |payload| {
                matches!(
                    payload,
                    crate::mailbox::MailboxPayload::SubagentControl { .. }
                )
            })
            .ok()??;
        let crate::mailbox::MailboxPayload::SubagentControl { mode, .. } = &message.payload else {
            return None;
        };
        Some(RuntimeControlRequest {
            mailbox_id: message.id,
            recipient,
            mode: *mode,
            body: message.body,
        })
    }

    async fn apply_child_control(
        &self,
        agent: &NativeAgent,
        recorder: &mut SessionRecorder,
        record: &SubagentRecord,
        credential_vault: &CredentialVault,
        request: RuntimeControlRequest,
        already_delivered: bool,
    ) -> ChildControlOutcome {
        if !already_delivered {
            let mailbox_path = self.mailbox_path.clone();
            let mailbox_id = request.mailbox_id.clone();
            let recipient = request.recipient.clone();
            let delivered = tokio::task::spawn_blocking(move || {
                crate::mailbox::MailboxStore::with_path(mailbox_path)
                    .mark_delivered(&mailbox_id, &recipient)
            })
            .await
            .is_ok_and(|result| result.is_ok());
            if !delivered {
                return ChildControlOutcome::Continue;
            }
        }
        let existing_receipt = self.control_receipt(record, &request.mailbox_id);
        if existing_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.state == DurableControlReceiptState::Applied)
        {
            let mailbox_path = self.mailbox_path.clone();
            let mailbox_id = request.mailbox_id;
            let recipient = request.recipient;
            let _ = tokio::task::spawn_blocking(move || {
                crate::mailbox::MailboxStore::with_path(mailbox_path).complete_delivery(
                    &mailbox_id,
                    &recipient,
                    None,
                )
            })
            .await;
            return ChildControlOutcome::Continue;
        }

        let is_new_receipt = existing_receipt.is_none();
        let acceptance_sequence = if is_new_receipt {
            self.next_control_receipt_sequence(record)
        } else {
            0
        };
        let mut receipt = existing_receipt.unwrap_or_else(|| DurableControlReceipt {
            mailbox_message_id: request.mailbox_id.clone(),
            queue_id: control_queue_id(&request.mailbox_id),
            mode: request.mode,
            body: request.body.clone(),
            attempt: record.attempt,
            accepted_at_ms: now_millis(),
            acceptance_sequence,
            state: DurableControlReceiptState::Accepted,
        });
        if receipt.queue_id == 0 {
            receipt.queue_id = control_queue_id(&receipt.mailbox_message_id);
        }
        if receipt.state == DurableControlReceiptState::Accepted
            && self.write_control_receipt(record, &receipt).is_err()
        {
            return ChildControlOutcome::Continue;
        }
        if is_new_receipt {
            let timeline_message = match request.mode {
                ControlMode::Steer => Some(ToAgentMessage::Steer {
                    content: request.body.clone(),
                    attachments: None,
                    managed_inference_authorization: None,
                }),
                ControlMode::Followup => Some(ToAgentMessage::Prompt {
                    content: request.body.clone(),
                    attachments: None,
                    managed_inference_authorization: None,
                }),
                ControlMode::Interrupt | ControlMode::Cancel => Some(ToAgentMessage::Interrupt),
                ControlMode::Collect => None,
            };
            if let Some(message) = timeline_message {
                let _ = recorder.record_sent(&message);
                let _ = recorder.flush_checkpoint();
            }
        }

        let mailbox_path = self.mailbox_path.clone();
        let mailbox_id = request.mailbox_id.clone();
        let recipient = request.recipient.clone();
        let completed = tokio::task::spawn_blocking(move || {
            crate::mailbox::MailboxStore::with_path(mailbox_path).complete_delivery(
                &mailbox_id,
                &recipient,
                None,
            )
        })
        .await
        .is_ok_and(|result| result.is_ok());
        if !completed {
            return ChildControlOutcome::Continue;
        }

        let body = credential_vault.resolve_all(&request.body);
        let outcome = match request.mode {
            ControlMode::Steer => agent
                .prompt_with_kind(body, Vec::new(), PromptKind::Steer, Some(receipt.queue_id))
                .await
                .map(|()| ChildControlOutcome::Continue),
            ControlMode::Followup => agent
                .prompt_with_kind(
                    body,
                    Vec::new(),
                    PromptKind::FollowUp,
                    Some(receipt.queue_id),
                )
                .await
                .map(|()| ChildControlOutcome::Continue),
            ControlMode::Interrupt => {
                agent.cancel_keep_queue();
                Ok(ChildControlOutcome::Interrupted)
            }
            ControlMode::Cancel => {
                agent.cancel();
                Ok(ChildControlOutcome::Cancelled)
            }
            ControlMode::Collect => Ok(ChildControlOutcome::Continue),
        };
        let Ok(outcome) = outcome else {
            return ChildControlOutcome::Continue;
        };
        receipt.state = DurableControlReceiptState::Applied;
        if self.write_control_receipt(record, &receipt).is_err() {
            return ChildControlOutcome::Continue;
        }
        outcome
    }

    fn control_receipts_dir(record: &SubagentRecord) -> PathBuf {
        Self::session_dir(record).join("control-receipts")
    }

    fn control_receipt_path(record: &SubagentRecord, mailbox_message_id: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(mailbox_message_id.as_bytes());
        Self::control_receipts_dir(record).join(format!("{:x}.json", hasher.finalize()))
    }

    fn control_receipt(
        &self,
        record: &SubagentRecord,
        mailbox_message_id: &str,
    ) -> Option<DurableControlReceipt> {
        let path = Self::control_receipt_path(record, mailbox_message_id);
        let bytes = std::fs::read(path).ok()?;
        let mut receipt: DurableControlReceipt = serde_json::from_slice(&bytes).ok()?;
        if receipt.queue_id == 0 {
            receipt.queue_id = control_queue_id(&receipt.mailbox_message_id);
        }
        (receipt.mailbox_message_id == mailbox_message_id).then_some(receipt)
    }

    fn write_control_receipt(
        &self,
        record: &SubagentRecord,
        receipt: &DurableControlReceipt,
    ) -> Result<(), String> {
        let directory = Self::control_receipts_dir(record);
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("create control receipt directory: {error}"))?;
        let body = serde_json::to_vec(receipt)
            .map_err(|error| format!("serialize control receipt: {error}"))?;
        crate::fs_atomic::write_atomic(
            Self::control_receipt_path(record, &receipt.mailbox_message_id),
            body,
        )
        .map_err(|error| format!("persist control receipt: {error}"))
    }

    fn control_receipts(&self, record: &SubagentRecord) -> Vec<DurableControlReceipt> {
        let Ok(entries) = std::fs::read_dir(Self::control_receipts_dir(record)) else {
            return Vec::new();
        };
        let mut receipts = entries
            .flatten()
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|body| serde_json::from_slice(&body).ok())
            .map(|mut receipt: DurableControlReceipt| {
                if receipt.queue_id == 0 {
                    receipt.queue_id = control_queue_id(&receipt.mailbox_message_id);
                }
                receipt
            })
            .collect::<Vec<DurableControlReceipt>>();
        receipts.sort_by(|left, right| {
            left.acceptance_sequence
                .cmp(&right.acceptance_sequence)
                .then_with(|| left.accepted_at_ms.cmp(&right.accepted_at_ms))
                .then_with(|| left.mailbox_message_id.cmp(&right.mailbox_message_id))
        });
        receipts
    }

    fn next_control_receipt_sequence(&self, record: &SubagentRecord) -> u64 {
        self.control_receipts(record)
            .into_iter()
            .map(|receipt| receipt.acceptance_sequence)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }
}

fn handoff_package_summary(package: &serde_json::Value) -> String {
    let manifest = package.get("manifest").unwrap_or(package);
    let package_id = manifest
        .get("package_id")
        .or_else(|| manifest.get("packageId"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let target = manifest
        .get("target_thread_id")
        .or_else(|| manifest.get("targetThreadId"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let items = package
        .get("items")
        .and_then(serde_json::Value::as_array)
        .or_else(|| manifest.get("items").and_then(serde_json::Value::as_array));
    let item_count = items.map_or(0, Vec::len);
    let bytes = items.map_or(0, |items| {
        items
            .iter()
            .filter_map(|item| {
                item.get("metadata")
                    .and_then(|metadata| metadata.get("size_bytes"))
                    .or_else(|| item.get("sizeBytes"))
                    .and_then(serde_json::Value::as_u64)
            })
            .sum::<u64>()
    });
    format!(
        "Handoff package {package_id} addressed to {target} ({item_count} items, {bytes} bytes)"
    )
}

fn handoff_package_list_summary(packages: &serde_json::Value) -> String {
    let Some(entries) = packages
        .get("packages")
        .and_then(serde_json::Value::as_array)
    else {
        return "No handoff packages".to_string();
    };
    if entries.is_empty() {
        return "No handoff packages".to_string();
    }
    entries
        .iter()
        .map(|package| {
            let package_id = package
                .get("package_id")
                .or_else(|| package.get("packageId"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let source = package
                .get("source_thread_id")
                .or_else(|| package.get("sourceThreadId"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let item_count = package
                .get("items")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            format!("{package_id}  from {source}  ({item_count} items)")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn default_root(cwd: &Path) -> PathBuf {
    let base = std::env::var_os("MAESTRO_SUBAGENTS_DIR")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir);
    let base = if std::env::var_os("MAESTRO_SUBAGENTS_DIR").is_some() {
        base
    } else {
        base.join(".maestro").join("subagents")
    };
    let canonical_cwd = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let canonical_cwd_string = canonical_cwd.to_string_lossy();
    let digest = Sha256::digest(canonical_cwd_string.as_bytes());
    let digest = format!("{digest:x}");
    base.join(format!(
        "--{}-{}--",
        sanitize_path_for_dirname(&canonical_cwd_string),
        &digest[..16]
    ))
}

fn parse_spawn_request(args: &serde_json::Value) -> Result<SpawnRequest, String> {
    let task = args
        .get("task")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "task is required to spawn a subagent".to_string())?;
    if task.len() > MAX_TASK_BYTES {
        return Err(format!(
            "subagent task exceeds the {} byte limit",
            MAX_TASK_BYTES
        ));
    }

    let role = SubagentRole::parse(args.get("role").and_then(serde_json::Value::as_str))?;
    let backend = SubagentBackend::parse(args.get("backend").and_then(serde_json::Value::as_str))?;
    let computer = args.get("computer");
    let orb_alias = args.get("orb");
    if computer.is_some() && orb_alias.is_some() {
        return Err(
            "provide only one hosted Computer configuration: `computer` or its `orb` compatibility alias"
                .to_string(),
        );
    }
    let orb = parse_orb_delegation_config(computer.or(orb_alias))?;
    if args.get("specialist").is_some()
        && (args.get("profile").is_some() || args.get("agent_profile").is_some())
    {
        return Err("provide only one of specialist or profile".into());
    }
    if args.get("specialist").is_some() && backend != SubagentBackend::Native {
        return Err("specialist selection is supported for native child agents; use computer.profile for hosted placement".into());
    }
    if let Some(value) = args.get("specialist") {
        if value.as_str().is_none_or(|name| name.trim().is_empty()) {
            return Err("specialist must be a non-empty string".into());
        }
    }
    let profile = args
        .get("specialist")
        .or_else(|| args.get("profile"))
        .or_else(|| args.get("agent_profile"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_ascii_lowercase);
    let isolation =
        SubagentIsolation::parse(args.get("isolation").and_then(serde_json::Value::as_str))?;
    let model = args
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    let work_difficulty = match args.get("work_type") {
        None => None,
        Some(value) => Some(match value.as_str() {
            Some("lookup") => TaskDifficulty::Light,
            Some("implementation") => TaskDifficulty::Medium,
            Some("diagnosis") => TaskDifficulty::Heavy,
            _ => return Err("work_type must be lookup, implementation, or diagnosis".into()),
        }),
    };
    let difficulty = match args.get("difficulty") {
        Some(value) => TaskDifficulty::parse(value.as_str().ok_or("difficulty must be a string")?)?,
        None if work_difficulty.is_some() => work_difficulty.expect("checked work type"),
        None if role == SubagentRole::Explore => TaskDifficulty::Light,
        None => TaskDifficulty::Medium,
    };
    let thinking = args
        .get("thinking")
        .map(|value| {
            value
                .as_str()
                .and_then(ThinkingLevel::parse)
                .ok_or_else(|| {
                    "thinking must be off, minimal, low, medium, high, or max".to_string()
                })
        })
        .transpose()?;
    if backend != SubagentBackend::Native
        && (thinking.is_some()
            || args.get("difficulty").is_some()
            || args.get("work_type").is_some())
    {
        return Err("thinking and difficulty currently apply to native workers only; hosted model selection belongs to Platform".into());
    }
    let timeout_ms = parse_child_timeout(args)?;
    let max_tokens = parse_child_max_tokens(args)?;
    let run_in_background = args
        .get("run_in_background")
        .or_else(|| args.get("runInBackground"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let worktree_name = args
        .get("worktree_name")
        .or_else(|| args.get("worktreeName"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);

    Ok(SpawnRequest {
        task,
        role,
        backend,
        orb,
        profile,
        profile_prompt: None,
        profile_tools: None,
        model,
        thinking,
        difficulty,
        timeout_ms,
        max_tokens,
        run_in_background,
        isolation,
        worktree_name,
    })
}

fn parse_orb_delegation_config(
    value: Option<&serde_json::Value>,
) -> Result<OrbDelegationConfig, String> {
    let Some(value) = value else {
        return Ok(OrbDelegationConfig::default());
    };
    let Some(object) = value.as_object() else {
        return Err("computer configuration must be an object".to_string());
    };
    let unsupported = [
        "repository_url",
        "agent_selection",
        "provider_identity_id",
        "permission_policy",
        "require_approval",
        "approval_policy",
        "provisioner",
        "machine",
        "resource_profile",
        "max_run_cost_credits",
    ];
    if let Some(field) = unsupported
        .iter()
        .find(|field| object.contains_key(**field))
    {
        return Err(format!(
            "computer.{field} is policy-owned infrastructure; provide only project/repository intent and an optional high-level computer.profile"
        ));
    }
    let string_field = |name: &str| {
        object
            .get(name)
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| format!("computer.{name} must be a non-empty string"))
            })
            .transpose()
    };
    let repository = match object.get("repository").or_else(|| object.get("repo")) {
        Some(value) => {
            let value = value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "computer.repository must be a non-empty string".to_string())?;
            validate_orb_repository_intent(value)?;
            Some(value.to_string())
        }
        None => None,
    };
    let profile = string_field("profile")?;
    if let Some(profile) = profile.as_deref() {
        validate_orb_profile_override(profile)?;
    }
    Ok(OrbDelegationConfig {
        project: string_field("project")?,
        repository,
        title: string_field("title")?,
        profile,
        settings: OrbSpawnSettings::default(),
    })
}

/// Fill the minimum hosted Computer launch context from the active workspace.
///
/// The model should not have to manufacture a project id or copy a repository
/// URL just because the user said "work on this in a Computer". When the
/// request omits either value, use the current workspace's `origin` remote,
/// normalize it to the HTTPS form required by `computer_launch`, and derive the
/// project identity from its path. Explicit caller values always win; a
/// missing or non-remote-backed workspace fails closed with an actionable
/// error rather than launching against an inferred personal/default project.
fn infer_hosted_computer_context(
    config: &mut OrbDelegationConfig,
    cwd: &Path,
) -> Result<(), String> {
    // Explicit repository intent still crosses the hosted launch boundary,
    // so canonicalize and validate it even when the caller supplied both
    // project and repository.  The previous early return let an absolute
    // `http://` URL (or one carrying a query/fragment) reach `computer_launch`
    // unchanged.
    if let Some(repository) = config.repository.as_deref() {
        config.repository = Some(normalize_hosted_repository_url(repository)?);
    }
    if let Some(repository) = config.settings.repository_url.as_deref() {
        config.settings.repository_url = Some(normalize_hosted_repository_url(repository)?);
    }

    let needs_project = config
        .project
        .as_deref()
        .is_none_or(|project| project.trim().is_empty());
    let needs_repository = config
        .settings
        .repository_url
        .as_deref()
        .is_none_or(|repository| repository.trim().is_empty())
        && config
            .repository
            .as_deref()
            .is_none_or(|repository| repository.trim().is_empty());

    if !needs_project && !needs_repository {
        return Ok(());
    }

    let repository_source = config
        .settings
        .repository_url
        .as_deref()
        .or(config.repository.as_deref())
        .map(str::to_owned)
        .or_else(|| git_origin_url(cwd));
    let Some(repository_source) = repository_source else {
        return Err(
            "Computer delegation needs the active workspace's remote repository; configure `computer.repository` or add an origin remote"
                .to_string(),
        );
    };
    let repository_url = normalize_hosted_repository_url(&repository_source)?;

    if needs_repository {
        config.repository = Some(repository_url.clone());
    }
    if needs_project {
        config.project = Some(hosted_project_from_repository_url(&repository_url)?);
    }
    Ok(())
}

fn git_origin_url(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then_some(value.to_string())
}

/// Normalize a credential-free Git origin into the HTTPS URL accepted by the
/// hosted Computer launch contract.
fn normalize_hosted_repository_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("computer.repository must be a non-empty repository URL".to_string());
    }

    if let Some(scp) = value.strip_prefix("git@") {
        let Some((host, path)) = scp.split_once(':') else {
            return Err(
                "computer.repository must be an HTTPS URL or a normal SSH Git origin".to_string(),
            );
        };
        return https_repository_url(host, path);
    }

    let parsed = url::Url::parse(value).map_err(|_| {
        "computer.repository must be an HTTPS URL or a normal SSH Git origin".to_string()
    })?;
    if parsed.username() != "" || parsed.password().is_some() {
        return Err("computer.repository must not contain embedded credentials".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("computer.repository must not contain a query or fragment".to_string());
    }
    if parsed.scheme() == "ssh" {
        let Some(host) = parsed.host_str() else {
            return Err("computer.repository SSH origin is missing a host".to_string());
        };
        return https_repository_url(host, parsed.path());
    }
    if parsed.scheme() != "https" {
        return Err("computer.repository must use HTTPS for hosted Computer launches".to_string());
    }
    if parsed.host_str().is_none() || parsed.path().trim_matches('/').is_empty() {
        return Err("computer.repository must include a host and repository path".to_string());
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn https_repository_url(host: &str, path: &str) -> Result<String, String> {
    let host = host.trim();
    let path = path.trim().trim_start_matches('/');
    if host.is_empty() || path.is_empty() || path.chars().any(char::is_control) {
        return Err("computer.repository must include a host and repository path".to_string());
    }
    let url = format!("https://{host}/{path}");
    let parsed = url::Url::parse(&url)
        .map_err(|_| "computer.repository could not be normalized to HTTPS".to_string())?;
    if parsed.host_str().is_none() || parsed.path().trim_matches('/').is_empty() {
        return Err("computer.repository must include a host and repository path".to_string());
    }
    Ok(url.trim_end_matches('/').to_string())
}

fn hosted_project_from_repository_url(repository_url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(repository_url)
        .map_err(|_| "computer.repository must be a valid HTTPS URL".to_string())?;
    let project = parsed
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    let project = project.trim_end_matches(".git").trim_matches('/');
    if project.is_empty() {
        return Err(
            "Computer delegation could not infer a project from the repository URL; provide `computer.project`"
                .to_string(),
        );
    }
    if project.chars().count() > 128 {
        return Err("computer.project is too long after repository inference".to_string());
    }
    Ok(project.to_string())
}

fn validate_orb_repository_intent(value: &str) -> Result<(), String> {
    let repository_url = url::Url::parse(value)
        .map_err(|_| "computer.repository must be a valid absolute URL".to_string())?;
    let has_credentials =
        !repository_url.username().is_empty() || repository_url.password().is_some();
    if has_credentials {
        return Err("computer.repository must not contain embedded credentials".to_string());
    }
    Ok(())
}

fn validate_orb_profile_override(value: &str) -> Result<(), String> {
    if value.len() > 64
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(
            "computer.profile must be a bounded hosted profile id using letters, numbers, '.', '-', or '_'"
                .to_string(),
        );
    }
    Ok(())
}

fn apply_orb_delegation_policy(request: &mut SpawnRequest) {
    if let Some(profile) = request.orb.profile.clone() {
        request.orb.settings.resource_profile = Some(profile);
    }
    if request.orb.settings.repository_url.is_none() {
        request.orb.settings.repository_url = request.orb.repository.clone();
    }
    if request.orb.settings.model.is_none() {
        request.orb.settings.model = request.model.clone();
    }
}

fn default_child_timeout_ms() -> u64 {
    DEFAULT_CHILD_TIMEOUT_MS
}

fn default_child_max_tokens() -> u32 {
    DEFAULT_CHILD_MAX_TOKENS
}

fn parse_child_timeout(args: &serde_json::Value) -> Result<u64, String> {
    let Some(value) = args.get("timeout_ms").or_else(|| args.get("timeoutMs")) else {
        return Ok(DEFAULT_CHILD_TIMEOUT_MS);
    };
    let timeout_ms = value
        .as_u64()
        .ok_or_else(|| "timeout_ms must be a positive integer".to_string())?;
    if timeout_ms == 0 {
        return Err("timeout_ms must be a positive integer".to_string());
    }
    Ok(timeout_ms.min(MAX_CHILD_TIMEOUT_MS))
}

fn parse_child_max_tokens(args: &serde_json::Value) -> Result<u32, String> {
    let Some(value) = args.get("max_tokens").or_else(|| args.get("maxTokens")) else {
        return Ok(DEFAULT_CHILD_MAX_TOKENS);
    };
    let max_tokens = value
        .as_u64()
        .ok_or_else(|| "max_tokens must be a positive integer".to_string())?;
    if max_tokens == 0 {
        return Err("max_tokens must be a positive integer".to_string());
    }
    Ok(max_tokens.min(u64::from(MAX_CHILD_MAX_TOKENS)) as u32)
}

fn role_instructions(role: SubagentRole) -> &'static str {
    match role {
        SubagentRole::Explore => {
            "Explore only the assigned question. Return precise file and symbol locations, relevant relationships, and uncertainties. Avoid broad tours and repeated searches. Do not implement changes."
        }
        SubagentRole::Plan => {
            "Return a bounded implementation plan grounded in existing code, with dependencies and unresolved decisions. Do not implement changes."
        }
        SubagentRole::Review => {
            "Inspect the assigned diff and causal behavior. Return actionable findings with file locations and evidence; distinguish proven defects from hypotheses. Do not implement changes."
        }
        SubagentRole::Code => {
            "Implement the assigned change and run relevant checks. Carry every unfinished requirement into the handoff; a partial implementation is not complete."
        }
    }
}

fn resolve_spawn_profile(request: &mut SpawnRequest, cwd: &Path) -> Result<(), String> {
    resolve_spawn_profile_with_trust(
        request,
        cwd,
        crate::config::workspace_trusted_in_global_config(cwd),
    )
}

fn resolve_spawn_profile_with_trust(
    request: &mut SpawnRequest,
    cwd: &Path,
    trusted: bool,
) -> Result<(), String> {
    // Role defaults are local preferences, not hosted placement/model authority.
    if request.profile.is_none() && request.backend != SubagentBackend::Native {
        return Ok(());
    }
    let explicit = request.profile.is_some();
    let profile_name = request
        .profile
        .clone()
        .unwrap_or_else(|| format!("role-{}", request.role.label()));
    let plugin_registry = crate::plugins::PluginRegistry::discover_for_workspace(cwd);
    let agent_paths = plugin_registry.agent_paths();
    let profiles = crate::agents_cli::profiles_for_delegation(cwd, &agent_paths, trusted)
        .map_err(|error| format!("load agent profiles: {error}"))?;
    let Some(profile) = profiles.into_iter().find(|profile| {
        profile.name == profile_name
            && (trusted
                || matches!(
                    profile.scope,
                    crate::agents_cli::Scope::User | crate::agents_cli::Scope::Builtin
                ))
    }) else {
        return if explicit {
            Err(format!(
                "agent profile `{profile_name}` was not found in an authorized scope"
            ))
        } else {
            Ok(())
        };
    };
    if request.backend == SubagentBackend::Native && request.thinking.is_none() {
        request.thinking = profile.thinking;
    }
    request.profile = Some(profile.name);
    request.profile_prompt = Some(profile.prompt);
    request.profile_tools = profile.tools;
    if request.model.is_none() {
        request.model = profile.model;
    }
    Ok(())
}

fn fallback_history_from_prompt(prompt: &str) -> Option<Vec<crate::ai::Message>> {
    let prompt = prompt.trim();
    (!prompt.is_empty()).then(|| {
        vec![crate::ai::Message {
            role: crate::ai::Role::User,
            content: crate::ai::MessageContent::text(prompt),
        }]
    })
}

fn restore_history_with_prompt(
    history: Option<Vec<crate::ai::Message>>,
    prompt: &str,
    attempt: u32,
    snapshot_attempt: Option<u32>,
) -> Option<Vec<crate::ai::Message>> {
    let Some(fallback) = fallback_history_from_prompt(prompt) else {
        return history;
    };
    if snapshot_attempt == Some(attempt) {
        return history.or(Some(fallback));
    }
    match history {
        Some(mut history) => {
            history.extend(fallback);
            Some(history)
        }
        None => Some(fallback),
    }
}

fn subagent_id(args: &serde_json::Value) -> Option<&str> {
    args.get("subagent_id")
        .or_else(|| args.get("subagentId"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

fn agent_ref(record: &SubagentRecord) -> String {
    format!("subagent:{}:{}", record.id, record.attempt)
}

fn parse_agent_ref(value: &str) -> Result<(String, u32), String> {
    let mut parts = value.trim().split(':');
    let prefix = parts.next();
    let id = parts.next();
    let attempt = parts.next();
    if prefix != Some("subagent") || parts.next().is_some() {
        return Err("agent_ref must have the form subagent:<uuid>:<attempt>".to_string());
    }
    let id = id.ok_or_else(|| "agent_ref is missing its subagent id".to_string())?;
    let id = SubagentManager::validate_id(id)?;
    let attempt = attempt
        .ok_or_else(|| "agent_ref is missing its attempt".to_string())?
        .parse::<u32>()
        .map_err(|_| "agent_ref attempt must be a non-negative integer".to_string())?;
    Ok((id, attempt))
}

fn parse_wait_timeout(args: &serde_json::Value) -> Result<u64, String> {
    let Some(value) = args.get("timeout_ms").or_else(|| args.get("timeoutMs")) else {
        return Ok(0);
    };
    value
        .as_u64()
        .map(|timeout_ms| timeout_ms.min(MAX_WAIT_MS))
        .ok_or_else(|| "timeout_ms must be a non-negative integer".to_string())
}

const READ_ONLY_CHILD_TOOLS: [&str; 21] = [
    "read",
    "glob",
    "grep",
    "list",
    "diff",
    "status",
    "parallel_ripgrep",
    "search",
    "web_fetch",
    "websearch",
    "codesearch",
    "read_image",
    "screenshot",
    "vscode_get_diagnostics",
    "jetbrains_get_diagnostics",
    "vscode_get_definition",
    "jetbrains_get_definition",
    "vscode_find_references",
    "jetbrains_find_references",
    "mcp_list_resources",
    "mcp_read_resource",
];

fn child_allowed_tools_for_role(
    role: SubagentRole,
    profile_tools: Option<&[String]>,
) -> HashSet<String> {
    let mut tools = ToolRegistry::new()
        .tools()
        .filter(|definition| {
            let name = definition.tool.name.to_ascii_lowercase();
            let globally_allowed = !SUBAGENT_TOOL_NAMES.contains(&name.as_str())
                && !matches!(
                    name.as_str(),
                    "get_goal" | "update_goal" | "todo" | "background_tasks" | "coding_task"
                );
            let role_allowed = matches!(role, SubagentRole::Code)
                || READ_ONLY_CHILD_TOOLS.contains(&name.as_str());
            globally_allowed && role_allowed
        })
        .map(|definition| definition.tool.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();

    if let Some(profile_tools) = profile_tools {
        let requested = profile_tools
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        tools.retain(|name| requested.contains(name));
    }
    tools
}

fn git_worktree_command<const N: usize>(
    cwd: &Path,
    args: [&str; N],
    appended_path: Option<&Path>,
) -> Result<std::process::Output, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    if let Some(path) = appended_path {
        command.arg(path);
    }
    command.output().map_err(|error| {
        format!(
            "run git {}: {error}",
            command.get_program().to_string_lossy()
        )
    })
}

fn git_worktree_is_registered(cwd: &Path, path: &Path) -> bool {
    let Ok(output) = git_worktree_command(cwd, ["worktree", "list", "--porcelain"], None) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(Path::new)
        .any(|candidate| candidate == path)
}

fn apply_subagent_start_hook(
    request: &mut SpawnRequest,
    cwd: &Path,
    parent_scope_id: &str,
) -> Result<(), String> {
    let mut hooks = IntegratedHookSystem::load_from_config(&cwd.to_string_lossy());
    // Local load skips the runner's SetSessionContext wiring, so stamp the
    // raw parent session id (not the `session:` routing scope) before
    // dispatching so payloads match every other hook in that session.
    let hook_session = crate::agent::ParentScopeId::from_raw(parent_scope_id).hook_session_id();
    hooks.set_session_id(Some(hook_session.into_string()));
    match hooks.execute_subagent_start(request.role.label(), &request.task, Some(parent_scope_id)) {
        HookResult::Continue => Ok(()),
        HookResult::Block { reason } => Err(format!("subagent spawn blocked by hook: {reason}")),
        HookResult::ModifyInput { new_input } => {
            match new_input {
                serde_json::Value::String(task) => request.task = task,
                serde_json::Value::Object(input) => {
                    if let Some(task) = input.get("task").and_then(serde_json::Value::as_str) {
                        request.task = task.to_string();
                    }
                    if let Some(role) = input.get("role").and_then(serde_json::Value::as_str) {
                        request.role = SubagentRole::parse(Some(role))?;
                    }
                }
                _ => return Err("subagent start hook returned an invalid input".to_string()),
            }
            Ok(())
        }
        HookResult::InjectContext { context } => {
            if !context.trim().is_empty() {
                request.task = format!("{}\n\nAdditional context:\n{}", request.task, context);
            }
            Ok(())
        }
    }?;

    request.task = request.task.trim().to_string();
    if request.task.is_empty() {
        return Err("subagent start hook returned an empty task".to_string());
    }
    if request.task.len() > MAX_TASK_BYTES {
        return Err(format!(
            "subagent task exceeds the {} byte limit after hook modification",
            MAX_TASK_BYTES
        ));
    }
    Ok(())
}

/// Sandbox policy for a child in `role`, given the parent's `policy`.
///
/// A role that is not allowed to mutate gets [`SandboxPolicy::ReadOnly`]
/// outright rather than inheriting the parent's. Restricting the advertised
/// tool list is not enough on its own: on the Codex app-server transport the
/// child also reaches native `commandExecution` and `fileChange` operations,
/// which never pass through the Maestro tool registry, and with
/// `isolation=shared` those act on the parent's own checkout. The policy is
/// what both transports enforce against, so it is where the role has to be
/// expressed.
fn child_sandbox_policy(
    role: SubagentRole,
    policy: Option<SandboxPolicy>,
) -> Option<SandboxPolicy> {
    if !role.can_mutate() {
        return Some(SandboxPolicy::ReadOnly);
    }
    match policy {
        Some(SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        }) => Some(SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        }),
        other => other,
    }
}

fn worktree_setup_allowed(policy: Option<&SandboxPolicy>) -> bool {
    matches!(policy, None | Some(SandboxPolicy::DangerFullAccess))
}

fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(OsString::from(String::from_utf8_lossy(bytes).into_owned()))
    }
}

fn changed_files(cwd: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(cwd)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut files = Vec::new();
    let mut records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let path = &record[3..];
        if path.is_empty() {
            continue;
        }
        files.push(path_from_git_bytes(path));

        let status = &record[..2];
        if status.contains(&b'R') || status.contains(&b'C') {
            if let Some(previous) = records.next().filter(|path| !path.is_empty()) {
                files.push(path_from_git_bytes(previous));
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

fn changed_file_fingerprints(cwd: &Path) -> HashMap<PathBuf, String> {
    let repository_root = git_repository_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let paths = changed_files(cwd);
    let index_fingerprints = git_index_fingerprints(&repository_root);
    paths
        .into_iter()
        .map(|path| {
            let index_fingerprint = index_fingerprints
                .get(&path)
                .map(String::as_str)
                .unwrap_or_default();
            let fingerprint = fingerprint_file(&repository_root, &path, index_fingerprint);
            (path, fingerprint)
        })
        .collect()
}

fn changed_file_baseline(cwd: &Path) -> (Vec<PathBuf>, HashMap<PathBuf, String>) {
    let fingerprints = changed_file_fingerprints(cwd);
    let mut files = fingerprints.keys().cloned().collect::<Vec<_>>();
    files.sort();
    (files, fingerprints)
}

fn serialize_file_baseline(
    paths: Vec<PathBuf>,
    fingerprints: HashMap<PathBuf, String>,
) -> (Vec<String>, HashMap<String, String>) {
    let paths = paths
        .into_iter()
        .map(|path| serialize_repository_path(&path))
        .collect();
    let fingerprints = fingerprints
        .into_iter()
        .map(|(path, fingerprint)| (serialize_repository_path(&path), fingerprint))
        .collect();
    (paths, fingerprints)
}

// The leading NUL cannot occur in a Unix filename, so this stays
// unambiguous with legacy records that stored UTF-8 paths directly.
const SERIALIZED_PATH_PREFIX: &str = "\0maestro-path-v1:";

fn serialize_repository_path(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = path.as_os_str().as_bytes().to_vec();
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing path bytes to a string cannot fail");
    }
    format!("{SERIALIZED_PATH_PREFIX}{encoded}")
}

fn deserialize_repository_path(serialized: &str) -> PathBuf {
    let Some(encoded) = serialized.strip_prefix(SERIALIZED_PATH_PREFIX) else {
        return PathBuf::from(serialized);
    };
    if encoded.len() % 2 != 0 {
        return PathBuf::from(serialized);
    }
    let Some(bytes) = (0..encoded.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&encoded[offset..offset + 2], 16).ok())
        .collect::<Option<Vec<_>>>()
    else {
        return PathBuf::from(serialized);
    };
    #[cfg(unix)]
    {
        PathBuf::from(OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(OsString::from(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

fn display_repository_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn git_repository_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

fn git_repository_head(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?;
    let head = head.trim();
    (!head.is_empty()).then(|| head.to_string())
}

fn fingerprint_file(
    repository_root: &Path,
    relative_path: &Path,
    index_fingerprint: &str,
) -> String {
    let path = repository_root.join(relative_path);
    let mut hasher = Sha256::new();
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            hasher.update(b"symlink:");
            update_file_mode(&mut hasher, &metadata);
            match std::fs::read_link(&path) {
                Ok(target) => hasher.update(target.to_string_lossy().as_bytes()),
                Err(_) => hasher.update(b"<unreadable>"),
            }
        }
        Ok(metadata) if metadata.is_file() => {
            hasher.update(b"file:");
            update_file_mode(&mut hasher, &metadata);
            match std::fs::read(&path) {
                Ok(contents) => hasher.update(contents),
                Err(_) => hasher.update(b"<unreadable>"),
            }
        }
        Ok(metadata) => {
            hasher.update(b"other:");
            update_file_mode(&mut hasher, &metadata);
            hasher.update(format!("{:?}", metadata.file_type()).as_bytes());
        }
        Err(_) => hasher.update(b"missing"),
    }
    hasher.update(b"\nindex:");
    hasher.update(index_fingerprint.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn update_file_mode(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    hasher.update(b"mode:");
    #[cfg(unix)]
    hasher.update((metadata.permissions().mode() & 0o7777).to_le_bytes());
    #[cfg(not(unix))]
    {
        let _ = metadata;
        hasher.update(b"unsupported");
    }
}

fn git_index_records(repository_root: &Path, stage: bool) -> Vec<(PathBuf, Vec<u8>)> {
    let args = if stage {
        ["ls-files", "--stage", "--full-name", "-z", "--"]
    } else {
        ["ls-files", "-v", "--full-name", "-z", "--"]
    };
    let Ok(output) = Command::new("git")
        .args(args)
        .current_dir(repository_root)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            if record.is_empty() {
                return None;
            }
            if stage {
                let separator = record.iter().position(|byte| *byte == b'\t')?;
                let path = record.get(separator + 1..)?;
                Some((path_from_git_bytes(path), record[..separator].to_vec()))
            } else {
                Some((
                    path_from_git_bytes(record.get(2..)?),
                    record.get(..2)?.to_vec(),
                ))
            }
        })
        .collect()
}

fn git_index_fingerprints(repository_root: &Path) -> HashMap<PathBuf, String> {
    let mut stage = HashMap::<PathBuf, Vec<u8>>::new();
    for (path, metadata) in git_index_records(repository_root, true) {
        let entry = stage.entry(path).or_default();
        entry.extend_from_slice(&metadata);
        entry.push(0);
    }
    let mut flags = HashMap::<PathBuf, Vec<u8>>::new();
    for (path, metadata) in git_index_records(repository_root, false) {
        let entry = flags.entry(path).or_default();
        entry.extend_from_slice(&metadata);
        entry.push(0);
    }

    stage
        .keys()
        .chain(flags.keys())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|path| {
            let mut hasher = Sha256::new();
            hasher.update(b"stage:");
            if let Some(value) = stage.get(&path) {
                hasher.update(value);
            }
            hasher.update([0]);
            hasher.update(b"flags:");
            if let Some(value) = flags.get(&path) {
                hasher.update(value);
            }
            (path, format!("{:x}", hasher.finalize()))
        })
        .collect()
}

fn committed_files_since(cwd: &Path, initial_head: Option<&str>) -> Vec<PathBuf> {
    let Some(initial_head) = initial_head else {
        return Vec::new();
    };
    let Some(current_head) = git_repository_head(cwd) else {
        return Vec::new();
    };
    if initial_head == current_head {
        return Vec::new();
    }

    let repository_root = git_repository_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let range = format!("{initial_head}..{current_head}");
    let output = Command::new("git")
        .args(["diff", "--name-only", "-z"])
        .arg(range)
        .arg("--")
        .current_dir(repository_root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_git_bytes)
        .collect()
}

fn changed_files_since(
    cwd: &Path,
    initial_head: Option<&str>,
    initial_files: &[String],
    initial_file_fingerprints: &HashMap<String, String>,
) -> Vec<String> {
    let mut changed = if initial_file_fingerprints.is_empty() {
        let initial = initial_files
            .iter()
            .map(|path| deserialize_repository_path(path))
            .collect::<HashSet<_>>();
        changed_files(cwd)
            .into_iter()
            .filter(|path| !initial.contains(path))
            .collect::<Vec<_>>()
    } else {
        let initial_file_fingerprints = initial_file_fingerprints
            .iter()
            .map(|(path, fingerprint)| (deserialize_repository_path(path), fingerprint.clone()))
            .collect::<HashMap<_, _>>();
        let current_file_fingerprints = changed_file_fingerprints(cwd);
        let paths = initial_file_fingerprints
            .keys()
            .chain(current_file_fingerprints.keys())
            .cloned()
            .collect::<HashSet<_>>();
        paths
            .into_iter()
            .filter(|path| {
                initial_file_fingerprints.get(path) != current_file_fingerprints.get(path)
            })
            .collect::<Vec<_>>()
    };
    changed.extend(committed_files_since(cwd, initial_head));
    changed.sort();
    changed.dedup();
    changed
        .into_iter()
        .map(|path| display_repository_path(&path))
        .collect()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn status_label(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => "queued",
        SubagentStatus::Running => "running",
        SubagentStatus::Completed => "completed",
        SubagentStatus::Failed => "failed",
        SubagentStatus::Cancelled => "cancelled",
        SubagentStatus::TimedOut => "timed_out",
        SubagentStatus::Interrupted => "interrupted",
    }
}

fn child_terminal_status(
    cancelled: bool,
    interrupted: bool,
    timed_out: bool,
    run_error: Option<String>,
    timeout_ms: u64,
) -> (SubagentStatus, Option<String>) {
    if cancelled {
        return (
            SubagentStatus::Cancelled,
            Some("subagent cancelled".to_string()),
        );
    }
    if interrupted {
        return (
            SubagentStatus::Interrupted,
            Some("subagent interrupted".to_string()),
        );
    }
    if timed_out {
        return (
            SubagentStatus::TimedOut,
            Some(format!(
                "subagent exceeded its {timeout_ms} ms execution budget"
            )),
        );
    }
    if let Some(error) = run_error {
        return (SubagentStatus::Failed, Some(error));
    }
    (SubagentStatus::Completed, None)
}

fn prepare_control_body(
    parent_vault: &CredentialVault,
    child_vault: Option<&CredentialVault>,
    body: &str,
) -> Result<String, String> {
    if let Some(child_vault) = child_vault {
        return parent_vault.rekey_references_to(child_vault, body);
    }
    if CredentialVault::has_references(body) {
        return Err(
            "control message contains credential references that cannot be re-keyed for a child running in another Deixic Code process"
                .to_string(),
        );
    }
    Ok(body.to_string())
}

fn record_details(record: &SubagentRecord) -> serde_json::Value {
    let cwd = deserialize_repository_path(&record.cwd);
    let worktree_path = record
        .worktree_path
        .as_deref()
        .map(deserialize_repository_path)
        .map(|path| display_repository_path(&path));
    let session_dir = SubagentManager::session_dir(record);
    let timeline_path = SubagentManager::timeline_path(record);
    let mut details = serde_json::json!({
        "subagentId": record.id,
        "agentRef": agent_ref(record),
        "childSessionId": record.id,
        "parentScopeId": record.parent_scope_id,
        "parentCallId": record.parent_call_id,
        "lastParentScopeId": record.last_parent_scope_id,
        "lastCallId": record.last_call_id,
        "task": record.task,
        "currentPrompt": record.current_prompt,
        "role": record.role,
        "backend": record.backend,
        "orbThreadId": record.orb.as_ref().map(|orb| &orb.thread_id),
        "orbReceiptId": record.orb.as_ref().and_then(|orb| orb.receipt_id.as_ref()),
        "orbLifecycleState": record.orb.as_ref().and_then(|orb| orb.lifecycle_state.as_ref()),
        "profile": record.profile,
        "model": record.model,
        "timeoutMs": record.timeout_ms,
        "maxTokens": record.max_tokens,
        "isolation": record.isolation,
        "cwd": display_repository_path(&cwd),
        "worktreePath": worktree_path,
        "worktreeCleaned": record.worktree_cleaned,
        "sessionDir": display_repository_path(&session_dir),
        "timelinePath": display_repository_path(&timeline_path),
        "status": record.status,
        "attempt": record.attempt,
        "snapshotAttempt": record.snapshot_attempt,
        "createdAtMs": record.created_at_ms,
        "startedAtMs": record.started_at_ms,
        "finishedAtMs": record.finished_at_ms,
        "result": record.result,
        "error": record.error,
        "recoverable": matches!(
            record.status,
            SubagentStatus::Queued
                | SubagentStatus::Running
                | SubagentStatus::TimedOut
                | SubagentStatus::Interrupted
        )
    });
    if record.backend == SubagentBackend::Native {
        let handoff = record
            .result
            .as_ref()
            .map(|result| crate::subagents::handoff::parse(&result.output));
        details["handoff"] =
            serde_json::json!(handoff.as_ref().and_then(|value| value.as_ref().ok()));
        details["handoffError"] =
            serde_json::json!(handoff.as_ref().and_then(|value| value.as_ref().err()));
        details["completionVerified"] = serde_json::json!(false);
        details["nextAction"] = serde_json::json!(if record.status.is_terminal() {
            "inspect_handoff_and_continue_original_task"
        } else {
            "wait"
        });
    }
    details
}

fn is_orb_owner_binding_error(error: &str) -> bool {
    error.starts_with("Hosted Computer owner binding")
        || error.starts_with("Hosted Computer task has no durable owner binding")
        || error.starts_with("Hosted Computer task has an incomplete durable owner binding")
}

fn orb_console_task(record: &SubagentRecord, unavailable: Option<&str>) -> OrbConsoleTask {
    let orb = record.orb.as_ref();
    let raw_state = unavailable
        .map(|_| "unavailable")
        .or_else(|| orb.and_then(|orb| orb.lifecycle_state.as_deref()))
        .unwrap_or_else(|| status_label(record.status));
    let summary = record
        .result
        .as_ref()
        .map(|result| result.output.trim())
        .filter(|summary| !summary.is_empty())
        .or(record.error.as_deref())
        .or(Some(record.task.as_str()));
    let event = orb_delegation_event(
        &format!("{}:{}", record.id, record.attempt),
        &record.id,
        record.attempt,
        raw_state,
        summary,
        unavailable.map(|_| "Hosted Computer is unavailable"),
        orb.map(|orb| orb.available_commands.as_slice())
            .unwrap_or(&[]),
    );
    let recoverable = unavailable.is_some()
        || matches!(
            event.lifecycle_state,
            DelegationLifecycleState::Queued
                | DelegationLifecycleState::Active
                | DelegationLifecycleState::Paused
                | DelegationLifecycleState::NeedsAttention
                | DelegationLifecycleState::ApprovalRequired
                | DelegationLifecycleState::Failed
                | DelegationLifecycleState::Unavailable
        );
    OrbConsoleTask {
        id: record.id.clone(),
        agent_ref: agent_ref(record),
        task: record.task.clone(),
        attempt: record.attempt,
        event,
        thread_id: orb
            .map(|orb| orb.thread_id.clone())
            .filter(|thread_id| !thread_id.is_empty()),
        receipt_id: orb.and_then(|orb| orb.receipt_id.clone()),
        recoverable,
        result: record.result.clone(),
        error: record.error.clone(),
    }
}

fn orb_console_task_result(task: OrbConsoleTask, message: String) -> ToolResult {
    ToolResult::success(message).with_details(serde_json::json!({
        "schemaVersion": maestro_runtime::DELEGATION_PROJECTION_SCHEMA_VERSION,
        "task": task,
    }))
}

fn orb_console_task_failure(task: OrbConsoleTask, message: String) -> ToolResult {
    ToolResult::failure(message).with_details(serde_json::json!({
        "schemaVersion": maestro_runtime::DELEGATION_PROJECTION_SCHEMA_VERSION,
        "task": task,
    }))
}

fn tool_result_for_record(record: SubagentRecord) -> ToolResult {
    let details = record_details(&record);
    let status = status_label(record.status);
    let output = record
        .result
        .as_ref()
        .map(|result| result.output.trim().to_string())
        .filter(|output| !output.is_empty())
        .unwrap_or_else(|| format!("Subagent {} is {status}", record.id));
    let output = if record.backend == SubagentBackend::Native && record.status.is_terminal() {
        format!(
            "{}\n\nSaved child output:\n{}",
            crate::subagents::handoff::notification(&output),
            output
        )
    } else {
        output
    };
    if matches!(
        record.status,
        SubagentStatus::Failed
            | SubagentStatus::Cancelled
            | SubagentStatus::TimedOut
            | SubagentStatus::Interrupted
    ) {
        ToolResult::failure(crate::subagents::handoff::parent_output(
            record
                .error
                .clone()
                .unwrap_or_else(|| format!("Subagent {} failed", record.id)),
        ))
        .with_details(details)
    } else {
        ToolResult::success(crate::subagents::handoff::parent_output(output)).with_details(details)
    }
}

/// Why a wait slice stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitWake {
    /// The slice ran to its end.
    Elapsed,
    /// The parent turn was cancelled.
    Cancelled,
    /// The user sent a message while the wait was blocking.
    Steered,
}

/// Sleep for one slice, but stop early on cancellation or a queued steer.
///
/// The sleep races cancellation and user injection so a blocking wait releases
/// promptly. An injection that is already pending returns without sleeping.
async fn sleep_or_release(
    slice: Duration,
    cancel: Option<&CancellationToken>,
    steer: Option<&SteerSignal>,
) -> WaitWake {
    if steer.is_some_and(SteerSignal::is_pending) {
        return WaitWake::Steered;
    }
    tokio::select! {
        biased;
        () = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        } => WaitWake::Cancelled,
        () = async {
            match steer {
                Some(signal) => signal.pending().await,
                None => std::future::pending().await,
            }
        } => WaitWake::Steered,
        () = tokio::time::sleep(slice) => WaitWake::Elapsed,
    }
}

/// The result returned when a user message ends a blocking wait.
///
/// This is a success, not a failure: nothing went wrong and the subagent is
/// untouched. The text says so explicitly, because a model that reads a
/// terminated wait as a terminated subagent will report work as abandoned
/// that is still running.
fn steer_released_result(record: &SubagentRecord) -> ToolResult {
    let mut details = record_details(record);
    if let Some(object) = details.as_object_mut() {
        object.insert(
            "status".to_string(),
            serde_json::Value::String("released_by_steering".to_string()),
        );
        object.insert(
            "releasedBySteering".to_string(),
            serde_json::Value::Bool(true),
        );
        object.insert(
            "subagentStatus".to_string(),
            serde_json::Value::String(status_label(record.status).to_string()),
        );
    }
    ToolResult::success(format!(
        "Stopped waiting on subagent {} because the user sent a new message. The subagent is \
         still {} and its completion will still be delivered. Read the new message and act on \
         it; do not wait again.",
        record.id,
        status_label(record.status),
    ))
    .with_details(details)
}

fn cancelled_result(message: &str) -> ToolResult {
    ToolResult::failure(message).with_details(serde_json::json!({
        "cancelled": true,
        "retryable": true
    }))
}

fn child_event_to_headless(event: &FromAgent, session_id: &str) -> Option<FromAgentMessage> {
    match event {
        FromAgent::ConversationSnapshot {
            protocol_version,
            messages,
            processed_queue_ids,
        } => Some(FromAgentMessage::ConversationSnapshot {
            protocol_version: protocol_version.clone(),
            messages: messages.clone(),
            processed_queue_ids: processed_queue_ids.clone(),
        }),
        FromAgent::ManagedGatewayReceipt {
            request_id,
            record_id,
            lineage_id,
            record_status,
            ..
        } => Some(FromAgentMessage::ManagedGatewayReceipt {
            request_id: request_id.clone(),
            record_id: record_id.clone(),
            lineage_id: lineage_id.clone(),
            record_status: record_status.clone(),
            prompt_experiment: None,
        }),
        FromAgent::Ready { model, provider } => Some(FromAgentMessage::Ready {
            protocol_version: Some(crate::headless::HEADLESS_PROTOCOL_VERSION.to_string()),
            model: model.clone(),
            provider: provider.clone(),
            session_id: Some(session_id.to_string()),
        }),
        FromAgent::ResponseStart { response_id } => Some(FromAgentMessage::ResponseStart {
            response_id: response_id.clone(),
        }),
        FromAgent::ResponseChunk {
            response_id,
            content,
            is_thinking,
        } if !is_thinking => Some(FromAgentMessage::ResponseChunk {
            response_id: response_id.clone(),
            content: content.clone(),
            is_thinking: false,
        }),
        FromAgent::ResponseEnd { response_id, usage } => Some(FromAgentMessage::ResponseEnd {
            response_id: response_id.clone(),
            usage: usage.as_ref().map(convert_usage),
            tools_summary: None,
            duration_ms: None,
            ttft_ms: None,
        }),
        FromAgent::TurnCompleted {
            response_id,
            coding_completion,
            coding_child_records,
        } => Some(FromAgentMessage::TurnCompleted {
            response_id: response_id.clone(),
            coding_completion: coding_completion.clone(),
            coding_child_records: coding_child_records.clone(),
        }),
        FromAgent::TurnInterrupted {
            response_id,
            reason,
        } => Some(FromAgentMessage::TurnInterrupted {
            response_id: response_id.clone(),
            reason: reason.clone(),
        }),
        FromAgent::CodexSessionState {
            state,
            thread_id,
            profile,
        } => Some(FromAgentMessage::CodexSessionState {
            state: state.clone(),
            thread_id: thread_id.clone(),
            profile: profile.clone(),
        }),
        FromAgent::CodexTurnState {
            state,
            thread_id,
            turn_id,
        } => Some(FromAgentMessage::CodexTurnState {
            state: state.clone(),
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        }),
        FromAgent::CodexUsageState { source, usage } => Some(FromAgentMessage::CodexUsageState {
            source: source.clone(),
            usage: usage.as_ref().map(convert_usage),
        }),
        FromAgent::CodexCompatibility {
            protocol_version,
            resume,
            steering,
        } => Some(FromAgentMessage::CodexCompatibility {
            protocol_version: protocol_version.clone(),
            resume: *resume,
            steering: *steering,
        }),
        FromAgent::ToolCall {
            call_id,
            tool,
            args,
            requires_approval,
            ..
        } => Some(FromAgentMessage::ToolCall {
            call_id: call_id.clone(),
            tool_execution_id: None,
            tool: tool.clone(),
            args: args.clone(),
            requires_approval: *requires_approval,
        }),
        FromAgent::ToolStart { call_id } => Some(FromAgentMessage::ToolStart {
            call_id: call_id.clone(),
        }),
        FromAgent::ToolOutput { call_id, content } => Some(FromAgentMessage::ToolOutput {
            call_id: call_id.clone(),
            content: content.clone(),
        }),
        FromAgent::ToolEnd {
            call_id,
            success,
            result,
            receipt,
        } => Some(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
            call_id: call_id.clone(),
            tool_execution_id: None,
            success: *success,
            tool: None,
            details: result.as_ref().and_then(|result| result.details.clone()),
            receipt: receipt.clone(),
        })),
        FromAgent::Error {
            message,
            fatal,
            terminal,
            retryable,
        } => Some(FromAgentMessage::Error {
            request_id: None,
            message: message.clone(),
            fatal: *fatal,
            terminal: *terminal,
            error_type: Some(crate::headless_server::headless_error_type(
                *fatal, *terminal, *retryable,
            )),
        }),
        FromAgent::ProviderError { kind, message } => Some(FromAgentMessage::ProviderError {
            kind: *kind,
            message: message.clone(),
        }),
        FromAgent::CodexNativeDecision { method, decision } => Some(FromAgentMessage::Status {
            message: format!("Codex approval receipt: method={method} decision={decision}"),
        }),
        FromAgent::CodexTransportReceipt {
            provider,
            transport,
            outcome,
            transport_restarted,
            auth_resumed,
            cancellation_requested,
        } => Some(FromAgentMessage::Status {
            message: format!(
                "Codex transport receipt: provider={provider} transport={transport} outcome={outcome} restarted={transport_restarted} auth_resumed={auth_resumed} cancellation_requested={cancellation_requested}"
            ),
        }),
        FromAgent::Status { message } => Some(FromAgentMessage::Status {
            message: message.clone(),
        }),
        FromAgent::Compaction {
            summary,
            first_kept_entry_index,
            tokens_before,
            auto,
            custom_instructions,
            continuation,
            timestamp,
        } => Some(FromAgentMessage::Compaction {
            summary: summary.clone(),
            first_kept_entry_index: *first_kept_entry_index,
            tokens_before: *tokens_before,
            auto: *auto,
            custom_instructions: custom_instructions.clone(),
            continuation: continuation.clone(),
            timestamp: timestamp.clone(),
        }),
        FromAgent::SessionInfo {
            session_id: child_session_id,
            cwd,
            git_branch,
        } => Some(FromAgentMessage::SessionInfo {
            session_id: child_session_id
                .clone()
                .or_else(|| Some(session_id.to_string())),
            cwd: cwd.clone(),
            git_branch: git_branch.clone(),
        }),
        _ => None,
    }
}

fn persist_child_event(
    recorder: &mut SessionRecorder,
    event: &FromAgent,
    session_id: &str,
    credential_vault: &CredentialVault,
    snapshot_attempt: u32,
) -> Result<bool, String> {
    let processed_queue_ids = match event {
        FromAgent::ConversationSnapshot {
            processed_queue_ids,
            ..
        } => processed_queue_ids.as_slice(),
        _ => &[],
    };
    let Some(message) = child_event_to_headless(event, session_id) else {
        return Ok(false);
    };
    let message = vault_headless_message(&message, credential_vault)
        .map_err(|error| format!("vault child event: {error}"))?;
    recorder
        .record_received_preserving_credential_references_with_snapshot_metadata(
            &message,
            Some(snapshot_attempt),
            processed_queue_ids,
        )
        .map_err(|error| format!("persist child event: {error}"))?;
    Ok(matches!(event, FromAgent::ConversationSnapshot { .. }))
}

fn drain_child_events(
    recorder: &mut SessionRecorder,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<FromAgent>,
    session_id: &str,
    credential_vault: &CredentialVault,
    snapshot_attempt: u32,
) -> Result<(), String> {
    while let Ok(event) = events.try_recv() {
        persist_child_event(
            recorder,
            &event,
            session_id,
            credential_vault,
            snapshot_attempt,
        )?;
    }
    Ok(())
}

fn vault_headless_message(
    message: &FromAgentMessage,
    credential_vault: &CredentialVault,
) -> Result<FromAgentMessage, String> {
    let value = serde_json::to_value(message).map_err(|error| error.to_string())?;
    let vaulted = credential_vault.vault_in_json(&value);
    serde_json::from_value(vaulted).map_err(|error| error.to_string())
}

fn convert_usage(usage: &crate::agent::TokenUsage) -> crate::headless::TokenUsage {
    crate::headless::TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost: usage.cost,
        total_tokens: None,
        model_id: None,
        provider: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::future::BoxFuture;

    use super::*;
    use crate::mcp::{McpContent, McpToolResult};
    use crate::tools::orb_delegation::OrbToolCaller;

    struct OwnerBindingCaller {
        calls: Mutex<Vec<String>>,
        lifecycle_state: String,
        available_commands: Vec<String>,
        delay: Duration,
    }

    impl OwnerBindingCaller {
        fn new() -> Arc<Self> {
            Self::with_status("active", &[])
        }

        fn with_status(lifecycle_state: &str, available_commands: &[&str]) -> Arc<Self> {
            Self::with_status_delay(lifecycle_state, available_commands, Duration::ZERO)
        }

        fn with_status_delay(
            lifecycle_state: &str,
            available_commands: &[&str],
            delay: Duration,
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                lifecycle_state: lifecycle_state.to_string(),
                available_commands: available_commands
                    .iter()
                    .map(|command| (*command).to_string())
                    .collect(),
                delay,
            })
        }
    }

    impl OrbToolCaller for OwnerBindingCaller {
        fn call<'a>(
            &'a self,
            tool: &'a str,
            _arguments: serde_json::Value,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<McpToolResult, String>> {
            self.calls
                .lock()
                .expect("owner caller calls lock")
                .push(tool.to_string());
            let value = match tool {
                "orb_task_status" => serde_json::json!({
                    "lifecycle_state": self.lifecycle_state,
                    "available_commands": self.available_commands,
                }),
                "orb_resume_task" => serde_json::json!({
                    "detail": {
                        "lifecycle_state": "active",
                        "available_commands": []
                    }
                }),
                "orb_send_message" => serde_json::json!({"accepted": true}),
                "orb_get_thread" => serde_json::json!({
                    "summary": {},
                    "recent_messages": []
                }),
                "orb_cancel_task" => serde_json::json!({
                    "lifecycle_state": "cancelled",
                    "available_commands": []
                }),
                _ => serde_json::json!({}),
            };
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                Ok(McpToolResult {
                    content: vec![McpContent::Text {
                        text: serde_json::to_string(&value).expect("owner caller response"),
                    }],
                    is_error: false,
                })
            })
        }
    }

    fn owner_binding(
        organization_id: &str,
        workspace_id: &str,
        connection_ref: &str,
        managed_generation: u64,
    ) -> HostedOrbOwnerBinding {
        HostedOrbOwnerBinding {
            organization_id: organization_id.to_string(),
            workspace_id: workspace_id.to_string(),
            connection_ref: connection_ref.to_string(),
            managed_generation,
        }
    }

    #[test]
    fn specialist_selects_the_existing_profile_and_rejects_ambiguity() {
        let request =
            parse_spawn_request(&serde_json::json!({"task":"inspect", "specialist":"product"}))
                .unwrap();
        assert_eq!(request.profile.as_deref(), Some("product"));
        for extra in [
            serde_json::json!({"task":"inspect", "specialist":"product", "profile":"security"}),
            serde_json::json!({"task":"inspect", "specialist":false}),
            serde_json::json!({"task":"inspect", "specialist":"product", "backend":"computer"}),
        ] {
            assert!(parse_spawn_request(&extra).is_err());
        }
    }

    #[test]
    fn orb_admitting_record_binds_owner_before_remote_thread_exists() {
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 7);
        let orb = admitting_orb_subagent_ref(
            OrbDelegationConfig::default(),
            binding.clone(),
            "start-a".to_string(),
        );
        assert!(orb.thread_id.is_empty());
        assert_eq!(orb.organization_id.as_deref(), Some("org-a"));
        assert_eq!(orb.workspace_id.as_deref(), Some("workspace-a"));
        assert_eq!(orb.connection_ref.as_deref(), Some("connection-a"));
        assert_eq!(orb.managed_generation, Some(7));
        assert_eq!(orb.start_idempotency_key, "start-a");
        assert_eq!(orb.lifecycle_state.as_deref(), Some("admitting"));
    }

    #[tokio::test]
    async fn orb_spawn_persists_owner_bound_admitting_record_before_dispatch() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        manager.set_parent_scope_id("session-a".to_string());
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::new();
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller,
                binding.clone(),
            ))
            .await;

        let result = manager
            .spawn(
                &serde_json::json!({
                    "task": "inspect the hosted workspace",
                    "backend": "orb",
                    "run_in_background": true,
                    "orb": {
                        "project": "project-a",
                        "repository": "https://github.com/evalops/example"
                    }
                }),
                "parent-call",
                None,
                CredentialVault::new(),
                None,
            )
            .await;
        assert!(!result.success, "the fixture intentionally rejects launch");
        let id = result
            .details
            .as_ref()
            .and_then(|details| details.get("subagentId"))
            .and_then(serde_json::Value::as_str)
            .expect("failed Computer launch keeps its durable id");
        let record = manager.load_record(id).expect("persisted Computer record");
        let orb = record.orb.expect("owner-bound admitting Computer record");
        assert!(orb.thread_id.is_empty());
        assert_eq!(orb.organization_id.as_deref(), Some("org-a"));
        assert_eq!(orb.workspace_id.as_deref(), Some("workspace-a"));
        assert_eq!(orb.connection_ref.as_deref(), Some("connection-a"));
        assert_eq!(orb.managed_generation, Some(binding.managed_generation));
        assert_eq!(orb.lifecycle_state.as_deref(), Some("admitting"));
        let retry = manager
            .spawn(
                &serde_json::json!({
                    "task": "inspect the hosted workspace",
                    "backend": "orb",
                    "run_in_background": true,
                    "orb": {
                        "project": "project-a",
                        "repository": "https://github.com/evalops/example"
                    }
                }),
                "parent-call",
                None,
                CredentialVault::new(),
                None,
            )
            .await;
        assert!(
            !retry.success,
            "the fixture intentionally rejects the retry"
        );
        let retry_id = retry
            .details
            .as_ref()
            .and_then(|details| details.get("subagentId"))
            .and_then(serde_json::Value::as_str)
            .expect("failed Computer retry keeps its durable id");
        assert_ne!(id, retry_id, "each local attempt keeps a distinct record");
        let retry_orb = manager
            .load_record(retry_id)
            .expect("persisted Computer retry record")
            .orb
            .expect("owner-bound retry record");
        assert_eq!(
            orb.start_idempotency_key, retry_orb.start_idempotency_key,
            "a retry of the same parent call must replay the atomic hosted launch"
        );
        assert_eq!(
            orb.start_idempotency_key,
            deterministic_idempotency_key("start", &["session-a", "parent-call"])
        );
    }

    #[test]
    fn parent_corrections_survive_worker_record_reload_and_legacy_records() {
        let root = tempfile::tempdir().unwrap();
        let manager = SubagentManager::with_root(root.path().into(), root.path().join("records"));
        let mut record = control_receipt_record(root.path());
        record.parent_requests = vec![
            "Build the API; do not publish".into(),
            "Keep the API; fix only the CLI".into(),
        ];
        manager.write_record(&record).unwrap();
        let restored = manager.load_record(&record.id).unwrap();
        assert_eq!(restored.parent_requests, record.parent_requests);
        let mut legacy = serde_json::to_value(&restored).unwrap();
        legacy.as_object_mut().unwrap().remove("parent_requests");
        assert!(
            serde_json::from_value::<SubagentRecord>(legacy)
                .unwrap()
                .parent_requests
                .is_empty()
        );
        let vault = CredentialVault::new();
        let secret = "sk-".to_owned() + &"a".repeat(48);
        manager.set_parent_requests(vec![vault.vault_in_text(&secret)]);
        let snapshot = manager.parent_request_snapshot().unwrap();
        assert!(!snapshot.join("").contains(&secret));
        assert!(!snapshot.join("").contains("credential:"));
        manager.set_parent_requests(record.parent_requests);
        manager.set_parent_scope_id("another-session".into());
        assert!(manager.parent_requests.lock().unwrap().is_empty());
        assert_eq!(manager.worker_activity().unwrap(), (0, 0));
    }

    #[test]
    fn work_type_routes_effort_without_granting_a_writing_role() {
        for (work, difficulty) in [
            ("lookup", TaskDifficulty::Light),
            ("implementation", TaskDifficulty::Medium),
            ("diagnosis", TaskDifficulty::Heavy),
        ] {
            let request = parse_spawn_request(
                &serde_json::json!({"task":"inspect", "role":"explore", "work_type":work}),
            )
            .unwrap();
            assert_eq!(request.difficulty, difficulty);
            assert!(!request.role.can_mutate());
        }
        let explicit = parse_spawn_request(
            &serde_json::json!({"task":"inspect", "work_type":"diagnosis", "difficulty":"light"}),
        )
        .unwrap();
        assert_eq!(explicit.difficulty, TaskDifficulty::Light);
        assert!(
            parse_spawn_request(&serde_json::json!({"task":"inspect", "work_type":"guess"}))
                .is_err()
        );
    }

    #[test]
    fn native_worker_difficulty_and_effort_are_strict_and_durable() {
        let request = parse_spawn_request(&serde_json::json!({
            "task": "inspect", "backend": "native", "role": "explore",
            "difficulty": "heavy", "thinking": "max"
        }))
        .unwrap();
        assert_eq!(request.role, SubagentRole::Explore);
        assert_eq!(request.difficulty, TaskDifficulty::Heavy);
        assert_eq!(request.thinking, Some(ThinkingLevel::Max));
        for extra in [
            serde_json::json!({"difficulty": 4}),
            serde_json::json!({"difficulty": "impossible"}),
            serde_json::json!({"thinking": "impossible"}),
            serde_json::json!({"backend": "orb", "thinking": "high"}),
        ] {
            let mut args = serde_json::json!({"task": "inspect", "backend": "native"});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(parse_spawn_request(&args).is_err());
        }
        let root = tempfile::tempdir().unwrap();
        let mut record = control_receipt_record(root.path());
        record.model = Some("openai/gpt-5.5".into());
        record.thinking = request.thinking;
        record.difficulty = request.difficulty;
        let mut json = serde_json::to_value(&record).unwrap();
        let restored: SubagentRecord = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(restored.model, record.model);
        assert_eq!(restored.thinking, Some(ThinkingLevel::Max));
        assert_eq!(restored.difficulty, TaskDifficulty::Heavy);
        json.as_object_mut().unwrap().remove("thinking");
        json.as_object_mut().unwrap().remove("difficulty");
        let legacy: SubagentRecord = serde_json::from_value(json).unwrap();
        assert_eq!(
            legacy.thinking.unwrap_or(ThinkingLevel::Off),
            ThinkingLevel::Off
        );
        assert_eq!(legacy.difficulty, TaskDifficulty::Medium);
    }

    struct RefusingChildFactory {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ChildAgentFactory for RefusingChildFactory {
        fn spawn(
            &self,
            request: ChildLaunchRequest,
        ) -> anyhow::Result<(NativeAgent, mpsc::UnboundedReceiver<FromAgent>)> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(!request.mailbox_identity.is_empty());
            assert!(
                SUBAGENT_TOOL_NAMES
                    .iter()
                    .all(|name| !request.allowed_tools.contains(*name))
            );
            anyhow::bail!("fixture child construction refused")
        }
    }

    async fn run_refusing_child(cancelled: bool) -> (SubagentRecord, usize, SubagentRecord) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("records");
        let factory = Arc::new(RefusingChildFactory {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let manager = SubagentManager::with_root(temp.path().to_path_buf(), root)
            .with_child_agent_factory(factory.clone());
        let mut record = control_receipt_record(temp.path());
        record.attempt = 0;
        record.status = SubagentStatus::Queued;
        record.started_at_ms = None;
        record.model = Some("fixture-provider/fixture-model".to_string());
        let id = record.id.clone();
        drop(SessionRecorder::with_id(SubagentManager::session_dir(&record), &id).unwrap());
        let token = CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        let (_control_tx, control_rx) = mpsc::channel(1);
        let parent = CredentialVault::new();
        let result = manager
            .run_child(
                record,
                ChildRun {
                    prompt: "Do not call a provider".to_string(),
                    history: None,
                    sandbox_policy: None,
                    token,
                    control_rx,
                },
                ChildLaunch {
                    lease: None,
                    credential_vault: CredentialVault::new(),
                    parent_credential_generation: parent.generation(),
                    parent_credential_vault: parent,
                    parent_cancel: None,
                },
            )
            .await
            .unwrap();
        let persisted = manager.load_record(&id).unwrap();
        (
            result,
            factory.calls.load(std::sync::atomic::Ordering::SeqCst),
            persisted,
        )
    }

    fn run_isolated_refusing_child(cancelled: bool) -> (SubagentRecord, usize, SubagentRecord) {
        let home = tempfile::tempdir().unwrap();
        // Only called in a dedicated test process, before its runtime starts.
        // Child construction must not read a developer's credentials or contact
        // managed setup while testing the injected factory.
        for name in crate::credential_mode::TEST_IDENTITY_ENV_VARS {
            std::env::remove_var(name);
        }
        std::env::set_var("HOME", home.path());
        std::env::set_var("MAESTRO_HOME", home.path());
        std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
        assert!(matches!(
            crate::credential_mode::detect().unwrap(),
            crate::credential_mode::DetectedMode::Byok
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_refusing_child(cancelled))
    }

    #[test]
    fn child_factory_failure_is_persisted_by_the_lifecycle_owner() {
        if crate::config::test_reexec_for_process_isolation() {
            return;
        }
        let (record, calls, persisted) = run_isolated_refusing_child(false);
        assert_eq!(calls, 1);
        assert_eq!(record.status, SubagentStatus::Failed);
        assert_eq!(persisted.status, SubagentStatus::Failed);
        assert!(
            persisted
                .error
                .unwrap()
                .contains("fixture child construction refused")
        );
        assert!(persisted.lifecycle_notification_published);
    }

    #[test]
    fn queued_child_cancellation_never_calls_the_factory() {
        if crate::config::test_reexec_for_process_isolation() {
            return;
        }
        let (record, calls, persisted) = run_isolated_refusing_child(true);
        assert_eq!(calls, 0);
        assert_eq!(record.status, SubagentStatus::Cancelled);
        assert_eq!(persisted.status, SubagentStatus::Cancelled);
        assert!(persisted.lifecycle_notification_published);
    }

    fn control_receipt_record(root: &Path) -> SubagentRecord {
        SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "parent".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.join("session")),
            status: SubagentStatus::Running,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        }
    }

    #[test]
    fn coding_acceptance_uses_sealed_execution_output_despite_record_file_edits() {
        let root = tempfile::tempdir().unwrap();
        let manager = SubagentManager::with_root(root.path().into(), root.path().join("records"));
        let record = control_receipt_record(root.path());
        let id = record.id.clone();
        manager.write_record(&record).unwrap();
        assert!(manager.coding_validator_record(&id).is_err());
        manager
            .coding_validator_receipts
            .lock()
            .unwrap()
            .insert(id.clone(), None);
        let vault = CredentialVault::new();
        let scope = ParentCredentialScope {
            vault: &vault,
            generation: vault.generation(),
        };
        let mut terminal = manager
            .finish_record(
                record,
                SubagentStatus::Completed,
                Some(SubagentResult {
                    output: "actual failed assertions".into(),
                    files_modified: vec![],
                }),
                None,
                &CredentialVault::new(),
                &scope,
            )
            .unwrap();
        terminal.result.as_mut().unwrap().output = "fabricated passing assertions".into();
        manager.write_record(&terminal).unwrap();
        assert_eq!(
            manager
                .coding_validator_record(&id)
                .unwrap()
                .result
                .unwrap()
                .output,
            "actual failed assertions"
        );
        manager.seal_coding_validator_record(&terminal);
        assert_eq!(
            manager
                .coding_validator_record(&id)
                .unwrap()
                .result
                .unwrap()
                .output,
            "actual failed assertions"
        );
        manager.clear_coding_validator_records();
        assert!(manager.coding_validator_record(&id).is_err());
    }

    fn orb_resume_record(root: &Path, binding: &HostedOrbOwnerBinding) -> SubagentRecord {
        let mut record = control_receipt_record(root);
        record.backend = SubagentBackend::Orb;
        record.status = SubagentStatus::Failed;
        record.orb = Some(OrbSubagentRef {
            thread_id: "thread-resume".to_string(),
            receipt_id: Some("receipt-resume".to_string()),
            start_idempotency_key: "start-resume".to_string(),
            config: OrbDelegationConfig::default(),
            organization_id: Some(binding.organization_id.clone()),
            workspace_id: Some(binding.workspace_id.clone()),
            connection_ref: Some(binding.connection_ref.clone()),
            managed_generation: Some(binding.managed_generation),
            lifecycle_state: Some("failed".to_string()),
            available_commands: Vec::new(),
        });
        record
    }

    fn running_orb_record(root: &Path, binding: &HostedOrbOwnerBinding) -> SubagentRecord {
        let mut record = orb_resume_record(root, binding);
        record.status = SubagentStatus::Running;
        record.error = None;
        if let Some(orb) = record.orb.as_mut() {
            orb.lifecycle_state = Some("active".to_string());
        }
        record
    }

    #[tokio::test]
    async fn orb_wait_zero_timeout_uses_cached_record_without_remote_refresh() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::new();
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;
        let record = running_orb_record(root.path(), &binding);
        manager
            .write_record(&record)
            .expect("persist Computer record");

        let result = manager
            .wait(
                &serde_json::json!({
                    "subagent_id": record.id,
                    "timeout_ms": 0,
                }),
                None,
            )
            .await;

        assert!(result.success, "cached running record is a valid snapshot");
        assert!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .is_empty(),
            "an immediate wait must not make a remote status or collect call"
        );
    }

    #[tokio::test]
    async fn orb_wait_bounds_remote_refresh_by_remaining_timeout() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::with_status_delay("active", &[], Duration::from_secs(5));
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;
        let record = running_orb_record(root.path(), &binding);
        manager
            .write_record(&record)
            .expect("persist Computer record");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            manager.wait(
                &serde_json::json!({
                    "subagent_id": record.id,
                    "timeout_ms": 25,
                }),
                None,
            ),
        )
        .await
        .expect("wait deadline must bound a slow remote refresh");

        assert!(result.success, "the cached running snapshot remains valid");
        assert_eq!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .as_slice(),
            ["orb_task_status"],
            "a timed-out status refresh must not proceed to collect"
        );
    }

    #[tokio::test]
    async fn orb_resume_rejects_all_terminal_lifecycle_aliases_without_sending() {
        for lifecycle_state in [
            "succeeded",
            "success",
            "completed",
            "cancelled",
            "canceled",
            "failed",
            "rejected",
            "timed_out",
            "interrupted",
        ] {
            let root = tempfile::tempdir().expect("temp root");
            let manager =
                SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
            let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
            let caller = OwnerBindingCaller::with_status(lifecycle_state, &[]);
            manager
                .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                    caller.clone(),
                    binding.clone(),
                ))
                .await;

            let result = manager
                .resume_orb(
                    orb_resume_record(root.path(), &binding),
                    "continue".to_string(),
                    &serde_json::json!({}),
                    "parent-call",
                    None,
                )
                .await;

            assert!(!result.success, "{lifecycle_state} must be terminal");
            assert_eq!(
                caller
                    .calls
                    .lock()
                    .expect("owner caller calls lock")
                    .as_slice(),
                ["orb_task_status"],
                "{lifecycle_state} must not resume or send a follow-up"
            );
        }
    }

    #[tokio::test]
    async fn orb_console_followup_rejects_failed_without_an_advertised_retry() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::with_status("failed", &[]);
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;
        let record = orb_resume_record(root.path(), &binding);
        manager
            .write_record(&record)
            .expect("persist Computer record");

        let result = manager.orb_console_followup(&record.id, "continue").await;

        assert!(!result.success, "failed without retry must be terminal");
        assert_eq!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .as_slice(),
            ["orb_task_status"],
            "terminal follow-up must stop after status"
        );
    }

    #[tokio::test]
    async fn orb_resume_uses_an_advertised_retry_before_sending_to_a_failed_task() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::with_status("failed", &["retry"]);
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;

        let result = manager
            .resume_orb(
                orb_resume_record(root.path(), &binding),
                "continue".to_string(),
                &serde_json::json!({}),
                "parent-call",
                None,
            )
            .await;

        assert!(
            result.success,
            "advertised retry should permit the follow-up"
        );
        assert_eq!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .as_slice(),
            ["orb_task_status", "orb_resume_task", "orb_send_message"]
        );
    }

    #[tokio::test]
    async fn orb_resume_accepts_the_raw_prefixed_resume_control() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::with_status("failed", &["orb_resume_task"]);
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;

        let result = manager
            .resume_orb(
                orb_resume_record(root.path(), &binding),
                "continue".to_string(),
                &serde_json::json!({}),
                "parent-call",
                None,
            )
            .await;

        assert!(result.success, "raw provider control should permit resume");
        assert_eq!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .as_slice(),
            ["orb_task_status", "orb_resume_task", "orb_send_message"]
        );
    }

    #[tokio::test]
    async fn orb_console_resume_accepts_the_raw_prefixed_resume_control() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::with_status("failed", &["orb_resume_task"]);
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;
        let record = orb_resume_record(root.path(), &binding);
        manager
            .write_record(&record)
            .expect("persist Computer record");

        let result = manager.orb_console_resume(&record.id).await;

        assert!(result.success, "raw provider control should permit resume");
        assert_eq!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .as_slice(),
            ["orb_task_status", "orb_resume_task"]
        );
    }

    #[tokio::test]
    async fn orb_console_followup_accepts_the_raw_prefixed_resume_control() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let caller = OwnerBindingCaller::with_status("failed", &["orb_resume_task"]);
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller.clone(),
                binding.clone(),
            ))
            .await;
        let record = orb_resume_record(root.path(), &binding);
        manager
            .write_record(&record)
            .expect("persist Computer record");

        let result = manager.orb_console_followup(&record.id, "continue").await;

        assert!(
            result.success,
            "raw provider control should permit follow-up"
        );
        assert_eq!(
            caller
                .calls
                .lock()
                .expect("owner caller calls lock")
                .as_slice(),
            ["orb_task_status", "orb_resume_task", "orb_send_message"]
        );
    }

    #[tokio::test]
    async fn orb_console_provider_failure_retains_identity_and_redacts_details() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let mut record = control_receipt_record(root.path());
        record.id = "123e4567-e89b-12d3-a456-426614174000".to_string();
        record.backend = SubagentBackend::Orb;
        record.task = "inspect the hosted workspace".to_string();
        record.orb = Some(OrbSubagentRef {
            thread_id: "thread-1".to_string(),
            receipt_id: Some("receipt-1".to_string()),
            start_idempotency_key: "start-1".to_string(),
            config: OrbDelegationConfig::default(),
            organization_id: None,
            workspace_id: None,
            connection_ref: None,
            managed_generation: None,
            lifecycle_state: Some("active".to_string()),
            available_commands: vec!["orb_pause_task".to_string()],
        });
        manager
            .write_record(&record)
            .expect("persist durable record");

        let result = manager
            .orb_console(
                OrbConsoleAction::Status {
                    id: record.id.clone(),
                },
                Some("provider secret and mcp endpoint".to_string()),
            )
            .await;
        assert!(!result.success);
        let details = result.details.expect("typed unavailable projection");
        let task = &details["task"];
        assert_eq!(task["id"], "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(task["threadId"], "thread-1");
        assert_eq!(task["receiptId"], "receipt-1");
        assert_eq!(task["event"]["lifecycleState"], "unavailable");
        assert_eq!(task["recoverable"], true);
        let encoded = serde_json::to_string(&details).expect("details serialize");
        assert!(!encoded.contains("provider secret"));
        assert!(!encoded.contains("mcp endpoint"));
        assert!(!encoded.contains("orb_pause_task"));

        let persisted = manager
            .load_record("123e4567-e89b-12d3-a456-426614174000")
            .expect("durable record remains");
        assert_eq!(
            persisted.orb.as_ref().expect("Computer identity").thread_id,
            "thread-1"
        );
    }

    #[tokio::test]
    async fn orb_owner_binding_switch_fails_closed_before_remote_status_or_cancel() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let owner_a = owner_binding("org-a", "workspace-a", "connection-a", 1);
        let owner_b = owner_binding("org-b", "workspace-b", "connection-b", 2);
        let caller_a = OwnerBindingCaller::new();
        let caller_b = OwnerBindingCaller::new();
        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller_a.clone(),
                owner_a.clone(),
            ))
            .await;

        let mut record = control_receipt_record(root.path());
        record.id = "123e4567-e89b-12d3-a456-426614174000".to_string();
        record.backend = SubagentBackend::Orb;
        record.orb = Some(OrbSubagentRef {
            thread_id: "thread-a".to_string(),
            receipt_id: Some("receipt-a".to_string()),
            start_idempotency_key: "start-a".to_string(),
            config: OrbDelegationConfig::default(),
            organization_id: Some(owner_a.organization_id.clone()),
            workspace_id: Some(owner_a.workspace_id.clone()),
            connection_ref: Some(owner_a.connection_ref.clone()),
            managed_generation: Some(owner_a.managed_generation),
            lifecycle_state: Some("active".to_string()),
            available_commands: vec!["orb_cancel_task".to_string()],
        });
        manager
            .write_record(&record)
            .expect("persist owner-bound record");

        let status = manager
            .get_remote(&serde_json::json!({"subagent_id": record.id.clone()}), None)
            .await;
        assert!(
            status.success,
            "matching owner should permit status: {status:?}"
        );
        assert_eq!(
            caller_a
                .calls
                .lock()
                .expect("owner A calls lock")
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["orb_task_status", "orb_get_thread"]
        );

        manager
            .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
                caller_b.clone(),
                owner_b,
            ))
            .await;
        let switched_status = manager
            .get_remote(&serde_json::json!({"subagent_id": record.id.clone()}), None)
            .await;
        assert!(!switched_status.success);
        assert!(
            switched_status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("owner binding"))
        );
        assert!(
            caller_b
                .calls
                .lock()
                .expect("owner B calls lock")
                .is_empty()
        );

        let switched_cancel = manager
            .cancel(&serde_json::json!({"subagent_id": record.id.clone()}), None)
            .await;
        assert!(!switched_cancel.success);
        assert!(
            switched_cancel
                .error
                .as_deref()
                .is_some_and(|error| error.contains("owner binding"))
        );
        assert!(
            caller_b
                .calls
                .lock()
                .expect("owner B calls lock")
                .is_empty()
        );

        let mut legacy_record = record;
        legacy_record.id = "123e4567-e89b-12d3-a456-426614174001".to_string();
        let legacy_orb = legacy_record
            .orb
            .as_mut()
            .expect("legacy Computer identity");
        legacy_orb.organization_id = None;
        legacy_orb.workspace_id = None;
        legacy_orb.connection_ref = None;
        legacy_orb.managed_generation = None;
        manager
            .write_record(&legacy_record)
            .expect("persist legacy record");
        let legacy_result = manager
            .get_remote(
                &serde_json::json!({"subagent_id": legacy_record.id.clone()}),
                None,
            )
            .await;
        assert!(!legacy_result.success);
        assert!(
            legacy_result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no durable owner binding"))
        );
        assert!(
            caller_b
                .calls
                .lock()
                .expect("owner B calls lock")
                .is_empty()
        );

        let mut unbound_record = control_receipt_record(root.path());
        unbound_record.id = "123e4567-e89b-12d3-a456-426614174003".to_string();
        unbound_record.backend = SubagentBackend::Orb;
        manager
            .write_record(&unbound_record)
            .expect("persist pre-owner-binding record");
        let unbound_result = manager
            .get_remote(
                &serde_json::json!({"subagent_id": unbound_record.id.clone()}),
                None,
            )
            .await;
        assert!(!unbound_result.success);
        assert!(
            unbound_result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no durable owner binding"))
        );
        assert!(
            caller_b
                .calls
                .lock()
                .expect("owner B calls lock")
                .is_empty()
        );

        let mut admitting_record = legacy_record;
        admitting_record.id = "123e4567-e89b-12d3-a456-426614174002".to_string();
        let admitting_orb = admitting_record
            .orb
            .as_mut()
            .expect("admitting Computer identity");
        admitting_orb.thread_id.clear();
        admitting_orb.organization_id = Some("org-a".to_string());
        admitting_orb.workspace_id = Some("workspace-a".to_string());
        admitting_orb.connection_ref = Some("connection-a".to_string());
        admitting_orb.managed_generation = Some(1);
        manager
            .write_record(&admitting_record)
            .expect("persist admitting record");
        let admitting_result = manager
            .get_remote(
                &serde_json::json!({"subagent_id": admitting_record.id.clone()}),
                None,
            )
            .await;
        assert!(!admitting_result.success);
        assert!(
            admitting_result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no remote thread"))
        );
        assert!(
            caller_b
                .calls
                .lock()
                .expect("owner B calls lock")
                .is_empty()
        );
    }

    fn coordination_control_message(
        id: &str,
        delivery_state: crate::mailbox::MailboxDeliveryState,
        created_at_unix: u64,
        mode: crate::mailbox::MailboxControlMode,
    ) -> crate::mailbox::MailboxMessage {
        crate::mailbox::MailboxMessage {
            id: id.to_string(),
            sender: "parent".to_string(),
            recipient: "subagent:child:1".to_string(),
            body: "control".to_string(),
            payload: crate::mailbox::MailboxPayload::SubagentControl { mode },
            delivery_state,
            idempotency_key: None,
            created_at_unix,
            delivered_at_unix: None,
            read_at_unix: None,
            acknowledged_at_unix: None,
            delivery_error: None,
        }
    }

    #[test]
    fn agents_pane_displays_the_same_held_control_that_approval_targets() {
        let held = coordination_control_message(
            "held-control",
            crate::mailbox::MailboxDeliveryState::Held,
            10,
            crate::mailbox::MailboxControlMode::Cancel,
        );
        let newer = coordination_control_message(
            "newer-control",
            crate::mailbox::MailboxDeliveryState::Queued,
            20,
            crate::mailbox::MailboxControlMode::Steer,
        );

        let controls = [&held, &newer];
        let (displayed, approval) = displayed_coordination_control(&controls);
        assert_eq!(
            displayed.map(|message| message.id.as_str()),
            Some("held-control")
        );
        assert_eq!(
            approval.map(|message| message.id.as_str()),
            Some("held-control")
        );
    }

    #[test]
    fn durable_control_receipt_closes_claim_apply_crash_window() {
        let root = tempfile::tempdir().expect("receipt root");
        let records = root.path().join("records");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            records.clone(),
            "parent".to_string(),
        );
        let record = control_receipt_record(root.path());
        let receipt = DurableControlReceipt {
            mailbox_message_id: "message-1".to_string(),
            queue_id: control_queue_id("message-1"),
            mode: ControlMode::Followup,
            body: "continue safely".to_string(),
            attempt: record.attempt,
            accepted_at_ms: 10,
            acceptance_sequence: 1,
            state: DurableControlReceiptState::Applied,
        };

        assert!(manager.control_receipt(&record, "message-1").is_none());
        manager
            .write_control_receipt(&record, &receipt)
            .expect("persist acceptance before applying control");

        let restarted = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            records,
            "parent".to_string(),
        );
        assert_eq!(
            restarted.control_receipt(&record, "message-1"),
            Some(receipt.clone())
        );
        restarted
            .write_control_receipt(&record, &receipt)
            .expect("idempotent rewrite");
        assert_eq!(restarted.control_receipts(&record), vec![receipt]);
    }

    #[test]
    fn same_attempt_control_replays_until_a_snapshot_covers_its_queue_id() {
        let receipt = DurableControlReceipt {
            mailbox_message_id: "message-2".to_string(),
            queue_id: control_queue_id("message-2"),
            mode: ControlMode::Steer,
            body: "inspect the failing test".to_string(),
            attempt: 3,
            accepted_at_ms: 20,
            acceptance_sequence: 1,
            state: DurableControlReceiptState::Applied,
        };
        let mut covered = HashSet::new();

        assert!(control_receipt_needs_replay(&receipt, Some(3), &covered));
        covered.insert(receipt.queue_id);
        assert!(!control_receipt_needs_replay(&receipt, Some(3), &covered));
    }

    #[test]
    fn control_receipt_sequence_orders_tied_acceptance_timestamps() {
        let root = tempfile::tempdir().expect("receipt root");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "parent".to_string(),
        );
        let record = control_receipt_record(root.path());
        let receipt = |id: &str, body: &str, acceptance_sequence| DurableControlReceipt {
            mailbox_message_id: id.to_string(),
            queue_id: control_queue_id(id),
            mode: ControlMode::Followup,
            body: body.to_string(),
            attempt: record.attempt,
            accepted_at_ms: 50,
            acceptance_sequence,
            state: DurableControlReceiptState::Applied,
        };
        let first = receipt("message-first", "first", 1);
        let second = receipt("message-second", "second", 2);
        manager
            .write_control_receipt(&record, &second)
            .expect("persist second receipt first");
        manager
            .write_control_receipt(&record, &first)
            .expect("persist first receipt second");

        assert_eq!(manager.control_receipts(&record), vec![first, second]);
    }

    #[test]
    fn lifecycle_reconciliation_detects_atomic_replacement_with_mtime_collision() {
        let root = tempfile::tempdir().expect("record root");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "parent".to_string(),
        );
        let mut record = control_receipt_record(root.path());
        record.status = SubagentStatus::Completed;
        record.finished_at_ms = Some(2);
        record.lifecycle_notification_published = true;
        manager.write_record(&record).expect("persist record");
        manager.reconcile_lifecycle_records();

        let path = manager.record_path(&record.id);
        let original_modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("record mtime");
        record.attempt = 2;
        manager.write_record(&record).expect("rewrite record");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| {
                file.set_times(std::fs::FileTimes::new().set_modified(original_modified))
            })
            .expect("restore record mtime");
        manager.reconcile_lifecycle_records();
        assert_eq!(
            manager
                .observed_lifecycle_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&record.id)
                .map(|state| state.attempt),
            Some(2),
            "atomic replacement changes file identity even when mtime and length collide"
        );

        let observation = manager
            .observed_lifecycle_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&record.id)
            .cloned()
            .expect("observed record");
        manager.reconcile_lifecycle_records();
        assert_eq!(
            manager
                .observed_lifecycle_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&record.id),
            Some(&observation),
            "unchanged metadata should retain the cached observation"
        );
    }

    #[test]
    fn spawn_request_defaults_to_isolated_background_code_agent() {
        let request = parse_spawn_request(&serde_json::json!({
            "task": "inspect the parser"
        }))
        .expect("request should parse");

        assert_eq!(request.role, SubagentRole::Code);
        assert_eq!(request.isolation, SubagentIsolation::Worktree);
        assert!(request.run_in_background);
    }

    #[test]
    fn wait_timeout_requires_a_non_negative_integer() {
        assert_eq!(parse_wait_timeout(&serde_json::json!({})), Ok(0));
        assert_eq!(
            parse_wait_timeout(&serde_json::json!({"timeout_ms": 1000})),
            Ok(1000)
        );
        assert!(parse_wait_timeout(&serde_json::json!({"timeout_ms": 1000.0})).is_err());
        assert!(parse_wait_timeout(&serde_json::json!({"timeout_ms": -1})).is_err());
    }

    #[test]
    fn spawn_budgets_are_bounded_and_have_safe_defaults() {
        let request = parse_spawn_request(&serde_json::json!({
            "task": "inspect the parser",
            "timeout_ms": 2500,
            "max_tokens": 4096
        }))
        .expect("budgeted request should parse");
        assert_eq!(request.timeout_ms, 2500);
        assert_eq!(request.max_tokens, 4096);

        assert!(
            parse_spawn_request(&serde_json::json!({
                "task": "inspect",
                "timeout_ms": 0
            }))
            .is_err()
        );
        assert!(
            parse_spawn_request(&serde_json::json!({
                "task": "inspect",
                "max_tokens": 0
            }))
            .is_err()
        );
    }

    #[test]
    fn child_output_budget_accumulates_across_responses() {
        let mut used_tokens = 0;
        let first = crate::agent::TokenUsage {
            output_tokens: 3,
            ..Default::default()
        };
        let second = crate::agent::TokenUsage {
            output_tokens: 2,
            ..Default::default()
        };

        assert!(!record_child_output_tokens(
            &mut used_tokens,
            Some(&first),
            None,
            5
        ));
        assert!(record_child_output_tokens(
            &mut used_tokens,
            Some(&second),
            None,
            5
        ));
        assert_eq!(used_tokens, 5);
    }

    #[test]
    fn child_output_is_estimated_when_the_runtime_reports_no_usage() {
        // The Codex app-server path emits `ResponseEnd { usage: None }`, so a
        // run under it was charged nothing and never reached its budget.
        let mut used_tokens = 0;

        assert!(
            !record_child_output_tokens(&mut used_tokens, None, Some(40), 100),
            "40 characters is about 10 tokens, well inside a 100-token budget"
        );
        assert_eq!(used_tokens, 10);

        assert!(
            record_child_output_tokens(&mut used_tokens, None, Some(400), 100),
            "an unmetered runtime must still exhaust the budget"
        );
        assert_eq!(used_tokens, 110);
    }

    #[test]
    fn exact_usage_wins_over_the_estimate() {
        let mut used_tokens = 0;
        let usage = crate::agent::TokenUsage {
            output_tokens: 3,
            ..Default::default()
        };

        assert!(!record_child_output_tokens(
            &mut used_tokens,
            Some(&usage),
            Some(4_000),
            100
        ));
        assert_eq!(
            used_tokens, 3,
            "streamed characters must be ignored when the provider reports usage"
        );
    }

    #[test]
    fn a_metered_runtime_is_never_charged_an_estimate() {
        // A metered provider that omits usage on one response must not be
        // charged a guess: it is already bounded per request by the runner's
        // clamp, and a guess can only cancel a response the budget allowed.
        let mut used_tokens = 0;

        assert!(!record_child_output_tokens(
            &mut used_tokens,
            None,
            None,
            100
        ));
        assert_eq!(used_tokens, 0);
    }

    #[test]
    fn tool_call_payloads_count_against_an_unmetered_budget() {
        // A write-heavy child emits its output as tool-call arguments, not as
        // assistant text. Counting only `ResponseChunk` left `max_tokens`
        // ineffective for exactly the children most able to do damage.
        let big_patch = "x".repeat(4_000);
        let charged = tool_call_output_chars(
            "write",
            &serde_json::json!({"path": "src/main.rs", "content": big_patch}),
        );
        assert!(
            charged > 4_000,
            "the serialized arguments must be charged, got {charged}"
        );

        let mut used_tokens = 0;
        assert!(
            record_child_output_tokens(&mut used_tokens, None, Some(charged), 100),
            "a tool-call-heavy child must reach its budget"
        );
    }

    #[test]
    fn a_tool_call_with_no_prose_is_still_charged() {
        assert!(
            tool_call_output_chars("read", &serde_json::json!({})) > 0,
            "even an argument-free call is model-produced output"
        );
    }

    #[test]
    fn read_only_child_roles_get_a_read_only_sandbox() {
        // The advertised tool list is not the only way a child acts: on the
        // Codex transport it also reaches native commandExecution and
        // fileChange, which never pass through the tool registry.
        for role in [
            SubagentRole::Explore,
            SubagentRole::Plan,
            SubagentRole::Review,
        ] {
            assert!(!role.can_mutate(), "{} must be read-only", role.label());
            assert!(
                matches!(
                    child_sandbox_policy(role, Some(SandboxPolicy::DangerFullAccess)),
                    Some(SandboxPolicy::ReadOnly)
                ),
                "{} must not inherit a writable parent policy",
                role.label()
            );
            assert!(
                matches!(
                    child_sandbox_policy(role, None),
                    Some(SandboxPolicy::ReadOnly)
                ),
                "{} must be restricted even with no parent policy",
                role.label()
            );
        }
    }

    #[test]
    fn a_code_child_still_inherits_the_parent_sandbox() {
        assert!(SubagentRole::Code.can_mutate());
        assert!(matches!(
            child_sandbox_policy(SubagentRole::Code, Some(SandboxPolicy::DangerFullAccess)),
            Some(SandboxPolicy::DangerFullAccess)
        ));
        assert!(child_sandbox_policy(SubagentRole::Code, None).is_none());
    }

    #[test]
    fn a_turn_without_usage_is_charged_the_estimate_even_on_a_metered_model() {
        // An OpenAI-compatible endpoint may omit the usage chunk on any turn.
        // Deciding by model name charged that turn nothing, so a tool-calling
        // child got its full allowance again every turn.
        let mut metering = ChildOutputMetering::for_model("openai/gpt-4o");
        assert!(
            !metering.enforces_mid_stream(),
            "a metered model is not policed mid-stream until it fails to report"
        );

        let estimate = metering.estimate_for_turn(false, 4_000);
        assert_eq!(
            estimate,
            Some(4_000),
            "a turn that reported no usage must be estimated"
        );
        assert!(
            metering.enforces_mid_stream(),
            "once a turn reports no usage the run is policed mid-stream"
        );

        let mut used_tokens = 0;
        assert!(record_child_output_tokens(
            &mut used_tokens,
            None,
            estimate,
            100
        ));
    }

    #[test]
    fn a_reported_turn_is_charged_exactly_and_not_also_estimated() {
        let mut metering = ChildOutputMetering::for_model("openai/gpt-4o");

        assert_eq!(
            metering.estimate_for_turn(true, 4_000),
            None,
            "streamed characters must not be charged on top of real usage"
        );
        assert!(
            !metering.enforces_mid_stream(),
            "a reporting provider stays unpoliced by the estimate"
        );

        let mut used_tokens = 0;
        let usage = crate::agent::TokenUsage {
            output_tokens: 7,
            ..Default::default()
        };
        assert!(!record_child_output_tokens(
            &mut used_tokens,
            Some(&usage),
            None,
            100
        ));
        assert_eq!(used_tokens, 7);
    }

    #[test]
    fn the_codex_path_is_policed_from_its_first_turn() {
        // Codex sends no per-request `max_tokens`, so an unpoliced first turn
        // has no bound at all. Every other runtime is bounded per request by
        // the runner's clamp, which is why they wait for evidence.
        let metering = ChildOutputMetering::for_model("openai-codex/gpt-5.5");
        assert!(metering.enforces_mid_stream());
    }

    #[test]
    fn only_the_codex_app_server_path_counts_as_unmetered() {
        assert!(!child_output_is_metered("openai-codex/gpt-5.5"));
        assert!(child_output_is_metered("claude-sonnet-4-5-20250514"));
        assert!(child_output_is_metered("openai/gpt-4o"));
    }

    #[test]
    fn streamed_output_exhausts_the_budget_before_the_response_boundary() {
        // Without the mid-stream check a single turn could produce far more
        // than the budget and only be stopped at the boundary after it.
        assert!(!child_output_budget_exhausted(0, 40, 100));
        assert!(
            child_output_budget_exhausted(50, 200, 100),
            "50 spent plus about 50 streamed reaches a 100-token budget"
        );
    }

    fn running_wait_record(root: &Path) -> SubagentRecord {
        SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "wait-scope".to_string(),
            parent_call_id: "wait-call".to_string(),
            last_parent_scope_id: "wait-scope".to_string(),
            last_call_id: "wait-call".to_string(),
            task: "keep working".to_string(),
            current_prompt: "keep working".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.join("session")),
            status: SubagentStatus::Running,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        }
    }

    /// `wait` loads through `load_record`, which rewrites a Running child with
    /// no runtime entry and no execution lease to Interrupted.
    fn hold_running_wait_lease(record: &SubagentRecord) -> SessionLock {
        std::fs::create_dir_all(SubagentManager::session_dir(record))
            .expect("session directory should exist");
        SessionLock::acquire(&SubagentManager::timeline_path(record))
            .expect("execution lease should be held")
    }

    #[tokio::test]
    async fn a_blocking_subagent_wait_is_released_when_the_user_steers() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let record = running_wait_record(root.path());
        manager.write_record(&record).expect("write running record");
        let _lease = hold_running_wait_lease(&record);

        let mut queue = crate::agent::message_queue::MessageQueue::with_max_size(10);
        manager.set_steer_signal(queue.steer_signal());

        let args = serde_json::json!({
            "subagent_id": record.id,
            "timeout_ms": 30_000,
        });
        let waiting_manager = manager.clone();
        let waiting = tokio::spawn(async move { waiting_manager.wait(&args, None).await });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let steered_at = Instant::now();
        queue.push_with_kind("stop and read this instead", PromptKind::Steer);

        let result = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("a steered wait must not run to its 30s timeout")
            .expect("wait task");
        let released_after = steered_at.elapsed();

        assert!(
            released_after < Duration::from_millis(250),
            "the wait must release promptly after the steer, took {released_after:?}"
        );
        assert!(result.success, "release is not a failure: {result:?}");
        assert!(
            result.output.contains("still running"),
            "the model must be told the subagent survived: {}",
            result.output
        );
        let details = result.details.expect("released wait carries details");
        assert_eq!(
            details.get("status").and_then(serde_json::Value::as_str),
            Some("released_by_steering")
        );
        assert_eq!(
            details
                .get("subagentStatus")
                .and_then(serde_json::Value::as_str),
            Some("running")
        );
    }

    #[tokio::test]
    async fn a_wait_without_a_queued_steer_still_runs_to_its_timeout() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let record = running_wait_record(root.path());
        manager.write_record(&record).expect("write running record");

        let queue = crate::agent::message_queue::MessageQueue::with_max_size(10);
        manager.set_steer_signal(queue.steer_signal());
        let _lease = hold_running_wait_lease(&record);

        let args = serde_json::json!({
            "subagent_id": record.id,
            "timeout_ms": 200,
        });
        let started = Instant::now();
        let result = manager.wait(&args, None).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(150),
            "an unsteered wait must still block for its timeout, took {elapsed:?}"
        );
        let details = result.details.expect("timed-out wait carries details");
        assert_ne!(
            details.get("status").and_then(serde_json::Value::as_str),
            Some("released_by_steering")
        );
    }

    #[test]
    fn child_output_firewall_bounds_parent_delivery_and_preserves_saved_result() {
        let root = tempfile::tempdir().unwrap();
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let mut record = running_wait_record(root.path());
        record.backend = SubagentBackend::Native;
        record.status = SubagentStatus::Completed;
        record.result = Some(SubagentResult {
            output: "界".repeat(10_000),
            files_modified: vec![],
        });
        manager.write_record(&record).unwrap();
        let result = manager.get(&serde_json::json!({"subagent_id": record.id}));
        assert!(result.output.len() <= 8_192);
        assert!(result.output.contains("inspect the saved child session"));
        assert_eq!(
            manager
                .load_record(&record.id)
                .unwrap()
                .result
                .unwrap()
                .output
                .len(),
            30_000
        );
    }

    #[test]
    fn partial_handoff_and_procedure_suggestion_survive_reload_and_parent_delivery() {
        let root = tempfile::tempdir().unwrap();
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let mut record = running_wait_record(root.path());
        record.last_parent_scope_id = manager.parent_scope_id();
        let skill = root.path().join("SKILL.md");
        std::fs::write(&skill, "Original procedure").unwrap();
        let output = serde_json::json!({
            "outcome":"partial","summary":"Parser implementation finished",
            "completed":["Parser"],"remaining":["Connect the UI"],
            "blockers":[],"references":["src/parser.rs:12"],
            "procedure_feedback":[{"skill_path":skill.to_string_lossy(),
                "observation":"Documented test command was missing",
                "suggested_change":"Use npm run test:ui"}]
        })
        .to_string();
        let child = CredentialVault::new();
        let parent = CredentialVault::new();
        let parent_scope = ParentCredentialScope {
            vault: &parent,
            generation: parent.generation(),
        };
        let finished = manager
            .finish_record(
                record,
                SubagentStatus::Completed,
                Some(SubagentResult {
                    output,
                    files_modified: vec!["src/parser.rs".into()],
                }),
                None,
                &child,
                &parent_scope,
            )
            .unwrap();
        let reloaded = manager.load_record(&finished.id).unwrap();
        let details = record_details(&reloaded);
        assert_eq!(details["handoff"]["remaining"][0], "Connect the UI");
        assert_eq!(
            details["handoff"]["procedure_feedback"][0]["suggested_change"],
            "Use npm run test:ui"
        );
        assert_eq!(details["completionVerified"], false);
        let result = manager.get(&serde_json::json!({"subagent_id":finished.id}));
        assert!(result.output.starts_with("Unfinished work: Connect the UI"));
        let events = manager.poll_lifecycle_events();
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .summary
                .as_deref()
                .unwrap()
                .starts_with("Unfinished work: Connect the UI")
        );
        manager.acknowledge_lifecycle_event(&events[0]).unwrap();
        assert!(
            manager.poll_lifecycle_events().is_empty(),
            "completion must not repeat after acknowledgment"
        );
        assert_eq!(
            std::fs::read_to_string(skill).unwrap(),
            "Original procedure"
        );
    }

    #[test]
    fn resumed_child_completion_is_routed_to_the_current_parent_scope() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
        let spawn_scope = format!("spawn-scope-{}", uuid::Uuid::new_v4());
        let resume_scope = format!("resume-scope-{}", uuid::Uuid::new_v4());
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: spawn_scope.clone(),
            parent_call_id: "spawn-call".to_string(),
            last_parent_scope_id: resume_scope.clone(),
            last_call_id: "resume-call".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root.path()),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("session")),
            status: SubagentStatus::Running,
            attempt: 2,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };

        let child_vault = CredentialVault::new();
        let parent_vault = CredentialVault::new();
        let parent_scope = ParentCredentialScope {
            vault: &parent_vault,
            generation: parent_vault.generation(),
        };
        manager
            .finish_record(
                record,
                SubagentStatus::Completed,
                Some(SubagentResult {
                    output: "done".to_string(),
                    files_modified: Vec::new(),
                }),
                None,
                &child_vault,
                &parent_scope,
            )
            .expect("terminal record should persist");

        manager.set_parent_scope_id(spawn_scope);
        assert!(
            manager.poll_lifecycle_events().is_empty(),
            "the original spawn scope no longer has a consumer"
        );
        manager.set_parent_scope_id(resume_scope);
        let events = manager.poll_lifecycle_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].parent_call_id, "resume-call");
        assert_eq!(events[0].attempt, 2);
        assert_eq!(events[0].status, SubagentStatus::Completed);
    }

    #[test]
    fn lifecycle_notification_failure_preserves_result_and_retries() {
        let root = tempfile::tempdir().expect("records root should exist");
        let records = root.path().join("records");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            records,
            "parent-scope".to_string(),
        );
        std::fs::create_dir_all(&manager.mailbox_path).expect("block mailbox with directory");
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root.path()),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("session")),
            status: SubagentStatus::Running,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        let child_vault = CredentialVault::new();
        let parent_vault = CredentialVault::new();
        let parent_scope = ParentCredentialScope {
            vault: &parent_vault,
            generation: parent_vault.generation(),
        };
        let finished = manager
            .finish_record(
                record,
                SubagentStatus::Completed,
                Some(SubagentResult {
                    output: "durable result".to_string(),
                    files_modified: Vec::new(),
                }),
                None,
                &child_vault,
                &parent_scope,
            )
            .expect("mailbox failure must not discard the terminal result");
        assert_eq!(
            finished
                .result
                .as_ref()
                .map(|result| result.output.as_str()),
            Some("durable result")
        );

        std::fs::remove_dir(&manager.mailbox_path).expect("unblock mailbox");
        let events = manager.poll_lifecycle_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attempt, 1);
        assert_eq!(
            events[0].summary.as_deref(),
            Some(
                "Completion is unverified: child did not return a valid structured handoff. Retrieve the saved result and clarify remaining work before declaring the parent task complete. durable result"
            )
        );
        let persisted = manager
            .load_record(&finished.id)
            .expect("reload terminal record");
        assert!(persisted.lifecycle_notification_published);

        manager
            .acknowledge_lifecycle_event(&events[0])
            .expect("acknowledge applied lifecycle");
        let mut mailbox = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
            .expect("reload mailbox");
        mailbox.compact().expect("compact acknowledged lifecycle");
        let mut compacted = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
            .expect("reload compacted mailbox");
        compacted.idempotency_receipts.clear();
        std::fs::write(
            &manager.mailbox_path,
            serde_json::to_vec_pretty(&compacted).expect("serialize compacted mailbox"),
        )
        .expect("drop bounded receipt to simulate long-lived record");
        manager
            .pending_lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(finished.id.clone());
        manager.retry_terminal_lifecycle_notifications();
        let reconciled = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
            .expect("reload reconciled mailbox");
        assert!(
            reconciled.messages.is_empty(),
            "a durable record marker must prevent stale lifecycle replay"
        );
    }

    #[test]
    fn lifecycle_reconciliation_discovers_late_records_and_resumed_attempts() {
        let root = tempfile::tempdir().expect("records root");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "parent-scope".to_string(),
        );
        manager.retry_terminal_lifecycle_notifications();

        let mut record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-late".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-late".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root.path()),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("session")),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: Some(3),
            result: Some(SubagentResult {
                output: "late completion".to_string(),
                files_modified: Vec::new(),
            }),
            error: None,
            lifecycle_notification_published: false,
        };
        manager.write_record(&record).expect("persist late record");

        manager.retry_terminal_lifecycle_notifications();
        let events = manager.poll_lifecycle_events();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subagent_id, record.id);
        assert_eq!(
            events[0].summary.as_deref(),
            Some(
                "Completion is unverified: child did not return a valid structured handoff. Retrieve the saved result and clarify remaining work before declaring the parent task complete. late completion"
            )
        );
        manager
            .acknowledge_lifecycle_event(&events[0])
            .expect("acknowledge first attempt");

        record.attempt = 2;
        record.last_call_id = "call-resumed".to_string();
        record.finished_at_ms = Some(4);
        record.result = Some(SubagentResult {
            output: "resumed completion".to_string(),
            files_modified: Vec::new(),
        });
        record.lifecycle_notification_published = false;
        manager
            .write_record(&record)
            .expect("persist resumed attempt completion");

        manager.retry_terminal_lifecycle_notifications();
        let resumed_events = manager.poll_lifecycle_events();

        assert_eq!(resumed_events.len(), 1);
        assert_eq!(resumed_events[0].subagent_id, record.id);
        assert_eq!(resumed_events[0].attempt, 2);
        assert_eq!(
            resumed_events[0].summary.as_deref(),
            Some(
                "Completion is unverified: child did not return a valid structured handoff. Retrieve the saved result and clarify remaining work before declaring the parent task complete. resumed completion"
            )
        );
    }

    #[test]
    fn lifecycle_marker_retry_does_not_overwrite_a_resumed_attempt() {
        let root = tempfile::tempdir().expect("records root");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "parent-scope".to_string(),
        );
        let mut record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root.path()),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("session")),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: Some(3),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        std::fs::create_dir_all(SubagentManager::session_dir(&record)).expect("session dir");
        manager
            .write_record(&record)
            .expect("persist terminal record");
        let _lease = SessionLock::acquire(&SubagentManager::timeline_path(&record))
            .expect("simulate resumed child lease");
        // The first reconciliation discovers this record from disk.
        manager.retry_terminal_lifecycle_notifications();
        assert!(
            manager
                .pending_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&record.id)
        );

        record.attempt = 2;
        record.status = SubagentStatus::Running;
        record.current_prompt = "resume safely".to_string();
        record.finished_at_ms = None;
        manager
            .write_record(&record)
            .expect("persist resumed attempt");
        manager.retry_terminal_lifecycle_notifications();

        let loaded = manager
            .load_record(&record.id)
            .expect("reload resumed attempt");
        assert_eq!(loaded.attempt, 2);
        assert_eq!(loaded.status, SubagentStatus::Running);
        assert_eq!(loaded.current_prompt, "resume safely");
        assert!(
            manager
                .pending_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&record.id)
        );

        drop(_lease);
        record.status = SubagentStatus::Completed;
        record.finished_at_ms = Some(4);
        manager
            .write_record(&record)
            .expect("persist resumed attempt completion");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let published = loop {
            manager.retry_terminal_lifecycle_notifications();
            let published = manager
                .load_record(&record.id)
                .expect("reload published completion");
            if published.lifecycle_notification_published {
                break published;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resumed completion stayed blocked after the inherited lock descriptor closed"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(published.lifecycle_notification_published);
        assert!(
            !manager
                .pending_lifecycle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&record.id)
        );
    }

    #[test]
    fn stale_held_control_is_denied_instead_of_approved() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "current-parent".to_string(),
        );
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "original-parent".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "current-parent".to_string(),
            last_call_id: "call-2".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect again".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(root.path()),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("session")),
            status: SubagentStatus::Running,
            attempt: 2,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(2),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager.write_record(&record).expect("persist record");
        let (control_tx, _control_rx) = mpsc::channel(RUNTIME_CONTROL_CAPACITY);
        manager
            .runtime
            .insert(&record.id, CancellationToken::new(), control_tx);
        assert_eq!(
            manager.active_mailbox_recipients(),
            vec![agent_ref(&record)]
        );
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&manager.mailbox_path);
        let owned_message_id = mailbox
            .send_typed(
                "other-parent",
                agent_ref(&record),
                "inspect before approval",
                crate::mailbox::MailboxPayload::SubagentControl {
                    mode: ControlMode::Steer,
                },
                crate::mailbox::MailboxDeliveryState::Held,
                None,
            )
            .expect("owned held control");
        let inspected = manager
            .inspect_control(&owned_message_id)
            .expect("inspect active child control");
        assert_eq!(
            inspected.delivery_state,
            crate::mailbox::MailboxDeliveryState::Held
        );
        assert_eq!(inspected.body, "inspect before approval");

        let message_id = mailbox
            .send_typed(
                "other-parent",
                format!("subagent:{}:1", record.id),
                "cancel",
                crate::mailbox::MailboxPayload::SubagentControl {
                    mode: ControlMode::Cancel,
                },
                crate::mailbox::MailboxDeliveryState::Held,
                None,
            )
            .expect("held control");

        let error = manager
            .approve_held_control(&message_id)
            .expect_err("stale attempt must not be approved");
        assert!(error.contains("now on attempt 2"), "{error}");
        let loaded = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
            .expect("reload mailbox");
        assert_eq!(
            loaded
                .messages
                .iter()
                .find(|message| message.id == message_id)
                .expect("stale control")
                .delivery_state,
            crate::mailbox::MailboxDeliveryState::Denied
        );
        assert_eq!(
            loaded
                .messages
                .iter()
                .find(|message| message.id == owned_message_id)
                .expect("owned control")
                .delivery_state,
            crate::mailbox::MailboxDeliveryState::Held
        );
        manager.runtime.remove(&record.id);
    }

    #[tokio::test]
    async fn runtime_registry_queues_after_its_concurrency_limit() {
        let runtime = RuntimeRegistry::with_capacity(1);
        let first = runtime.acquire_permit().await.expect("first permit");
        assert_eq!(runtime.available_permits(), 0);

        let second =
            tokio::time::timeout(Duration::from_millis(10), runtime.acquire_permit()).await;
        assert!(second.is_err(), "second child should remain queued");
        drop(first);
        let _second = runtime
            .acquire_permit()
            .await
            .expect("permit should be reusable");
    }

    #[tokio::test]
    async fn runtime_registry_delivers_attempt_scoped_controls() {
        let runtime = RuntimeRegistry::with_capacity(1);
        let token = CancellationToken::new();
        let (sender, mut receiver) = mpsc::channel(RUNTIME_CONTROL_CAPACITY);
        let id = uuid::Uuid::new_v4().to_string();
        runtime.insert(&id, token, sender);
        assert!(runtime.send_control(
            &id,
            RuntimeControlRequest {
                mailbox_id: "m-control".to_string(),
                recipient: format!("subagent:{id}:3"),
                mode: ControlMode::Steer,
                body: "focus on the regression".to_string(),
            }
        ));
        let delivered = receiver.recv().await.expect("control request");
        assert_eq!(delivered.mode, ControlMode::Steer);
        assert_eq!(delivered.recipient, format!("subagent:{id}:3"));
        runtime.remove(&id);
    }

    #[tokio::test]
    async fn runtime_control_channel_is_bounded() {
        let runtime = RuntimeRegistry::with_capacity(1);
        let token = CancellationToken::new();
        let (sender, _receiver) = mpsc::channel(RUNTIME_CONTROL_CAPACITY);
        let id = uuid::Uuid::new_v4().to_string();
        runtime.insert(&id, token, sender);

        for index in 0..RUNTIME_CONTROL_CAPACITY {
            assert!(runtime.send_control(
                &id,
                RuntimeControlRequest {
                    mailbox_id: format!("m-{index}"),
                    recipient: format!("subagent:{id}:0"),
                    mode: ControlMode::Steer,
                    body: "queued durably".to_string(),
                }
            ));
        }
        assert!(!runtime.send_control(
            &id,
            RuntimeControlRequest {
                mailbox_id: "m-overflow".to_string(),
                recipient: format!("subagent:{id}:0"),
                mode: ControlMode::Steer,
                body: "falls back to mailbox polling".to_string(),
            }
        ));
    }

    #[test]
    fn agent_refs_are_attempt_scoped_and_strictly_parsed() {
        let id = uuid::Uuid::new_v4().to_string();
        assert_eq!(parse_agent_ref(&format!("subagent:{id}:7")), Ok((id, 7)));
        assert!(parse_agent_ref("subagent:not-a-uuid:7").is_err());
        assert!(parse_agent_ref("subagent:missing-attempt").is_err());
    }

    #[test]
    fn lifecycle_events_are_parent_scoped_and_consumed_once() {
        let root = tempfile::tempdir().expect("records root");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "parent-a".to_string(),
        );
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&manager.mailbox_path);
        for (recipient, id, status) in [
            ("parent-a", "child-1", MailboxLifecycleStatus::Completed),
            ("parent-b", "child-2", MailboxLifecycleStatus::Failed),
        ] {
            mailbox
                .send_typed(
                    "child",
                    recipient,
                    "finished",
                    crate::mailbox::MailboxPayload::SubagentLifecycle {
                        subagent_id: id.to_string(),
                        parent_call_id: "call-1".to_string(),
                        attempt: 0,
                        status,
                        summary: Some("done".to_string()),
                        error: None,
                        finished_at_ms: 10,
                    },
                    crate::mailbox::MailboxDeliveryState::Queued,
                    Some(format!("lifecycle:{id}")),
                )
                .expect("lifecycle message");
        }

        let events = manager.poll_lifecycle_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subagent_id, "child-1");
        let delivered = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
            .expect("reload delivered lifecycle");
        assert_eq!(
            delivered
                .messages
                .iter()
                .find(|message| message.id == events[0].mailbox_message_id)
                .map(|message| message.delivery_state),
            Some(crate::mailbox::MailboxDeliveryState::Delivered)
        );
        manager
            .acknowledge_lifecycle_event(&events[0])
            .expect("acknowledge after host application");
        let acknowledged = crate::mailbox::MailboxStore::load_from_path(&manager.mailbox_path)
            .expect("reload acknowledged lifecycle");
        assert_eq!(
            acknowledged
                .messages
                .iter()
                .find(|message| message.id == events[0].mailbox_message_id)
                .map(|message| message.delivery_state),
            Some(crate::mailbox::MailboxDeliveryState::Acknowledged)
        );
        assert!(manager.poll_lifecycle_events().is_empty());
        manager.set_parent_scope_id("parent-b".to_string());
        assert_eq!(manager.poll_lifecycle_events().len(), 1);
    }

    #[test]
    fn a_manager_scope_rotates_behind_a_shared_handle() {
        // The tool executor holding this manager is behind an `Arc` that
        // outlives any one conversation, so the scope has to rotate through
        // `&self` rather than by rebuilding the executor.
        let manager = SubagentManager::with_parent_scope(PathBuf::from("/tmp"), "session:alpha");
        let shared = manager.clone();
        assert_eq!(shared.parent_scope_id(), "session:alpha");

        manager.set_parent_scope_id("session:beta".to_string());

        assert_eq!(manager.parent_scope_id(), "session:beta");
        assert_eq!(
            shared.parent_scope_id(),
            "session:beta",
            "every handle to one executor's manager must see the rotation"
        );
    }

    #[test]
    fn a_rotated_scope_leaves_the_previous_conversation_events_parked() {
        // A child that finishes after its conversation ended must not be
        // drained by the next one, and must still be there when its own
        // session is resumed.
        let root = tempfile::tempdir().expect("records root");
        let manager = SubagentManager::with_root_and_parent_scope(
            root.path().to_path_buf(),
            root.path().join("records"),
            "session:beta".to_string(),
        );
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&manager.mailbox_path);
        mailbox
            .send_typed(
                "child",
                "session:alpha",
                "finished",
                crate::mailbox::MailboxPayload::SubagentLifecycle {
                    subagent_id: "child-1".to_string(),
                    parent_call_id: "call-1".to_string(),
                    attempt: 0,
                    status: MailboxLifecycleStatus::Completed,
                    summary: Some("done".to_string()),
                    error: None,
                    finished_at_ms: 10,
                },
                crate::mailbox::MailboxDeliveryState::Queued,
                Some("lifecycle:child-1".to_string()),
            )
            .expect("lifecycle message");

        assert!(
            manager.poll_lifecycle_events().is_empty(),
            "the new conversation must not see the previous one's child"
        );

        manager.set_parent_scope_id("session:alpha".to_string());
        let resumed = manager.poll_lifecycle_events();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].subagent_id, "child-1");
    }

    #[test]
    fn workspace_record_root_distinguishes_colliding_sanitized_paths() {
        let left = default_root(Path::new("/tmp/a/b-c"));
        let right = default_root(Path::new("/tmp/a-b/c"));
        assert_ne!(left, right);
    }

    #[test]
    fn worktree_setup_requires_an_unrestricted_parent_sandbox() {
        let workspace_write = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        };

        assert!(!worktree_setup_allowed(Some(&workspace_write)));
        assert!(!worktree_setup_allowed(Some(&SandboxPolicy::ReadOnly)));
        assert!(worktree_setup_allowed(Some(
            &SandboxPolicy::DangerFullAccess
        )));
        assert!(worktree_setup_allowed(None));
    }

    #[test]
    fn changed_files_since_detects_changes_to_initially_dirty_files() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.path().join("tracked.txt"), "base\n")
            .expect("base file should be written");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "init"]);

        std::fs::write(root.path().join("tracked.txt"), "parent change\n")
            .expect("parent change should be written");
        std::fs::write(root.path().join("space name.txt"), "untracked\n")
            .expect("untracked file should be written");
        assert!(changed_files(root.path()).contains(&PathBuf::from("space name.txt")));
        let (initial_paths, initial_fingerprints) = changed_file_baseline(root.path());
        let (initial_files, initial_file_fingerprints) =
            serialize_file_baseline(initial_paths, initial_fingerprints);

        std::fs::write(root.path().join("tracked.txt"), "child change\n")
            .expect("child change should be written");
        assert_eq!(
            changed_files_since(
                root.path(),
                None,
                &initial_files,
                &initial_file_fingerprints,
            ),
            vec!["tracked.txt"]
        );

        std::fs::remove_file(root.path().join("tracked.txt")).expect("tracked file should delete");
        std::fs::remove_file(root.path().join("space name.txt"))
            .expect("untracked file should delete");
        let mut deleted = changed_files_since(
            root.path(),
            None,
            &initial_files,
            &initial_file_fingerprints,
        );
        deleted.sort();
        assert_eq!(deleted, vec!["space name.txt", "tracked.txt"]);
    }

    #[test]
    fn changed_file_fingerprints_resolve_repository_root_relative_paths() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        let cwd = root.path().join("packages").join("foo");
        std::fs::create_dir_all(&cwd).expect("nested cwd should be created");
        std::fs::write(cwd.join("tracked.txt"), "base\n").expect("base file should be written");
        git(&["add", "packages/foo/tracked.txt"]);
        git(&["commit", "--quiet", "-m", "init"]);

        std::fs::write(cwd.join("tracked.txt"), "parent change\n")
            .expect("parent change should be written");
        let (initial_paths, initial_fingerprints) = changed_file_baseline(&cwd);
        let (initial_files, initial_file_fingerprints) =
            serialize_file_baseline(initial_paths, initial_fingerprints);

        std::fs::write(cwd.join("tracked.txt"), "child change\n")
            .expect("child change should be written");
        assert_eq!(
            changed_files_since(&cwd, None, &initial_files, &initial_file_fingerprints,),
            vec!["packages/foo/tracked.txt"]
        );
    }

    #[test]
    fn changed_files_since_detects_index_only_changes() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.path().join("tracked.txt"), "base\n")
            .expect("base file should be written");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "init"]);

        std::fs::write(root.path().join("tracked.txt"), "parent change\n")
            .expect("parent change should be written");
        let (unstaged_paths, unstaged_fingerprints) = changed_file_baseline(root.path());
        let (unstaged_files, unstaged_fingerprints) =
            serialize_file_baseline(unstaged_paths, unstaged_fingerprints);

        git(&["add", "tracked.txt"]);
        assert_eq!(
            changed_files_since(root.path(), None, &unstaged_files, &unstaged_fingerprints,),
            vec!["tracked.txt"]
        );

        let (staged_paths, staged_fingerprints) = changed_file_baseline(root.path());
        let (staged_files, staged_fingerprints) =
            serialize_file_baseline(staged_paths, staged_fingerprints);
        git(&["restore", "--staged", "--", "tracked.txt"]);
        assert_eq!(
            changed_files_since(root.path(), None, &staged_files, &staged_fingerprints,),
            vec!["tracked.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn changed_files_since_detects_worktree_mode_changes() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        let path = root.path().join("tracked.txt");
        std::fs::write(&path, "base\n").expect("base file should be written");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "init"]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("baseline mode should be set");

        let (initial_paths, initial_fingerprints) = changed_file_baseline(root.path());
        let (initial_files, initial_file_fingerprints) =
            serialize_file_baseline(initial_paths, initial_fingerprints);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("child mode should be set");

        assert_eq!(
            changed_files_since(
                root.path(),
                None,
                &initial_files,
                &initial_file_fingerprints,
            ),
            vec!["tracked.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn serialized_non_utf8_paths_round_trip_for_change_reporting() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        let raw_name = b"invalid-\xff.txt";
        let path = PathBuf::from(OsString::from_vec(raw_name.to_vec()));
        let serialized = serialize_repository_path(&path);
        assert_eq!(
            deserialize_repository_path(&serialized)
                .as_os_str()
                .as_bytes(),
            raw_name
        );
        let initial_files = vec![serialized.clone()];
        let initial_file_fingerprints = HashMap::from([(serialized, "baseline".to_string())]);

        assert_eq!(
            changed_files_since(
                root.path(),
                None,
                &initial_files,
                &initial_file_fingerprints,
            ),
            vec![String::from_utf8_lossy(raw_name).into_owned()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn subagent_record_preserves_non_utf8_working_directory() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let cwd = root.path().join(OsString::from_vec(b"child-\xff".to_vec()));
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(&cwd),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("session")),
            status: SubagentStatus::Queued,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: None,
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };

        manager.write_record(&record).expect("write record");
        let loaded = manager.load_record(&record.id).expect("reload record");

        assert_eq!(deserialize_repository_path(&loaded.cwd), cwd);
    }

    #[test]
    fn changed_files_since_detects_committed_child_changes() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.path().join("tracked.txt"), "base\n")
            .expect("base file should be written");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "init"]);

        let initial_head = git_repository_head(root.path()).expect("initial HEAD should exist");
        let (initial_paths, initial_fingerprints) = changed_file_baseline(root.path());
        let (initial_files, initial_file_fingerprints) =
            serialize_file_baseline(initial_paths, initial_fingerprints);
        std::fs::write(root.path().join("tracked.txt"), "child commit\n")
            .expect("child change should be written");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "child"]);

        assert!(changed_files(root.path()).is_empty());
        assert_eq!(
            changed_files_since(
                root.path(),
                Some(&initial_head),
                &initial_files,
                &initial_file_fingerprints,
            ),
            vec!["tracked.txt"]
        );
    }

    #[test]
    fn refreshed_baseline_excludes_parent_changes_from_resumed_attempt() {
        let root = tempfile::tempdir().expect("temp root");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git should run");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.path().join("tracked.txt"), "base\n")
            .expect("base file should be written");
        git(&["add", "tracked.txt"]);
        git(&["commit", "--quiet", "-m", "init"]);

        let (spawn_paths, spawn_fingerprints) = changed_file_baseline(root.path());
        let (spawn_files, spawn_fingerprints) =
            serialize_file_baseline(spawn_paths, spawn_fingerprints);
        std::fs::write(root.path().join("tracked.txt"), "first attempt\n")
            .expect("first attempt should be written");
        assert_eq!(
            changed_files_since(root.path(), None, &spawn_files, &spawn_fingerprints,),
            vec!["tracked.txt"]
        );

        let (resume_paths, resume_fingerprints) = changed_file_baseline(root.path());
        let (resume_files, resume_fingerprints) =
            serialize_file_baseline(resume_paths, resume_fingerprints);
        assert!(
            changed_files_since(root.path(), None, &resume_files, &resume_fingerprints,).is_empty()
        );
    }

    #[tokio::test]
    async fn subagent_record_is_atomic_reloadable_and_listed_with_safe_handles() {
        let root = tempfile::tempdir().expect("temp root");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: uuid::Uuid::new_v4().to_string(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: "/workspace".to_string(),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: root.path().join("session").display().to_string(),
            status: SubagentStatus::Queued,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: None,
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };

        manager.write_record(&record).expect("write record");
        let loaded = manager.load_record(&record.id).expect("reload record");
        assert_eq!(loaded.id, record.id);
        assert_eq!(loaded.parent_call_id, "call-1");

        let malformed_dir = manager.root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&malformed_dir).expect("malformed record dir");
        std::fs::write(malformed_dir.join("record.json"), "not json").expect("malformed record");
        let listed = manager.list().await;
        let details = listed.details.expect("list details");
        assert_eq!(details["complete"], false);
        assert_eq!(details["count"], 1);
        assert_eq!(details["errors"].as_array().map(Vec::len), Some(1));
        assert!(details["snapshotId"].as_str().is_some());
        assert_eq!(
            details["subagents"][0]["agentRef"],
            format!("subagent:{}:1", record.id)
        );

        let mut cancelled = loaded;
        cancelled.status = SubagentStatus::Cancelled;
        cancelled.error = Some("subagent cancelled".to_string());
        assert!(!tool_result_for_record(cancelled).success);
    }

    #[test]
    fn terminal_subagent_without_worktree_can_be_inspected_and_cleaned() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: "/workspace".to_string(),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: root.path().join("session").display().to_string(),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        let inspected = manager.inspect(&serde_json::json!({"subagent_id": id}));
        assert!(inspected.success);
        let cleaned = manager.cleanup(&serde_json::json!({"subagent_id": record.id}));
        assert!(cleaned.success);
        assert!(
            !manager
                .load_record(&record.id)
                .expect("record should remain durable")
                .worktree_cleaned,
            "shared children have no worktree, so cleanup must leave them resumable"
        );
    }

    #[test]
    fn shared_child_remains_resumable_after_noop_cleanup() {
        let root = tempfile::tempdir().expect("records root should exist");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace should exist");
        let manager = SubagentManager::with_root(workspace.clone(), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let session_dir = root.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session directory should exist");
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(&workspace),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&session_dir),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        let cleaned = manager.cleanup(&serde_json::json!({"subagent_id": id}));
        assert!(cleaned.success);
        assert!(
            manager.revalidate_resumable_under_lease(&id).is_ok(),
            "a shared child must remain resumable after a no-op cleanup"
        );
    }

    #[test]
    fn a_child_holding_the_execution_lease_is_not_marked_interrupted() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let session_dir = root.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session directory should exist");
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: "/workspace".to_string(),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&session_dir),
            status: SubagentStatus::Running,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: None,
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        // Stands in for a second Maestro process running this child: the
        // lease is held, but this process's runtime registry never saw the
        // child, so `runtime.get` reports nothing.
        let lease = SessionLock::acquire(&SubagentManager::timeline_path(&record))
            .expect("execution lease should be held");

        assert_eq!(
            manager.load_record(&id).expect("record should load").status,
            SubagentStatus::Running,
            "a leased child must not be rewritten to a terminal status"
        );
        let cleaned = manager.cleanup(&serde_json::json!({"subagent_id": id}));
        assert!(
            !cleaned.success,
            "cleanup must refuse a child whose execution lease is still held"
        );

        drop(lease);
        // Another test may have forked while `lease` was live, temporarily
        // keeping its inherited file descriptor open until exec. Once that
        // descriptor closes, the record must be reconciled as interrupted.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let interrupted = loop {
            let loaded = manager.load_record(&id).expect("record should load");
            if loaded.status == SubagentStatus::Interrupted {
                break loaded;
            }
            assert_eq!(
                loaded.status,
                SubagentStatus::Running,
                "a lease handoff must not produce another terminal status"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "a child with no live lease stayed running after the inherited descriptor closed"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(
            interrupted.status,
            SubagentStatus::Interrupted,
            "a child with no live lease is an orphan from a previous run"
        );
    }

    #[test]
    fn cleanup_refuses_a_terminal_child_whose_lease_another_process_holds() {
        // The dangerous ordering is not "cleanup sees a running child" -- that
        // is already refused -- but "cleanup sees a terminal child, then a
        // second Maestro resumes it". `resume` takes the execution lease before
        // it rewrites the record, so a cleanup that does not hold the lease can
        // pass its status check and then force-remove a worktree the resumed
        // child is working in.
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let session_dir = root.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session directory should exist");
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: "/workspace".to_string(),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&session_dir),
            // Terminal, so the status check on its own would let cleanup run.
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        // Stands in for a second Maestro process that has just resumed this
        // child: it holds the lease, and this process's runtime registry never
        // saw the child.
        let lease = SessionLock::acquire(&SubagentManager::timeline_path(&record))
            .expect("execution lease should be held");

        let cleaned = manager.cleanup(&serde_json::json!({"subagent_id": id}));
        assert!(
            !cleaned.success,
            "cleanup must refuse a terminal child whose execution lease is held"
        );
        assert!(
            !manager
                .load_record(&id)
                .expect("record should load")
                .worktree_cleaned,
            "a refused cleanup must leave the record alone"
        );

        drop(lease);

        // Parallel fixtures can fork while this test owns the lease. Their
        // inherited descriptor keeps the advisory lock live until the child
        // execs or exits, so wait only for that expected contention instead of
        // treating a just-released local guard as an immediate global unlock.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let cleaned = manager.cleanup(&serde_json::json!({"subagent_id": id}));
            if cleaned.success {
                break;
            }
            assert!(
                cleaned
                    .error
                    .as_deref()
                    .is_some_and(|error| { error.contains("is being run by another process") }),
                "cleanup failed for a reason other than an inherited lease: {cleaned:?}"
            );
            assert!(
                Instant::now() < deadline,
                "cleanup must proceed after every inherited lease closes: {cleaned:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // This fixture is isolation=shared (no worktree). Cleanup is a no-op
        // and must not flip worktree_cleaned, which would block resume.
        assert!(
            !manager
                .load_record(&id)
                .expect("record should load")
                .worktree_cleaned,
            "no-op cleanup must not mark a shared child as cleaned"
        );
    }

    #[test]
    fn a_cleaned_child_cannot_be_resumed() {
        // `cleanup` holds the execution lease across its check and removal, so
        // a resume that acquires the lease afterwards is looking at a record it
        // read before the removal. Relaunching on that stale copy would put the
        // child in a directory that no longer exists.
        let root = tempfile::tempdir().expect("records root should exist");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace should exist");
        let manager = SubagentManager::with_root(workspace.clone(), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let session_dir = root.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session directory should exist");
        let mut record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(&workspace),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&session_dir),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        assert!(
            manager.revalidate_resumable_under_lease(&id).is_ok(),
            "an uncleaned child with a live cwd is resumable"
        );

        record.worktree_cleaned = true;
        manager
            .write_record(&record)
            .expect("record should persist");

        let refused = manager
            .revalidate_resumable_under_lease(&id)
            .expect_err("a cleaned child must not be resumable");
        assert!(refused.contains("worktree was cleaned up"), "{refused}");
    }

    #[test]
    fn a_child_whose_working_directory_is_gone_cannot_be_resumed() {
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let session_dir = root.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session directory should exist");
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: serialize_repository_path(&root.path().join("removed-worktree")),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&session_dir),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        let refused = manager
            .revalidate_resumable_under_lease(&id)
            .expect_err("a missing working directory must refuse the resume");
        assert!(refused.contains("no longer exists"), "{refused}");
    }

    #[test]
    fn cleanup_proceeds_when_the_session_directory_is_already_gone() {
        // The lock file lives in the session directory, so a pruned transcript
        // means no lease can exist and no resume can start one. Cleanup must
        // stay usable rather than refusing forever.
        let root = tempfile::tempdir().expect("records root should exist");
        let manager =
            SubagentManager::with_root(PathBuf::from("/workspace"), root.path().join("records"));
        let id = uuid::Uuid::new_v4().to_string();
        let record = SubagentRecord {
            parent_requests: Vec::new(),
            id: id.clone(),
            parent_scope_id: "parent-scope".to_string(),
            parent_call_id: "call-1".to_string(),
            last_parent_scope_id: "parent-scope".to_string(),
            last_call_id: "call-1".to_string(),
            task: "inspect".to_string(),
            current_prompt: "inspect".to_string(),
            role: SubagentRole::Explore,
            backend: SubagentBackend::Native,
            orb: None,
            profile: None,
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            isolation: SubagentIsolation::Shared,
            cwd: "/workspace".to_string(),
            worktree_path: None,
            worktree_cleaned: false,
            initial_files: Vec::new(),
            initial_file_fingerprints: HashMap::new(),
            initial_head: None,
            session_dir: serialize_repository_path(&root.path().join("missing-session")),
            status: SubagentStatus::Completed,
            attempt: 1,
            snapshot_attempt: None,
            created_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            result: None,
            error: None,
            lifecycle_notification_published: false,
        };
        manager
            .write_record(&record)
            .expect("record should persist");

        let cleaned = manager.cleanup(&serde_json::json!({"subagent_id": id}));

        assert!(cleaned.success, "cleanup: {}", cleaned.output);
    }

    #[test]
    fn child_tool_set_excludes_parent_global_tools() {
        let tools = child_allowed_tools_for_role(SubagentRole::Code, None);
        for name in SUBAGENT_TOOL_NAMES {
            assert!(!tools.contains(name), "child advertised {name}");
        }
        assert!(!tools.contains("get_goal"));
        assert!(!tools.contains("update_goal"));
        assert!(!tools.contains("todo"));
        assert!(!tools.contains("background_tasks"));
        assert!(tools.contains("read"));
    }

    #[test]
    fn role_tool_policies_prevent_read_only_roles_from_mutating() {
        for role in [
            SubagentRole::Explore,
            SubagentRole::Plan,
            SubagentRole::Review,
        ] {
            let tools = child_allowed_tools_for_role(role, None);
            assert!(tools.contains("read"), "{role:?} should read files");
            assert!(tools.contains("grep"), "{role:?} should search files");
            assert!(
                !tools.contains("bash"),
                "{role:?} must not run shell commands"
            );
            assert!(!tools.contains("write"), "{role:?} must not write files");
            assert!(!tools.contains("edit"), "{role:?} must not edit files");
        }

        let code_tools = child_allowed_tools_for_role(SubagentRole::Code, None);
        assert!(code_tools.contains("bash"));
        assert!(code_tools.contains("write"));
        assert!(code_tools.contains("edit"));
    }

    #[test]
    fn role_default_profile_routes_model_without_widening_tools() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".maestro/agent-profiles");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("role-explore.md"),
            "---\nname: role-explore\nmodel: small-model\ntools: [read, write]\n---\nReturn exact locations.").unwrap();
        let mut request = parse_spawn_request(&serde_json::json!({
            "task":"Find the parser","role":"explore"
        }))
        .unwrap();
        resolve_spawn_profile_with_trust(&mut request, root.path(), false).unwrap();
        assert!(
            request.profile.is_none(),
            "untrusted project cannot select a model"
        );
        resolve_spawn_profile_with_trust(&mut request, root.path(), true).unwrap();
        assert_eq!(request.model.as_deref(), Some("small-model"));
        assert_eq!(
            child_allowed_tools_for_role(request.role, request.profile_tools.as_deref()),
            HashSet::from(["read".to_string()])
        );
        let mut explicit = parse_spawn_request(&serde_json::json!({
            "task":"Find the parser","role":"explore","model":"chosen-model"
        }))
        .unwrap();
        resolve_spawn_profile_with_trust(&mut explicit, root.path(), true).unwrap();
        assert_eq!(explicit.model.as_deref(), Some("chosen-model"));
    }

    #[test]
    fn profile_tools_only_narrow_a_role_policy() {
        let requested = ["read".to_string(), "write".to_string(), "bash".to_string()];
        let explore_tools = child_allowed_tools_for_role(SubagentRole::Explore, Some(&requested));
        assert_eq!(explore_tools, HashSet::from(["read".to_string()]));

        let code_tools = child_allowed_tools_for_role(SubagentRole::Code, Some(&requested));
        assert_eq!(
            code_tools,
            HashSet::from(["read".to_string(), "write".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn project_profile_resolves_prompt_tools_and_model_for_a_child() {
        let root = tempfile::tempdir().expect("profile root should exist");
        let profile_dir = root.path().join(".maestro/agent-profiles");
        std::fs::create_dir_all(&profile_dir).expect("profile directory should be created");
        std::fs::write(
            profile_dir.join("rust-reviewer.md"),
            "---\nname: rust-reviewer\ntools: [read, grep]\nmodel: review-model\n---\n\nFocus on correctness and regressions.\n",
        )
        .expect("profile should be written");

        let mut request = SpawnRequest {
            task: "review the change".to_string(),
            role: SubagentRole::Review,
            backend: SubagentBackend::Native,
            orb: OrbDelegationConfig::default(),
            profile: Some("rust-reviewer".to_string()),
            profile_prompt: None,
            profile_tools: None,
            model: None,
            thinking: None,
            difficulty: TaskDifficulty::Medium,
            timeout_ms: DEFAULT_CHILD_TIMEOUT_MS,
            max_tokens: DEFAULT_CHILD_MAX_TOKENS,
            run_in_background: true,
            isolation: SubagentIsolation::Shared,
            worktree_name: None,
        };
        resolve_spawn_profile_with_trust(&mut request, root.path(), true)
            .expect("profile should resolve");

        assert_eq!(request.profile.as_deref(), Some("rust-reviewer"));
        assert_eq!(
            request.profile_prompt.as_deref(),
            Some("Focus on correctness and regressions.")
        );
        assert_eq!(
            request.profile_tools,
            Some(vec!["read".to_string(), "grep".to_string()])
        );
        assert_eq!(request.model.as_deref(), Some("review-model"));
    }

    #[test]
    fn orb_delegation_keeps_placement_policy_out_of_the_request() {
        let mut request = parse_spawn_request(&serde_json::json!({
            "task": "inspect the repository",
            "role": "explore",
            "backend": "orb",
            "orb": {
                "project": "demo",
                "repository": "https://github.com/example/example"
            }
        }))
        .expect("high-level Computer intent should parse");
        apply_orb_delegation_policy(&mut request);
        assert_eq!(request.orb.project.as_deref(), Some("demo"));
        assert_eq!(
            request.orb.settings.repository_url.as_deref(),
            Some("https://github.com/example/example")
        );
        assert_eq!(request.orb.settings.resource_profile, None);
        assert_eq!(request.orb.settings.provisioner, None);
        assert_eq!(request.orb.settings.machine, None);

        let error = parse_spawn_request(&serde_json::json!({
            "task": "inspect the repository",
            "backend": "orb",
            "orb": {"resource_profile": "large"}
        }))
        .expect_err("raw placement settings must stay policy-owned");
        assert!(error.contains("policy-owned infrastructure"));

        let error = parse_spawn_request(&serde_json::json!({
            "task": "inspect the repository",
            "backend": "orb",
            "orb": {"profile": "large profile"}
        }))
        .expect_err("profile overrides must be bounded ids");
        assert!(error.contains("bounded hosted profile id"));
    }

    #[test]
    fn computer_delegation_is_the_canonical_model_facing_backend() {
        let request = parse_spawn_request(&serde_json::json!({
            "task": "implement the requested change",
            "backend": "computer",
            "computer": {
                "project": "demo",
                "repository": "https://github.com/example/example"
            }
        }))
        .expect("canonical Computer delegation should parse");

        assert_eq!(request.backend, SubagentBackend::Orb);
        assert_eq!(request.orb.project.as_deref(), Some("demo"));
        assert_eq!(
            request.orb.repository.as_deref(),
            Some("https://github.com/example/example")
        );

        let error = parse_spawn_request(&serde_json::json!({
            "task": "inspect the repository",
            "backend": "computer",
            "computer": {"project": "demo"},
            "orb": {"project": "legacy"}
        }))
        .expect_err("canonical and compatibility config must not be ambiguous");
        assert!(error.contains("only one hosted Computer configuration"));
    }

    #[test]
    fn hosted_computer_context_infers_https_origin_and_project() {
        let root = tempfile::tempdir().expect("temporary workspace should exist");
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .expect("git should be available")
            .success()
            .then_some(())
            .expect("temporary workspace should be a git repository");
        Command::new("git")
            .args([
                "config",
                "remote.origin.url",
                "git@github.com:evalops/mono.git",
            ])
            .current_dir(root.path())
            .status()
            .expect("git config should be available")
            .success()
            .then_some(())
            .expect("origin should be configured");

        let mut config = OrbDelegationConfig::default();
        infer_hosted_computer_context(&mut config, root.path())
            .expect("hosted Computer context should be inferred");

        assert_eq!(config.project.as_deref(), Some("evalops/mono"));
        assert_eq!(
            config.repository.as_deref(),
            Some("https://github.com/evalops/mono.git")
        );
    }

    #[test]
    fn hosted_computer_context_never_infers_from_a_credential_bearing_origin() {
        let root = tempfile::tempdir().expect("temporary workspace should exist");
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .expect("git should be available")
            .success()
            .then_some(())
            .expect("temporary workspace should be a git repository");
        Command::new("git")
            .args([
                "config",
                "remote.origin.url",
                "https://user:secret@github.com/evalops/mono.git",
            ])
            .current_dir(root.path())
            .status()
            .expect("git config should be available")
            .success()
            .then_some(())
            .expect("origin should be configured");

        let mut config = OrbDelegationConfig::default();
        let error = infer_hosted_computer_context(&mut config, root.path())
            .expect_err("credential-bearing origins must fail closed");
        assert!(error.contains("embedded credentials"));
    }

    #[test]
    fn hosted_computer_context_fails_closed_without_a_remote_origin() {
        let root = tempfile::tempdir().expect("temporary workspace should exist");
        let mut config = OrbDelegationConfig::default();

        let error = infer_hosted_computer_context(&mut config, root.path())
            .expect_err("a workspace without an origin must not pick a default project");
        assert!(error.contains("remote repository"));
    }

    #[test]
    fn hosted_computer_context_validates_explicit_repository_even_with_project() {
        let root = tempfile::tempdir().expect("temporary workspace should exist");
        let mut config = OrbDelegationConfig {
            project: Some("demo".to_string()),
            repository: Some("http://github.com/example/example".to_string()),
            ..OrbDelegationConfig::default()
        };

        let error = infer_hosted_computer_context(&mut config, root.path())
            .expect_err("hosted Computer launches must require HTTPS repositories");
        assert!(error.contains("must use HTTPS"));
    }

    #[test]
    fn orb_repository_intent_rejects_malformed_urls() {
        let error = parse_spawn_request(&serde_json::json!({
            "task": "inspect the repository",
            "backend": "orb",
            "orb": {"repository": "https://secret@"}
        }))
        .expect_err("malformed repository URLs must fail before delegation");

        assert!(error.contains("valid absolute URL"));
    }

    #[test]
    fn child_credential_scope_survives_parent_reset_for_resume() {
        let runtime = RuntimeRegistry::new();
        let parent = CredentialVault::new();
        let child = parent.fork();
        runtime.set_credential_scope("child", child.clone());

        let reference = child.store(
            "discovered-child-secret",
            crate::agent::CredentialType::Secret,
        );
        parent.clear();

        let resumed = runtime
            .credential_scope("child")
            .expect("child credential scope should be retained");
        assert_eq!(resumed.resolve_all(&reference), "discovered-child-secret");
    }

    #[test]
    fn resume_rekeys_new_parent_credentials_into_child_scope() {
        let parent = CredentialVault::new();
        let child = parent.fork();
        let child_reference = child.store(
            "discovered-child-secret",
            crate::agent::CredentialType::Secret,
        );
        let mappings = parent.absorb_child_credentials_at_generation(&child, parent.generation());
        let parent_reference = mappings
            .get(&child_reference)
            .expect("child credential should be imported into parent");

        let incoming_task = format!("reuse {parent_reference}");
        let parent_resolved_task = parent.resolve_all(&incoming_task);
        let child_task = child.vault_in_text(&parent_resolved_task);

        assert!(!child_task.contains("discovered-child-secret"));
        assert_eq!(
            child.resolve_all(&child_task),
            incoming_task.replace(parent_reference, "discovered-child-secret",)
        );
    }

    #[test]
    fn controls_rekey_new_parent_credentials_into_child_scope() {
        let parent = CredentialVault::new();
        let child = parent.fork();
        let parent_reference =
            parent.store("new-parent-secret", crate::agent::CredentialType::Secret);
        let incoming = format!("use {parent_reference}");

        let prepared = prepare_control_body(&parent, Some(&child), &incoming)
            .expect("same-process control should re-key credentials");
        assert!(!prepared.contains("new-parent-secret"));
        assert_eq!(child.resolve_all(&prepared), "use new-parent-secret");
        assert!(prepare_control_body(&parent, None, &incoming).is_err());
    }

    #[test]
    fn interrupt_has_a_distinct_terminal_status() {
        let (status, error) = child_terminal_status(
            false,
            true,
            false,
            Some("turn interrupted".to_string()),
            DEFAULT_CHILD_TIMEOUT_MS,
        );
        assert_eq!(status, SubagentStatus::Interrupted);
        assert_eq!(error.as_deref(), Some("subagent interrupted"));
    }

    #[test]
    fn fallback_history_restores_prompt_when_snapshot_is_missing() {
        let history = fallback_history_from_prompt("original delegated task")
            .expect("prompt should become fallback history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, crate::ai::Role::User);
        assert_eq!(
            history[0].content.as_text(),
            Some("original delegated task")
        );
        assert!(fallback_history_from_prompt("  ").is_none());
    }

    #[test]
    fn restore_history_augments_stale_snapshot_with_interrupted_prompt() {
        let prior = crate::ai::Message {
            role: crate::ai::Role::User,
            content: crate::ai::MessageContent::text("original delegated task"),
        };
        let restored = restore_history_with_prompt(
            Some(vec![prior.clone()]),
            "interrupted follow-up",
            2,
            None,
        )
        .expect("history should be restored");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[1].content.as_text(), Some("interrupted follow-up"));

        let already_recorded = restore_history_with_prompt(
            Some(vec![
                prior,
                crate::ai::Message {
                    role: crate::ai::Role::User,
                    content: crate::ai::MessageContent::text("interrupted follow-up"),
                },
            ]),
            "interrupted follow-up",
            2,
            Some(2),
        )
        .expect("history should remain available");
        assert_eq!(already_recorded.len(), 2);
    }

    #[test]
    fn restore_history_appends_repeated_prompt_from_an_older_attempt() {
        let prompt = "repeat the same delegated task";
        let history = vec![crate::ai::Message {
            role: crate::ai::Role::User,
            content: crate::ai::MessageContent::text(prompt),
        }];
        let current_snapshot =
            restore_history_with_prompt(Some(history.clone()), prompt, 1, Some(1))
                .expect("current snapshot should be restored");
        assert_eq!(current_snapshot.len(), 1);

        let restored = restore_history_with_prompt(Some(history), prompt, 2, Some(1))
            .expect("history should be restored");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[1].content.as_text(), Some(prompt));
    }

    #[test]
    fn child_transcript_preserves_resume_checkpoint() {
        let root = tempfile::tempdir().expect("temp root");
        let session_dir = root.path().join("session");
        let session_id = uuid::Uuid::new_v4().to_string();
        let messages = vec![crate::ai::Message {
            role: crate::ai::Role::User,
            content: crate::ai::MessageContent::text("inspect the parser"),
        }];
        let event = FromAgent::ConversationSnapshot {
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_string(),
            messages: messages.clone(),
            processed_queue_ids: Vec::new(),
        };
        let received =
            child_event_to_headless(&event, &session_id).expect("snapshot should be recordable");

        let mut recorder =
            SessionRecorder::with_id(&session_dir, &session_id).expect("create transcript");
        recorder
            .record_received(&received)
            .expect("record snapshot");
        recorder.flush_checkpoint().expect("flush transcript");

        let resumed =
            SessionRecorder::resume(&session_dir, &session_id).expect("resume transcript");
        let restored = resumed
            .replay()
            .semantic_conversation
            .expect("semantic checkpoint");
        assert_eq!(
            serde_json::to_value(restored).expect("serialize restored messages"),
            serde_json::to_value(messages).expect("serialize source messages")
        );
    }

    #[test]
    fn child_snapshot_persists_attempt_with_resume_checkpoint() {
        let root = tempfile::tempdir().expect("temp root");
        let session_id = uuid::Uuid::new_v4().to_string();
        let event = FromAgent::ConversationSnapshot {
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_string(),
            messages: vec![crate::ai::Message {
                role: crate::ai::Role::User,
                content: crate::ai::MessageContent::text("inspect the parser"),
            }],
            processed_queue_ids: vec![42],
        };

        let mut recorder =
            SessionRecorder::with_id(root.path(), &session_id).expect("create transcript");
        persist_child_event(
            &mut recorder,
            &event,
            &session_id,
            &CredentialVault::new(),
            7,
        )
        .expect("record snapshot attempt and queue watermark");
        recorder.flush_checkpoint().expect("flush transcript");
        drop(recorder);

        let resumed = SessionRecorder::resume(root.path(), &session_id).expect("resume transcript");
        assert_eq!(resumed.semantic_conversation_attempt(), Some(7));
        assert!(resumed.semantic_processed_queue_ids().contains(&42));
        assert!(resumed.replay().semantic_conversation.is_some());
    }

    #[test]
    fn drains_shutdown_snapshot_into_resume_checkpoint() {
        let root = tempfile::tempdir().expect("temp root");
        let session_id = uuid::Uuid::new_v4().to_string();
        let messages = vec![
            crate::ai::Message {
                role: crate::ai::Role::User,
                content: crate::ai::MessageContent::text("inspect the parser"),
            },
            crate::ai::Message {
                role: crate::ai::Role::Assistant,
                content: crate::ai::MessageContent::text("I found the parser boundary."),
            },
        ];
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(FromAgent::ConversationSnapshot {
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_string(),
                messages: messages.clone(),
                processed_queue_ids: Vec::new(),
            })
            .expect("queue shutdown snapshot");
        drop(sender);

        let mut recorder =
            SessionRecorder::with_id(root.path(), &session_id).expect("create transcript");
        drain_child_events(
            &mut recorder,
            &mut events,
            &session_id,
            &CredentialVault::new(),
            1,
        )
        .expect("drain shutdown snapshot");
        recorder.flush_checkpoint().expect("flush transcript");

        let restored = SessionRecorder::resume(root.path(), &session_id)
            .expect("resume transcript")
            .replay()
            .semantic_conversation
            .expect("semantic checkpoint");
        assert_eq!(
            serde_json::to_value(restored).expect("serialize restored messages"),
            serde_json::to_value(messages).expect("serialize source messages")
        );
    }

    #[test]
    fn child_snapshots_revault_resolved_prompt_content_before_persistence() {
        let root = tempfile::tempdir().expect("temp root");
        let vault = CredentialVault::new();
        let reference = vault.store("arbitrary-secret", crate::agent::CredentialType::Password);
        let session_id = uuid::Uuid::new_v4().to_string();
        let event = FromAgent::ConversationSnapshot {
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_string(),
            messages: vec![crate::ai::Message {
                role: crate::ai::Role::User,
                content: crate::ai::MessageContent::text("Use arbitrary-secret now"),
            }],
            processed_queue_ids: Vec::new(),
        };
        let message = child_event_to_headless(&event, "child").expect("snapshot should map");
        let vaulted = vault_headless_message(&message, &vault).expect("snapshot should re-vault");
        let serialized = serde_json::to_string(&vaulted).expect("snapshot should serialize");
        assert!(serialized.contains(&reference));
        assert!(!serialized.contains("arbitrary-secret"));

        let mut recorder =
            SessionRecorder::with_id(root.path(), &session_id).expect("create transcript");
        recorder
            .record_received_preserving_credential_references(&vaulted)
            .expect("record vaulted snapshot");
        recorder.flush_checkpoint().expect("flush transcript");
        let resumed = SessionRecorder::resume(root.path(), &session_id).expect("resume transcript");
        let restored = resumed
            .replay()
            .semantic_conversation
            .expect("semantic checkpoint");
        let restored_text = restored[0].content.as_text().expect("restored prompt text");
        assert_eq!(restored_text, format!("Use {reference} now"));
        assert_eq!(vault.resolve_all(restored_text), "Use arbitrary-secret now");
    }

    #[test]
    fn managed_gateway_receipt_survives_subagent_forwarding() {
        let mapped = child_event_to_headless(
            &FromAgent::ManagedGatewayReceipt {
                request_id: "request-child".to_string(),
                record_id: "record-child".to_string(),
                lineage_id: "lineage-child".to_string(),
                record_status: "planned".to_string(),
                provider_prompt_sha256: None,
                provider_tools_sha256: None,
                provider_tool_count: None,
            },
            "child-session",
        )
        .expect("managed receipt should be forwarded");

        assert!(matches!(
            mapped,
            FromAgentMessage::ManagedGatewayReceipt {
                request_id,
                record_id,
                lineage_id,
                record_status,
                ..
            } if request_id == "request-child"
                && record_id == "record-child"
                && lineage_id == "lineage-child"
                && record_status == "planned"
        ));
    }

    #[test]
    fn local_assistant_content_cannot_complete_child_checkpoint() {
        let root = tempfile::tempdir().expect("temp root");
        let session_id = uuid::Uuid::new_v4().to_string();
        let vault = CredentialVault::new();
        let mut recorder = SessionRecorder::with_id(root.path(), &session_id).unwrap();
        let local = FromAgent::LocalAssistantContent {
            response_id: "response".to_owned(),
            content: vec![crate::ai::ContentBlock::Text {
                text: "partial answer".to_owned(),
            }],
        };
        let mut seen = persist_child_event(&mut recorder, &local, &session_id, &vault, 1).unwrap();
        assert!(!terminal_checkpoint_ready(true, seen));
        assert!(recorder.replay().semantic_conversation.is_none());
        let snapshot = FromAgent::ConversationSnapshot {
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_owned(),
            messages: vec![crate::ai::Message {
                role: crate::ai::Role::Assistant,
                content: crate::ai::MessageContent::text("complete answer"),
            }],
            processed_queue_ids: vec![42],
        };
        seen |= persist_child_event(&mut recorder, &snapshot, &session_id, &vault, 1).unwrap();
        assert!(terminal_checkpoint_ready(true, seen));
        recorder.flush_checkpoint().unwrap();
        let resumed = SessionRecorder::resume(root.path(), &session_id).unwrap();
        assert!(resumed.replay().semantic_conversation.is_some());
        assert!(resumed.semantic_processed_queue_ids().contains(&42));
    }

    #[test]
    fn preterminal_child_snapshot_avoids_legacy_terminal_wait() {
        assert!(!terminal_checkpoint_ready(false, true));
        assert!(terminal_checkpoint_ready(true, true));
        assert!(!terminal_checkpoint_ready(true, false));
    }
}
