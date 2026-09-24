//! Single-session hosted runner HTTP surface for Platform-managed runtimes.
//!
//! This module is the Rust-owned counterpart to Maestro's TypeScript hosted
//! runner server. It intentionally exposes the same provider-neutral HTTP
//! contract so Platform and conformance tests can target a Rust runtime without
//! routing through the Node web server.

use maestro_runtime_contracts::tool_wire;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use maestro_runtime::{
    RuntimeLifecycleState, RuntimeReceipt, RuntimeReceiptInput, RuntimeReceiptKind,
    RuntimeTerminalClassification, TraceHeaders, headless_protocol_capability_digest,
    record_outcome, route_class, server_span,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::headless::controller_binding::{
    ControllerBindingReceipt, ControllerScopeExpectation, controller_binding_from_hello_extension,
};
use crate::headless::messages::{
    ClientCapabilities, ClientInfo, ConnectionRole, ConnectionState, FromAgentMessage,
    HEADLESS_PROTOCOL_VERSION, InitConfig, ServerCapabilities, ServerRequestType, ThinkingLevel,
    ToAgentMessage, UtilityCommandShellMode, UtilityCommandStream, UtilityCommandTerminalMode,
    UtilityFileSearchMatch, UtilityOperation,
};
use crate::headless::{
    AgentState, AgentSupervisor, AsyncTransportError, ResponseAcknowledgement, SessionReplay,
    native_server_capabilities, response_ack_request_id,
};
use crate::headless_server::{GovernedGrantVerificationContext, verify_governed_tool_grant};

mod config;
mod env_aliases;
mod handle;
mod initial_actions;
mod manifests;
pub mod rendezvous_carrier;
pub mod rendezvous_protocol;
pub mod rendezvous_runtime;
mod shared;
mod snapshots;
mod thread_protocol;
mod workload_identity;

use self::{manifests::*, snapshots::*, thread_protocol::*};
pub use config::{HostedRunnerConfig, HostedRunnerConfigError};
pub use env_aliases::{DEPRECATED_ENV_ALIASES, DeprecatedEnvAlias};
pub use handle::{HostedRunnerHandle, HostedRunnerIdentity};

pub const HOSTED_RUNNER_IDENTITY_PATH: &str = "/.well-known/evalops/remote-runner/identity";
pub const HOSTED_RUNNER_DRAIN_PATH: &str = "/.well-known/evalops/remote-runner/drain";
pub const HOSTED_RUNNER_RECEIPT_PATH: &str = "/.well-known/evalops/remote-runner/receipt";

pub const HOSTED_RUNNER_IDENTITY_PROTOCOL_VERSION: &str = "evalops.remote-runner.identity.v1";
pub const HOSTED_RUNNER_DRAIN_PROTOCOL_VERSION: &str = "evalops.remote-runner.drain.v1";
pub const HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION: &str =
    "evalops.remote-runner.snapshot-manifest.v1";
pub const HOSTED_RUNNER_RETENTION_POLICY_VERSION: &str = "evalops.remote-runner.retention.v1";
pub const HOSTED_RUNNER_WORK_CONTINUITY_VERSION: &str = "evalops.remote-runner.work-continuity.v1";
pub const HOSTED_RUNNER_PLATFORM_EVIDENCE_VERSION: &str =
    "evalops.remote-runner.platform-evidence.v1";
pub const HOSTED_RUNNER_RUNTIME_CONTINUITY_VERSION: &str =
    "evalops.remote-runner.runtime-continuity.v1";
pub(super) const HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS: &str =
    "Drain finalization pending";

const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 15_000;
const CONNECTION_IDLE_MS: i64 = (DEFAULT_HEARTBEAT_INTERVAL_MS as i64) * 3;
const MAINTENANCE_PUMP_INTERVAL: Duration = Duration::from_millis(100);
// Connected transports notify the pump after enqueueing every event. Keep a
// slower safety drain for missed notifications without waking 10 times a
// second throughout an otherwise idle hosted session.
const NOTIFIED_MAINTENANCE_PUMP_INTERVAL: Duration = Duration::from_secs(5);
const THREAD_PERSISTENCE_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_EVENTS: usize = 1024;
// Response retries are short-lived transport retries; retain enough completed
// keys to cover a burst without allowing a long-lived hosted session's journal
// to grow without bound.
const MAX_RESPONSE_IDEMPOTENCY_RECORDS: usize = 4096;
const RESPONSE_ACK_TIMEOUT: Duration = Duration::from_millis(500);

pub(super) const HOSTED_RUNNER_IDENTITY_BINDING_PROTOCOL_VERSION: &str =
    "evalops.maestro.hosted-runner-identity-binding.v1";
const MAX_IDENTITY_BINDING_FAILURES: usize = 64;
const MAX_IDENTITY_BINDING_FAILURE_ID_BYTES: usize = 256;

fn bounded_identity_binding_id(value: &str) -> String {
    if value.len() <= MAX_IDENTITY_BINDING_FAILURE_ID_BYTES {
        return value.to_string();
    }
    let marker = "…";
    let mut end = MAX_IDENTITY_BINDING_FAILURE_ID_BYTES.saturating_sub(marker.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], marker)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunnerSessionId(String);

impl RunnerSessionId {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaestroSessionId(String);

impl MaestroSessionId {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
struct HostedRunnerBinding {
    runner_session_id: RunnerSessionId,
    maestro_session_id: MaestroSessionId,
}

impl HostedRunnerBinding {
    fn from_config(
        config: &HostedRunnerConfig,
        restore_manifest: Option<&SnapshotManifest>,
    ) -> Self {
        let maestro_session_id = config
            .maestro_session_id
            .clone()
            .or_else(|| restore_manifest.map(|manifest| manifest.maestro_session_id.clone()))
            .unwrap_or_else(|| config.runner_session_id.clone());
        Self {
            runner_session_id: RunnerSessionId::new(config.runner_session_id.clone()),
            maestro_session_id: MaestroSessionId::new(maestro_session_id),
        }
    }

    fn matches(&self, requested: &str) -> bool {
        requested == self.runner_session_id.as_str()
            || requested == self.maestro_session_id.as_str()
    }
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct IdentityBindingFailure {
    pub(super) protocol_version: String,
    pub(super) operation: String,
    pub(super) requested_session_id: String,
    pub(super) expected_runner_session_id: String,
    pub(super) bound_maestro_session_id: String,
    pub(super) runtime_generation: u64,
}

impl IdentityBindingFailure {
    fn new(
        binding: &HostedRunnerBinding,
        operation: &'static str,
        requested_session_id: &str,
        runtime_generation: u64,
    ) -> Self {
        Self {
            protocol_version: HOSTED_RUNNER_IDENTITY_BINDING_PROTOCOL_VERSION.to_string(),
            operation: operation.to_string(),
            requested_session_id: bounded_identity_binding_id(requested_session_id),
            expected_runner_session_id: binding.runner_session_id.as_str().to_string(),
            bound_maestro_session_id: binding.maestro_session_id.as_str().to_string(),
            runtime_generation,
        }
    }

    fn details(&self) -> serde_json::Value {
        json!({ "identity_binding": self })
    }
}
#[derive(Clone)]
struct SharedRunner {
    binding: HostedRunnerBinding,
    config: Arc<HostedRunnerConfig>,
    state: Arc<Mutex<RunnerState>>,
    events: broadcast::Sender<StreamEnvelope>,
    controller_events: broadcast::Sender<StreamEnvelope>,
    message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
    rendezvous_outbound_authority: Arc<AtomicBool>,
    thread_journal: Arc<ThreadJournal>,
    mutation_lifecycle: Arc<tokio::sync::Mutex<()>>,
    thread_persistence_retry_pending: Arc<AtomicBool>,
    thread_persistence_recovery_running: Arc<AtomicBool>,
    thread_persistence_recovery_cancellation: CancellationToken,
    thread_persistence_recovery_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    #[cfg(test)]
    thread_persistence_failures: Arc<Mutex<usize>>,
    #[cfg(test)]
    executor_drain_result_persistence_failures: Arc<Mutex<usize>>,
    event_pump_cancellation: CancellationToken,
    event_pump_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

struct RunnerState {
    ready: bool,
    draining: bool,
    /// The executor has completed drain, but the final journal hand-off
    /// failed. A later drain request retries only that durable boundary.
    drain_finalization_pending: bool,
    drain_runtime_failed_before_finalization: bool,
    /// The durable Draining boundary exists, but executor ownership has not
    /// completed its hand-off. A failed executor drain remains retryable.
    drain_executor_pending: bool,
    /// The executor-complete result is retained until the final Drained
    /// snapshot is durable. It is also mirrored in a private generation-bound
    /// handoff file so a process crash cannot cause a second executor drain.
    pending_executor_drain_result: Option<PendingExecutorDrainResult>,
    /// Leading messages from the retained executor result that were applied
    /// before the durable executor-complete boundary. The journal stores this
    /// position so restart recovery does not infer progress from bounded
    /// replay envelopes.
    executor_drain_result_applied_count: usize,
    executor_drain_result_applied: bool,
    runtime_failed: bool,
    /// A restore manifest whose runtime flush was incomplete is a permanent
    /// not-ready fence for this generation; a later child Ready event cannot
    /// turn the incomplete restore into execution-ready evidence.
    restore_incomplete: bool,
    session_id: String,
    cursor: u64,
    last_init: Option<InitConfig>,
    last_status: Option<String>,
    last_error: Option<String>,
    last_error_type: Option<String>,
    provider_error_kind: Option<maestro_ai::ProviderStreamErrorKind>,
    identity_binding_failures: VecDeque<IdentityBindingFailure>,
    restored_snapshot: Option<RuntimeSnapshot>,
    restored_snapshot_lineage: Option<String>,
    /// Durable model binding used for lifecycle receipts across restart.
    runtime_model_binding: Option<String>,
    /// Durable provider binding used for lifecycle receipts across restart.
    runtime_provider_binding: Option<String>,
    runtime_receipt: Option<RuntimeReceipt>,
    controller_binding: Option<ControllerBindingReceipt>,
    controller_connection_id: Option<String>,
    controller_stream_cancellation: CancellationToken,
    connections: HashMap<String, ConnectionRecord>,
    subscriptions: HashMap<String, SubscriptionRecord>,
    active_utility_commands: HashMap<String, ActiveUtilityCommandSnapshot>,
    active_file_watches: HashMap<String, ActiveFileWatchSnapshot>,
    active_response_ids: HashSet<String>,
    response_idempotency_keys: HashSet<String>,
    response_idempotency_digests: HashMap<String, String>,
    response_request_owners: HashMap<String, String>,
    pending_response_idempotency: HashMap<String, ToAgentMessage>,
    response_idempotency_order: VecDeque<String>,
    pending_response_idempotency_order: VecDeque<String>,
    envelopes: VecDeque<StreamEnvelope>,
    controller_envelopes: VecDeque<StreamEnvelope>,
    pending_controller_events: VecDeque<FromAgentMessage>,
    thread: ThreadProtocolState,
}

#[derive(Debug, Clone)]
struct PendingExecutorDrainResult {
    replay_cursor: u64,
    result: HostedRunnerDrainResult,
}

fn runtime_snapshot_is_failed(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.state.last_status.as_deref() == Some("Runtime failed")
        || snapshot.state.last_error_type.as_deref() == Some("fatal")
}

fn runtime_availability_error(state: &RunnerState, message: &'static str) -> HostedError {
    if state.runtime_failed {
        HostedError::new(
            HostedRunnerErrorCode::RuntimeFailed,
            "hosted runner runtime failed",
        )
    } else {
        HostedError::new(HostedRunnerErrorCode::RuntimeNotReady, message)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionAuthorityMode {
    LegacySubscription,
    Capability,
}

#[derive(Clone)]
struct ConnectionRecord {
    id: String,
    connection_capability: Option<String>,
    authority_mode: ConnectionAuthorityMode,
    role: ConnectionRole,
    client_protocol_version: Option<String>,
    client_info: Option<ClientInfo>,
    capabilities: Option<ClientCapabilities>,
    opt_out_notifications: Vec<String>,
    subscription_ids: HashSet<String>,
    last_seen_at: DateTime<Utc>,
}

struct ConnectionUpsert {
    connection_id: String,
    connection_capability: Option<String>,
    connection_capability_required: bool,
    role: ConnectionRole,
    client_protocol_version: Option<String>,
    client_info: Option<ClientInfo>,
    capabilities: Option<ClientCapabilities>,
    opt_out_notifications: Vec<String>,
    take_control: bool,
}

#[derive(Clone)]
struct SubscriptionRecord {
    connection_id: String,
    connection_capability: Option<String>,
    authority_mode: ConnectionAuthorityMode,
    role: ConnectionRole,
    attached: bool,
}

#[derive(Debug, Deserialize)]
struct ConnectionCreateRequest {
    #[serde(rename = "protocolVersion")]
    protocol_version: Option<String>,
    #[serde(rename = "clientInfo")]
    client_info: Option<ClientInfo>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "connectionId")]
    connection_id: Option<String>,
    #[serde(rename = "connectionCapability")]
    connection_capability: Option<String>,
    #[serde(rename = "connectionCapabilityRequired", default)]
    connection_capability_required: bool,
    #[serde(rename = "thinkingLevel")]
    _thinking_level: Option<ThinkingLevel>,
    capabilities: Option<HttpClientCapabilities>,
    #[serde(rename = "optOutNotifications", default)]
    opt_out_notifications: Vec<String>,
    role: Option<ConnectionRole>,
    #[serde(rename = "takeControl", default)]
    take_control: bool,
}

#[derive(Debug, Deserialize)]
struct SubscribeRequest {
    #[serde(rename = "connectionId")]
    connection_id: Option<String>,
    #[serde(rename = "subscriptionId")]
    subscription_id: Option<String>,
    #[serde(rename = "connectionCapability")]
    connection_capability: Option<String>,
    #[serde(rename = "connectionCapabilityRequired", default)]
    connection_capability_required: bool,
    #[serde(rename = "protocolVersion")]
    protocol_version: Option<String>,
    #[serde(rename = "clientInfo")]
    client_info: Option<ClientInfo>,
    capabilities: Option<HttpClientCapabilities>,
    #[serde(rename = "optOutNotifications", default)]
    opt_out_notifications: Vec<String>,
    role: Option<ConnectionRole>,
    #[serde(rename = "takeControl", default)]
    take_control: bool,
}

#[derive(Debug, Deserialize)]
struct HeartbeatRequest {
    #[serde(rename = "connectionId")]
    connection_id: Option<String>,
    #[serde(rename = "subscriptionId")]
    subscription_id: Option<String>,
    #[serde(rename = "connectionCapability")]
    connection_capability: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DisconnectRequest {
    #[serde(rename = "connectionId")]
    connection_id: Option<String>,
    #[serde(rename = "subscriptionId")]
    subscription_id: Option<String>,
    #[serde(rename = "connectionCapability")]
    connection_capability: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DrainRequest {
    reason: Option<String>,
    requested_by: Option<String>,
    export_paths: Option<Vec<String>>,
}

struct UtilityCommandInvocation {
    connection_id: Option<String>,
    command_id: String,
    command: String,
    cwd: Option<String>,
    env: HashMap<String, String>,
    shell_mode: UtilityCommandShellMode,
    terminal_mode: UtilityCommandTerminalMode,
    columns: Option<u32>,
    rows: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct HttpClientCapabilities {
    #[serde(rename = "serverRequests")]
    server_requests: Option<Vec<ServerRequestType>>,
    #[serde(rename = "utilityOperations")]
    utility_operations: Option<Vec<UtilityOperation>>,
    #[serde(rename = "rawAgentEvents")]
    raw_agent_events: Option<bool>,
    #[serde(rename = "transcriptGrade")]
    transcript_grade: Option<crate::transcript::TranscriptGrade>,
    #[serde(rename = "governedCodeMode")]
    governed_code_mode: Option<bool>,
}

impl From<HttpClientCapabilities> for ClientCapabilities {
    fn from(value: HttpClientCapabilities) -> Self {
        Self {
            server_requests: value.server_requests,
            utility_operations: value.utility_operations,
            raw_agent_events: value.raw_agent_events,
            transcript_grade: value.transcript_grade,
            governed_code_mode: value.governed_code_mode,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostedRunnerHeadlessMessageContext {
    pub session_id: String,
    pub connection_id: String,
    pub subscription_id: Option<String>,
    pub role: ConnectionRole,
    pub controller_connection_id: Option<String>,
    pub client_protocol_version: Option<String>,
    pub client_info: Option<ClientInfo>,
    pub capabilities: Option<ClientCapabilities>,
    pub opt_out_notifications: Option<Vec<String>>,
    pub lease_expires_at: String,
    pub workspace_root: PathBuf,
    /// Stable response key used by executors to reconcile a durable pending
    /// response without applying it twice after a hosted-runner restart.
    pub response_idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedRunnerHeadlessMessageExecution {
    TransportOnly,
    RuntimeHandled,
}

#[derive(Debug, Clone)]
pub struct HostedRunnerHeadlessMessageResult {
    pub execution: HostedRunnerHeadlessMessageExecution,
    pub messages: Vec<FromAgentMessage>,
    pub message: String,
    /// Whether a response idempotency key can move from pending to completed.
    pub idempotency_finalized: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HostedRunnerDrainResult {
    pub messages: Vec<FromAgentMessage>,
    pub consumed_response_keys: Vec<String>,
    pub rejected_response_keys: Vec<String>,
}

impl HostedRunnerHeadlessMessageResult {
    pub fn transport_only(messages: Vec<FromAgentMessage>, message: impl Into<String>) -> Self {
        Self {
            execution: HostedRunnerHeadlessMessageExecution::TransportOnly,
            messages,
            message: message.into(),
            idempotency_finalized: true,
        }
    }

    pub fn runtime_handled(messages: Vec<FromAgentMessage>, message: impl Into<String>) -> Self {
        Self {
            execution: HostedRunnerHeadlessMessageExecution::RuntimeHandled,
            messages,
            message: message.into(),
            idempotency_finalized: true,
        }
    }

    fn with_pending_idempotency(mut self) -> Self {
        self.idempotency_finalized = false;
        self
    }
}

pub trait HostedRunnerHeadlessMessageExecutor: Send + Sync {
    fn execute(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError>;

    /// Reconcile a response durably journaled before a prior executor attempt
    /// completed. Remote transports use the stable key in the context to make
    /// this replay idempotent.
    fn reconcile_pending(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        self.execute(context, message)
    }

    fn execute_async<'a>(
        &'a self,
        context: &'a HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<HostedRunnerHeadlessMessageResult, HostedRunnerError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.execute(context, message) })
    }

    fn reconcile_pending_async<'a>(
        &'a self,
        context: &'a HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<HostedRunnerHeadlessMessageResult, HostedRunnerError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.reconcile_pending(context, message) })
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        Ok(HostedRunnerDrainResult::default())
    }

    /// Return the transport wake used to prompt a drain before the next
    /// maintenance tick. The default keeps fixtures and transport-only
    /// executors maintenance-driven.
    fn event_notification(&self) -> Result<Option<Arc<Notify>>, HostedRunnerError> {
        Ok(None)
    }

    /// Pending response acknowledgements and failed ledger writes need the
    /// short maintenance cadence even when transport events are notified.
    fn has_pending_maintenance_work(&self) -> bool {
        false
    }

    /// Report whether a runtime that was previously connected has lost its
    /// transport. Hosted mode owns one child generation, so the event pump
    /// treats this as terminal while leaving `drain` available for export.
    fn disconnected_after_ready(&self) -> Result<bool, HostedRunnerError> {
        Ok(false)
    }

    fn state(&self) -> Result<Option<AgentState>, HostedRunnerError> {
        Ok(None)
    }

    /// Flush the executor-owned session recorder before a terminal receipt is
    /// published. Executors with durable session recording must return an
    /// error when the result cannot be flushed; the hosted boundary then
    /// withholds terminal acceptance and persists a failed runtime boundary.
    fn flush_session(&self) -> Result<Option<PathBuf>, HostedRunnerError> {
        Ok(None)
    }
}

/// Validate the supervisor-owned model/provider binding before the hosted
/// runtime publishes its startup receipt. Invalid bindings must fail startup
/// rather than being silently removed from the public snapshot, because the
/// Ready receipt is the first durable execution boundary for this generation.
pub(crate) fn validate_startup_runtime_receipt_binding(state: &AgentState) -> io::Result<()> {
    match (state.model.as_deref(), state.provider.as_deref()) {
        (None, None) => Ok(()),
        (Some(_), None) | (None, Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid startup runtime receipt binding: model and provider must be supplied together",
        )),
        (Some(model), Some(provider)) => HostedRunnerConfig::validate_live_runtime_receipt_binding(
            model, provider,
        )
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid startup runtime receipt binding: {error}"),
            )
        }),
    }
}

fn validate_message_executor_startup_runtime_receipt_binding(
    message_executor: &dyn HostedRunnerHeadlessMessageExecutor,
) -> io::Result<()> {
    let state = message_executor.state().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to inspect startup runtime receipt binding: {error}"),
        )
    })?;
    state
        .as_ref()
        .map_or(Ok(()), validate_startup_runtime_receipt_binding)
}

#[derive(Clone)]
pub struct AgentSupervisorHostedRunnerMessageExecutor {
    supervisor: Arc<Mutex<AgentSupervisor>>,
    causal_receipt_id: Option<String>,
    response_ledger_transaction: Arc<Mutex<()>>,
    queued_responses: Arc<Mutex<HashMap<String, QueuedResponseOwnership>>>,
    queued_unkeyed_responses: Arc<Mutex<HashMap<String, u64>>>,
    memory_completed_responses: Arc<Mutex<HashMap<String, QueuedResponseOwnership>>>,
    #[cfg(test)]
    ledger_persistence_failures: Arc<Mutex<usize>>,
    #[cfg(test)]
    ledger_admission_barriers: SharedLedgerAdmissionBarriers,
}

#[cfg(test)]
type SharedLedgerAdmissionBarriers =
    Arc<Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>>;

#[derive(Debug, Clone)]
struct QueuedResponseOwnership {
    request_id: String,
    transport_generation: u64,
    workspace_root: PathBuf,
    session_id: String,
}

impl AgentSupervisorHostedRunnerMessageExecutor {
    #[must_use]
    pub fn new(supervisor: Arc<Mutex<AgentSupervisor>>) -> Self {
        Self::new_with_causal_receipt_id(supervisor, None)
    }

    #[must_use]
    pub fn new_with_causal_receipt_id(
        supervisor: Arc<Mutex<AgentSupervisor>>,
        causal_receipt_id: Option<String>,
    ) -> Self {
        Self {
            supervisor,
            causal_receipt_id,
            response_ledger_transaction: Arc::new(Mutex::new(())),
            queued_responses: Arc::new(Mutex::new(HashMap::new())),
            queued_unkeyed_responses: Arc::new(Mutex::new(HashMap::new())),
            memory_completed_responses: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            ledger_persistence_failures: Arc::new(Mutex::new(0)),
            #[cfg(test)]
            ledger_admission_barriers: Arc::new(Mutex::new(None)),
        }
    }

    fn causal_receipt_id(&self) -> &str {
        self.causal_receipt_id.as_deref().unwrap_or("")
    }

    #[cfg(test)]
    fn fail_next_ledger_persistences(&self, count: usize) {
        *self
            .ledger_persistence_failures
            .lock()
            .expect("ledger persistence failure counter") = count;
    }

    #[cfg(test)]
    fn set_ledger_admission_barriers(
        &self,
        loaded: Arc<std::sync::Barrier>,
        resume: Arc<std::sync::Barrier>,
    ) {
        *self
            .ledger_admission_barriers
            .lock()
            .expect("ledger admission barriers") = Some((loaded, resume));
    }

    fn admit_response_key(
        &self,
        workspace_root: &Path,
        session_id: &str,
        key: &str,
        ownership_capacity_available: bool,
    ) -> Result<bool, HostedRunnerError> {
        let _transaction = self
            .response_ledger_transaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ledger = load_executor_response_ledger(workspace_root, session_id)?;
        #[cfg(test)]
        if let Some((loaded, resume)) = self
            .ledger_admission_barriers
            .lock()
            .expect("ledger admission barriers")
            .take()
        {
            loaded.wait();
            resume.wait();
        }
        if ledger
            .iter()
            .any(|(entry, dispatched)| entry == key && *dispatched)
        {
            return Ok(true);
        }
        if !ownership_capacity_available {
            return Err(HostedRunnerError::new(
                HostedRunnerErrorCode::ResponseCapacity,
                "native response queue ownership capacity is full",
            ));
        }
        if !ledger.iter().any(|(entry, _)| entry == key) {
            if ledger.len() >= MAX_RESPONSE_IDEMPOTENCY_RECORDS {
                let Some(index) = ledger.iter().position(|(_, dispatched)| *dispatched) else {
                    return Err(HostedRunnerError::new(
                        HostedRunnerErrorCode::ResponseCapacity,
                        "executor response ledger is full of live pending responses",
                    ));
                };
                ledger.remove(index);
            }
            ledger.push((key.to_string(), false));
            persist_executor_response_ledger(workspace_root, session_id, &ledger)?;
        }
        Ok(false)
    }

    fn remove_pending_response_key(
        &self,
        workspace_root: &Path,
        session_id: &str,
        key: &str,
    ) -> Result<(), HostedRunnerError> {
        let _transaction = self
            .response_ledger_transaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(test)]
        {
            let mut failures = self
                .ledger_persistence_failures
                .lock()
                .expect("ledger persistence failure counter");
            if *failures > 0 {
                *failures -= 1;
                return Err(HostedRunnerError::internal(
                    "injected executor ledger persistence failure",
                ));
            }
        }
        let mut ledger = load_executor_response_ledger(workspace_root, session_id)?;
        ledger.retain(|(entry, dispatched)| entry != key || *dispatched);
        persist_executor_response_ledger(workspace_root, session_id, &ledger)
    }

    /// Drop in-memory ownership first, then best-effort the ledger write.
    /// A transient persist failure must not replace a protocol rejection or
    /// transport error with `Internal`, and must not keep the key counted
    /// against queued-response capacity. Admission only dedups dispatched
    /// entries, so a leftover pending row does not block a same-key retry.
    /// Distinct stale pending rows still occupy ledger slots until a
    /// dispatched row can be evicted or the key is retried.
    fn release_undispatched_response_ownership(
        &self,
        workspace_root: &Path,
        session_id: &str,
        key: &str,
        event: &'static str,
    ) {
        self.queued_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        self.memory_completed_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        if let Err(error) = self.remove_pending_response_key(workspace_root, session_id, key) {
            tracing::warn!(
                target: "maestro.hosted",
                event,
                causal_receipt_id = self.causal_receipt_id(),
                error = %error,
                "response ledger cleanup failed; in-memory ownership released and the stale pending entry clears on retry",
            );
        }
    }

    fn persist_consumed_response(
        &self,
        key: &str,
        ownership: &QueuedResponseOwnership,
    ) -> Result<(), HostedRunnerError> {
        let _transaction = self
            .response_ledger_transaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(test)]
        {
            let mut failures = self
                .ledger_persistence_failures
                .lock()
                .expect("ledger persistence failure counter");
            if *failures > 0 {
                *failures -= 1;
                return Err(HostedRunnerError::internal(
                    "injected executor ledger persistence failure",
                ));
            }
        }
        let mut ledger =
            load_executor_response_ledger(&ownership.workspace_root, &ownership.session_id)?;
        if let Some((_, dispatched)) = ledger.iter_mut().find(|(entry, _)| entry == key) {
            *dispatched = true;
        } else {
            if ledger.len() >= MAX_RESPONSE_IDEMPOTENCY_RECORDS {
                let Some(index) = ledger.iter().position(|(_, dispatched)| *dispatched) else {
                    return Err(HostedRunnerError::new(
                        HostedRunnerErrorCode::ResponseCapacity,
                        "executor response ledger is full of live pending responses",
                    ));
                };
                ledger.remove(index);
            }
            ledger.push((key.to_string(), true));
        }
        persist_executor_response_ledger(&ownership.workspace_root, &ownership.session_id, &ledger)
    }

    async fn await_queued_response_outcome(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        key: &str,
        request_id: &str,
        mut result: HostedRunnerHeadlessMessageResult,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        let (messages, acknowledgement) = AgentSupervisor::wait_for_response_acknowledgement_async(
            Arc::clone(&self.supervisor),
            request_id.to_string(),
            RESPONSE_ACK_TIMEOUT,
        )
        .await;
        let generation = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?
            .transport_generation();
        result.messages.extend(messages);
        match acknowledgement {
            ResponseAcknowledgement::Consumed => {
                self.queued_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(key);
                let ownership = QueuedResponseOwnership {
                    request_id: request_id.to_string(),
                    transport_generation: generation,
                    workspace_root: context.workspace_root.clone(),
                    session_id: context.session_id.clone(),
                };
                if self.persist_consumed_response(key, &ownership).is_err() {
                    self.memory_completed_responses
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key.to_string(), ownership);
                }
                result.idempotency_finalized = true;
                Ok(result)
            }
            ResponseAcknowledgement::Rejected => {
                self.release_undispatched_response_ownership(
                    &context.workspace_root,
                    &context.session_id,
                    key,
                    "queued_response_rejection_ledger_stale",
                );
                let rejection = result.messages.iter().find_map(|message| match message {
                    FromAgentMessage::Error {
                        request_id: Some(rejected_id),
                        message,
                        error_type: Some(crate::headless::messages::HeadlessErrorType::Protocol),
                        ..
                    } if rejected_id == request_id => Some(message.as_str()),
                    _ => None,
                });
                Err(HostedRunnerError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    format!(
                        "native response rejected: {}",
                        rejection.unwrap_or("headless protocol rejected the response")
                    ),
                ))
            }
            ResponseAcknowledgement::NotExpected | ResponseAcknowledgement::Queued => Ok(result),
        }
    }
}

impl std::fmt::Debug for AgentSupervisorHostedRunnerMessageExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSupervisorHostedRunnerMessageExecutor")
            .finish_non_exhaustive()
    }
}

impl HostedRunnerHeadlessMessageExecutor for AgentSupervisorHostedRunnerMessageExecutor {
    fn execute(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        self.execute_with_mode(context, message, false)
    }

    fn reconcile_pending(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        self.execute_with_mode(context, message, true)
    }

    fn execute_async<'a>(
        &'a self,
        context: &'a HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<HostedRunnerHeadlessMessageResult, HostedRunnerError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let key = context.response_idempotency_key.clone();
            let request_id = response_ack_request_id(&message).map(str::to_owned);
            let result = self.execute_with_mode(context, message, false)?;
            match (result.idempotency_finalized, key, request_id) {
                (false, Some(key), Some(request_id)) => {
                    self.await_queued_response_outcome(context, &key, &request_id, result)
                        .await
                }
                _ => Ok(result),
            }
        })
    }

    fn reconcile_pending_async<'a>(
        &'a self,
        context: &'a HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<HostedRunnerHeadlessMessageResult, HostedRunnerError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let key = context.response_idempotency_key.clone();
            let request_id = response_ack_request_id(&message).map(str::to_owned);
            let result = self.execute_with_mode(context, message, true)?;
            match (result.idempotency_finalized, key, request_id) {
                (false, Some(key), Some(request_id)) => {
                    self.await_queued_response_outcome(context, &key, &request_id, result)
                        .await
                }
                _ => Ok(result),
            }
        })
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        let mut supervisor = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
        let messages = supervisor.drain_available_agent_messages();
        let generation = supervisor.transport_generation();
        let mut queued = self
            .queued_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale = queued
            .iter()
            .filter(|(_, ownership)| ownership.transport_generation != generation)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale {
            queued.remove(&key);
        }
        let mut queued_unkeyed = self
            .queued_unkeyed_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale_unkeyed = queued_unkeyed
            .iter()
            .filter(|(_, transport_generation)| **transport_generation != generation)
            .map(|(request_id, transport_generation)| (request_id.clone(), *transport_generation))
            .collect::<Vec<_>>();
        for (request_id, transport_generation) in stale_unkeyed {
            supervisor.discard_response_acknowledgement(&request_id, transport_generation);
            queued_unkeyed.remove(&request_id);
        }
        let completed_unkeyed = queued_unkeyed
            .keys()
            .filter(|request_id| {
                supervisor.has_response_acknowledgement(request_id)
                    || supervisor.has_response_rejection(request_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for request_id in completed_unkeyed {
            let _ = supervisor.take_response_acknowledgement(&request_id);
            let _ = supervisor.take_response_rejection(&request_id);
            queued_unkeyed.remove(&request_id);
        }
        {
            let pending_persistence = self
                .memory_completed_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            for (key, ownership) in pending_persistence {
                if self.persist_consumed_response(&key, &ownership).is_ok() {
                    self.memory_completed_responses
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&key);
                }
            }
        }
        let acknowledged = queued
            .iter()
            .filter(|(_, ownership)| supervisor.has_response_acknowledgement(&ownership.request_id))
            .map(|(key, ownership)| (key.clone(), ownership.clone()))
            .collect::<Vec<_>>();
        let mut consumed_response_keys = Vec::with_capacity(acknowledged.len());
        for (key, ownership) in acknowledged {
            if supervisor.take_response_acknowledgement(&ownership.request_id) {
                queued.remove(&key);
                if self.persist_consumed_response(&key, &ownership).is_err() {
                    self.memory_completed_responses
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(key.clone(), ownership);
                }
                consumed_response_keys.push(key);
            }
        }
        let rejected = queued
            .iter()
            .filter(|(_, ownership)| supervisor.has_response_rejection(&ownership.request_id))
            .map(|(key, ownership)| (key.clone(), ownership.clone()))
            .collect::<Vec<_>>();
        let mut rejected_response_keys = Vec::with_capacity(rejected.len());
        for (key, ownership) in rejected {
            if supervisor
                .take_response_rejection(&ownership.request_id)
                .is_some()
            {
                queued.remove(&key);
                self.memory_completed_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);
                // A transient ledger cleanup failure must not fail the drain:
                // the rejection is already taken from the supervisor, and a
                // stale pending ledger entry never dedups admission and
                // clears when the key is retried.
                if let Err(error) = self.remove_pending_response_key(
                    &ownership.workspace_root,
                    &ownership.session_id,
                    &key,
                ) {
                    tracing::warn!(
                        event = "drained_rejection_ledger_stale",
                        causal_receipt_id = self.causal_receipt_id(),
                        error = %error,
                        "rejected response ledger cleanup failed during drain; the stale pending entry clears on retry",
                    );
                }
                rejected_response_keys.push(key);
            }
        }
        Ok(HostedRunnerDrainResult {
            messages,
            consumed_response_keys,
            rejected_response_keys,
        })
    }

    fn event_notification(&self) -> Result<Option<Arc<Notify>>, HostedRunnerError> {
        let supervisor = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
        Ok(supervisor.event_notification())
    }

    fn has_pending_maintenance_work(&self) -> bool {
        !self
            .queued_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            || !self
                .queued_unkeyed_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            || !self
                .memory_completed_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }

    fn disconnected_after_ready(&self) -> Result<bool, HostedRunnerError> {
        let supervisor = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
        Ok(supervisor.transport_generation() != 0 && !supervisor.is_connected())
    }

    fn state(&self) -> Result<Option<AgentState>, HostedRunnerError> {
        let supervisor = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
        Ok(Some(supervisor.state().clone()))
    }

    fn flush_session(&self) -> Result<Option<PathBuf>, HostedRunnerError> {
        let mut supervisor = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
        supervisor
            .flush_session()
            .map_err(|error| HostedRunnerError::internal(error.to_string()))?;
        Ok(supervisor.session_file().map(Path::to_path_buf))
    }
}

impl AgentSupervisorHostedRunnerMessageExecutor {
    fn execute_with_mode(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
        reconciled: bool,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        let started = Instant::now();
        let message_kind = hosted_message_kind(&message);
        if let ToAgentMessage::Hello {
            controller_binding: Some(binding),
            ..
        } = &message
        {
            let mut supervisor = self
                .supervisor
                .lock()
                .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
            supervisor
                .set_local_controller_binding(binding.clone())
                .map_err(hosted_runner_error_from_async_transport)?;
            return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                Vec::new(),
                "Rust hosted runner forwarded the resident controller binding to the native child",
            ));
        }
        if matches!(message, ToAgentMessage::Hello { .. }) {
            tracing::debug!(
                target: "maestro.hosted",
                event = "hosted_message_transport_handled",
                causal_receipt_id = self.causal_receipt_id(),
                session_id = %context.session_id,
                connection_id = %context.connection_id,
                message_kind,
                duration_ms = started.elapsed().as_millis() as u64,
            );
            return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                vec![self.hosted_hello_ok_for_context(context)],
                "Rust hosted runner negotiated the connection at the hosted boundary",
            ));
        }
        let response_key = is_control_response_message(&message)
            .then_some(context.response_idempotency_key.as_deref())
            .flatten();
        let response_request_id = response_ack_request_id(&message).map(str::to_owned);
        if let Some(key) = response_key {
            if self
                .memory_completed_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(key)
            {
                return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                    Vec::new(),
                    "Rust hosted runner reconciled a memory-completed response",
                ));
            }
            let queued = self
                .queued_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let memory_completed = self
                .memory_completed_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ownership_capacity_available = queued.contains_key(key)
                || memory_completed.contains_key(key)
                || queued.len() + memory_completed.len() < MAX_RESPONSE_IDEMPOTENCY_RECORDS;
            drop(memory_completed);
            drop(queued);
            if self.admit_response_key(
                &context.workspace_root,
                &context.session_id,
                key,
                ownership_capacity_available,
            )? {
                return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                    Vec::new(),
                    "Rust hosted runner reconciled an already accepted response",
                ));
            }
        }
        let mut supervisor = self
            .supervisor
            .lock()
            .map_err(|_| HostedRunnerError::internal("agent supervisor mutex poisoned"))?;
        let reconciled_acknowledgement = if reconciled {
            response_key.and_then(|key| {
                let ownership = self
                    .queued_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(key)
                    .cloned();
                ownership.and_then(|ownership| {
                    if ownership.transport_generation == supervisor.transport_generation() {
                        Some(supervisor.wait_for_response_acknowledgement(&ownership.request_id))
                    } else {
                        self.queued_responses
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(key);
                        None
                    }
                })
            })
        } else {
            None
        };
        let configured_model = supervisor
            .state()
            .model
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let configured_provider = supervisor
            .state()
            .provider
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        tracing::info!(
            target: "maestro.hosted",
            event = "hosted_model_binding_dispatch",
            causal_receipt_id = self.causal_receipt_id(),
            session_id = %context.session_id,
            connection_id = %context.connection_id,
            message_kind,
            configured_model = %configured_model,
            configured_provider = %configured_provider,
            response_idempotency_present = response_key.is_some(),
            reconcile_pending = reconciled,
        );
        let (messages, acknowledgement) = match reconciled_acknowledgement {
            Some(result) => result,
            None => match supervisor.send_and_drain_agent_messages_with_ack(message) {
                Ok(result) => result,
                Err(error) => {
                    if let Some(key) = response_key {
                        self.release_undispatched_response_ownership(
                            &context.workspace_root,
                            &context.session_id,
                            key,
                            "response_dispatch_ledger_stale",
                        );
                    }
                    tracing::warn!(
                        target: "maestro.hosted",
                        event = "hosted_model_binding_dispatch_failed",
                        causal_receipt_id = self.causal_receipt_id(),
                        session_id = %context.session_id,
                        connection_id = %context.connection_id,
                        message_kind,
                        configured_model = %configured_model,
                        configured_provider = %configured_provider,
                        duration_ms = started.elapsed().as_millis() as u64,
                        outcome = "transport_error",
                    );
                    return Err(hosted_runner_error_from_async_transport(error));
                }
            },
        };
        if response_key.is_some() && matches!(acknowledgement, ResponseAcknowledgement::NotExpected)
        {
            if let Some(key) = response_key {
                self.release_undispatched_response_ownership(
                    &context.workspace_root,
                    &context.session_id,
                    key,
                    "response_not_expected_ledger_stale",
                );
            }
            tracing::warn!(
                target: "maestro.hosted",
                event = "hosted_model_binding_dispatch_failed",
                causal_receipt_id = self.causal_receipt_id(),
                session_id = %context.session_id,
                connection_id = %context.connection_id,
                message_kind,
                configured_model = %configured_model,
                configured_provider = %configured_provider,
                duration_ms = started.elapsed().as_millis() as u64,
                outcome = "response_not_acknowledged",
                reconcile_pending = reconciled,
            );
            return Err(HostedRunnerError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                "response consumer did not acknowledge the control response",
            ));
        }
        if response_key.is_some() && matches!(acknowledgement, ResponseAcknowledgement::Rejected) {
            if let Some(key) = response_key {
                self.release_undispatched_response_ownership(
                    &context.workspace_root,
                    &context.session_id,
                    key,
                    "queued_response_rejection_ledger_stale",
                );
            }
            let rejection = messages.iter().find_map(|message| match message {
                FromAgentMessage::Error {
                    request_id: Some(request_id),
                    message,
                    error_type: Some(crate::headless::messages::HeadlessErrorType::Protocol),
                    ..
                } if response_request_id.as_deref() == Some(request_id.as_str()) => {
                    Some(message.as_str())
                }
                _ => None,
            });
            return Err(HostedRunnerError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                format!(
                    "native response rejected: {}",
                    rejection.unwrap_or("headless protocol rejected the response")
                ),
            ));
        }
        if response_key.is_some() && matches!(acknowledgement, ResponseAcknowledgement::Queued) {
            if let (Some(key), Some(request_id)) = (response_key, response_request_id.as_ref()) {
                self.queued_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        key.to_string(),
                        QueuedResponseOwnership {
                            request_id: request_id.clone(),
                            transport_generation: supervisor.transport_generation(),
                            workspace_root: context.workspace_root.clone(),
                            session_id: context.session_id.clone(),
                        },
                    );
            }
            tracing::info!(
                target: "maestro.hosted",
                event = "hosted_response_queued",
                causal_receipt_id = self.causal_receipt_id(),
                session_id = %context.session_id,
                connection_id = %context.connection_id,
                message_kind,
                "response remains owned by the native queue; outer idempotency prevents a duplicate dispatch",
            );
        }
        if response_key.is_none() && matches!(acknowledgement, ResponseAcknowledgement::Queued) {
            if let Some(request_id) = response_request_id.as_ref() {
                self.queued_unkeyed_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(request_id.clone(), supervisor.transport_generation());
            }
        }
        if let Some(key) =
            response_key.filter(|_| matches!(acknowledgement, ResponseAcknowledgement::Consumed))
        {
            self.queued_responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(key);
            let ownership = QueuedResponseOwnership {
                request_id: response_request_id.clone().unwrap_or_default(),
                transport_generation: supervisor.transport_generation(),
                workspace_root: context.workspace_root.clone(),
                session_id: context.session_id.clone(),
            };
            if self.persist_consumed_response(key, &ownership).is_err() {
                self.memory_completed_responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key.to_string(), ownership);
                tracing::warn!(
                    target: "maestro.hosted",
                    event = "hosted_response_idempotency_memory_only",
                    causal_receipt_id = self.causal_receipt_id(),
                    session_id = %context.session_id,
                    connection_id = %context.connection_id,
                    message_kind,
                    "native consumed the response; retaining live-process idempotency after ledger persistence failed",
                );
            }
        }
        tracing::info!(
            target: "maestro.hosted",
            event = "hosted_model_binding_dispatch_completed",
            causal_receipt_id = self.causal_receipt_id(),
            session_id = %context.session_id,
            connection_id = %context.connection_id,
            message_kind,
            configured_model = %configured_model,
            configured_provider = %configured_provider,
            response_message_count = messages.len(),
            acknowledged = matches!(acknowledgement, ResponseAcknowledgement::Consumed),
            queued = matches!(acknowledgement, ResponseAcknowledgement::Queued),
            duration_ms = started.elapsed().as_millis() as u64,
            outcome = "success",
            reconcile_pending = reconciled,
        );
        let result = HostedRunnerHeadlessMessageResult::runtime_handled(
            messages,
            "Rust hosted runner forwarded the headless message to AgentSupervisor",
        );
        if matches!(acknowledgement, ResponseAcknowledgement::Queued) {
            Ok(result.with_pending_idempotency())
        } else {
            Ok(result)
        }
    }
}

fn hosted_message_kind(message: &ToAgentMessage) -> &'static str {
    match message {
        ToAgentMessage::Hello { .. } => "hello",
        ToAgentMessage::Init { .. } => "init",
        ToAgentMessage::GovernedInit { .. } => "governed_init",
        ToAgentMessage::RestoreConversation { .. } => "restore_conversation",
        ToAgentMessage::Prompt { .. } => "prompt",
        ToAgentMessage::GovernedPrompt { .. } => "governed_prompt",
        ToAgentMessage::Steer { .. } => "steer",
        ToAgentMessage::GovernedSteer { .. } => "governed_steer",
        ToAgentMessage::Interrupt => "interrupt",
        ToAgentMessage::ToolResponse { .. } => "tool_response",
        ToAgentMessage::ClientToolResult { .. } => "client_tool_result",
        ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
            ..
        }) => "governed_client_tool_result",
        ToAgentMessage::ManagedAuthorizationResult { .. } => "managed_authorization_result",
        ToAgentMessage::ApplyWorkspaceCapabilitySet { .. } => "apply_workspace_capability_set",
        ToAgentMessage::ConfigurePromptExperiment { .. } => "configure_prompt_experiment",
        ToAgentMessage::ServerRequestResponse { .. } => "server_request_response",
        ToAgentMessage::UtilityCommandStart { .. } => "utility_command_start",
        ToAgentMessage::UtilityCommandTerminate { .. } => "utility_command_terminate",
        ToAgentMessage::UtilityCommandStdin { .. } => "utility_command_stdin",
        ToAgentMessage::UtilityCommandResize { .. } => "utility_command_resize",
        ToAgentMessage::UtilityFileSearch { .. } => "utility_file_search",
        ToAgentMessage::UtilityFileRead { .. } => "utility_file_read",
        ToAgentMessage::UtilityFileWatchStart { .. } => "utility_file_watch_start",
        ToAgentMessage::UtilityFileWatchStop { .. } => "utility_file_watch_stop",
        ToAgentMessage::Cancel => "cancel",
        ToAgentMessage::Shutdown => "shutdown",
    }
}

fn executor_response_ledger_path(workspace_root: &Path, session_id: &str) -> PathBuf {
    let session_digest = Sha256::digest(session_id.as_bytes());
    workspace_root
        .join(".maestro")
        .join("hosted-runner")
        .join(format!(
            "executor-response-idempotency-{session_digest:x}.json"
        ))
}

fn load_executor_response_ledger(
    workspace_root: &Path,
    session_id: &str,
) -> Result<Vec<(String, bool)>, HostedRunnerError> {
    let path = executor_response_ledger_path(workspace_root, session_id);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(HostedRunnerError::internal(format!(
                "read response ledger: {error}"
            )));
        }
    };
    serde_json::from_str(&contents)
        .map_err(|error| HostedRunnerError::internal(format!("invalid response ledger: {error}")))
}

fn persist_executor_response_ledger(
    workspace_root: &Path,
    session_id: &str,
    ledger: &[(String, bool)],
) -> Result<(), HostedRunnerError> {
    let path = executor_response_ledger_path(workspace_root, session_id);
    let parent = path
        .parent()
        .ok_or_else(|| HostedRunnerError::internal("response ledger has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| HostedRunnerError::internal(format!("create response ledger: {error}")))?;
    let contents = serde_json::to_vec(ledger).map_err(|error| {
        HostedRunnerError::internal(format!("serialize response ledger: {error}"))
    })?;
    crate::fs_atomic::write_atomic(&path, contents)
        .map_err(|error| HostedRunnerError::internal(format!("persist response ledger: {error}")))
}

#[derive(Debug, Default)]
struct TransportOnlyHostedRunnerMessageExecutor;

impl HostedRunnerHeadlessMessageExecutor for TransportOnlyHostedRunnerMessageExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            vec![FromAgentMessage::Status {
                message:
                    "Rust hosted runner accepted the headless message; agent execution is not attached yet"
                        .to_string(),
            }],
            "Rust hosted runner accepted the headless message; agent execution is not attached yet",
        ))
    }
}

impl AgentSupervisorHostedRunnerMessageExecutor {
    fn negotiated_server_capabilities(&self) -> Option<ServerCapabilities> {
        let Ok(supervisor) = self.supervisor.lock() else {
            return Some(native_server_capabilities_without_governed_grants());
        };
        supervisor.state().server_capabilities.clone().or_else(|| {
            (supervisor.transport_generation() == 0)
                .then(native_server_capabilities_without_governed_grants)
        })
    }

    fn hosted_hello_ok_for_context(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
    ) -> FromAgentMessage {
        FromAgentMessage::HelloOk {
            protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
            controller_binding_version: None,
            controller_binding_sha256: None,
            connection_id: Some(context.connection_id.clone()),
            client_protocol_version: context.client_protocol_version.clone(),
            client_info: context.client_info.clone(),
            capabilities: context.capabilities.clone(),
            server_capabilities: self.negotiated_server_capabilities(),
            opt_out_notifications: context.opt_out_notifications.clone(),
            role: Some(context.role),
            controller_connection_id: context.controller_connection_id.clone(),
            lease_expires_at: Some(context.lease_expires_at.clone()),
        }
    }
}

fn server_capabilities_for_executor(
    message_executor: &dyn HostedRunnerHeadlessMessageExecutor,
) -> Option<ServerCapabilities> {
    match message_executor.state() {
        Ok(Some(agent_state)) => agent_state.server_capabilities,
        Ok(None) | Err(_) => Some(native_server_capabilities_without_governed_grants()),
    }
}

fn native_server_capabilities_without_governed_grants() -> ServerCapabilities {
    server_capabilities_without_governed_grants(native_server_capabilities())
}

fn server_capabilities_without_governed_grants(
    mut capabilities: ServerCapabilities,
) -> ServerCapabilities {
    capabilities.governed_tool_grant_algorithms.clear();
    capabilities
}

fn governed_grant_algorithms_for_executor(
    message_executor: &dyn HostedRunnerHeadlessMessageExecutor,
) -> Vec<String> {
    server_capabilities_for_executor(message_executor)
        .map(|capabilities| capabilities.governed_tool_grant_algorithms)
        .unwrap_or_default()
}

fn hosted_runner_error_from_async_transport(error: AsyncTransportError) -> HostedRunnerError {
    HostedRunnerError::runtime_not_ready(format!("agent supervisor is not ready: {error}"))
}

fn json_string_value<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HostedError {
    status: u16,
    code: HostedRunnerErrorCode,
    message: String,
    details: Option<serde_json::Value>,
}

type HostedResult<T> = Result<T, HostedError>;

pub async fn start_hosted_runner(config: HostedRunnerConfig) -> io::Result<HostedRunnerHandle> {
    start_hosted_runner_with_message_executor(
        config,
        Arc::new(TransportOnlyHostedRunnerMessageExecutor),
    )
    .await
}

pub(crate) async fn load_hosted_runner_session_replay(
    config: &HostedRunnerConfig,
) -> io::Result<Option<SessionReplay>> {
    Ok(load_restore_manifest(config)
        .await?
        .map(|manifest| manifest.session_replay()))
}

fn startup_workspace_id(config: &HostedRunnerConfig) -> String {
    config
        .workload_identity
        .as_ref()
        .map(|identity| identity.workspace_id.clone())
        .or_else(|| config.workspace_id.clone())
        .unwrap_or_default()
}

async fn join_initial_identity_exchanges<SF, CF, S, C, E>(
    server: SF,
    client: CF,
) -> Result<(S, C), E>
where
    SF: Future<Output = Result<S, E>>,
    CF: Future<Output = Result<C, E>>,
{
    tokio::try_join!(server, client)
}

type HostedRunnerInitialIdentityRuntime = (
    Arc<workload_identity::WorkloadIdentityExchanger>,
    workload_identity::ReloadableServerIdentity,
    Option<workload_identity::ReloadableClientIdentity>,
    config::HostedRunnerWorkloadIdentityConfig,
);

pub(crate) struct PreparedHostedRunner {
    startup_started: Instant,
    config: HostedRunnerConfig,
    restore_manifest: Option<SnapshotManifest>,
    identity_runtime: Option<HostedRunnerInitialIdentityRuntime>,
    listener: TcpListener,
    local_addr: SocketAddr,
}

pub async fn start_hosted_runner_with_message_executor(
    config: HostedRunnerConfig,
    message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
) -> io::Result<HostedRunnerHandle> {
    validate_message_executor_startup_runtime_receipt_binding(message_executor.as_ref())?;
    let prepared = prepare_hosted_runner(config).await?;
    start_prepared_hosted_runner(prepared, message_executor)
}

#[cfg(test)]
async fn start_hosted_runner_with_message_executor_and_maintenance_interval(
    config: HostedRunnerConfig,
    message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
    maintenance_interval: Duration,
) -> io::Result<HostedRunnerHandle> {
    validate_message_executor_startup_runtime_receipt_binding(message_executor.as_ref())?;
    let prepared = prepare_hosted_runner(config).await?;
    start_prepared_hosted_runner_with_maintenance_interval(
        prepared,
        message_executor,
        maintenance_interval,
    )
}

pub(crate) async fn prepare_hosted_runner(
    config: HostedRunnerConfig,
) -> io::Result<PreparedHostedRunner> {
    let startup_started = Instant::now();
    let runner_session_id = config.runner_session_id.clone();
    let workspace_root = tokio::fs::canonicalize(&config.workspace_root).await?;
    if !tokio::fs::metadata(&workspace_root).await?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace root must be a directory",
        ));
    }

    let mut config = config;
    config.auth_token = normalize_auth_token(config.auth_token.as_deref()).map(str::to_string);
    if !config.bind_addr.ip().is_loopback()
        && config.auth_token.is_none()
        && config.workload_identity.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "deixic-code hosted-runner requires auth_token or workload identity when binding to non-loopback interfaces",
        ));
    }
    config.workspace_root = workspace_root;
    let restore_manifest = load_restore_manifest(&config).await?;
    config
        .validate_runtime_receipt_identity(restore_manifest.as_ref())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let identity_runtime = if let Some(identity_config) = config.workload_identity.clone() {
        let exchange_started = Instant::now();
        let exchanger = match workload_identity::WorkloadIdentityExchanger::try_new(
            identity_config.clone(),
            config.runner_session_id.clone(),
        ) {
            Ok(exchanger) => Arc::new(exchanger),
            Err(error) => {
                tracing::warn!(
                    target: "maestro.hosted",
                    event = "hosted_runner_startup_stage",
                    stage = "workload_identity_exchange",
                    outcome = "error",
                    error_kind = error.as_str(),
                    runner_session_id = %runner_session_id,
                    organization_id = %identity_config.organization_id,
                    workspace_id = %identity_config.workspace_id,
                    sandbox_id = %identity_config.sandbox_id,
                    placement_generation = identity_config.placement_generation,
                    duration_ms = exchange_started.elapsed().as_millis() as u64,
                    "Hosted runner startup stage failed before identity exchange"
                );
                return Err(io::Error::other(error));
            }
        };
        let identities = if let Some(rendezvous) = config.rendezvous.as_ref() {
            join_initial_identity_exchanges(
                exchanger.exchange_initial(),
                exchanger.exchange_client_initial(&rendezvous.identity_exchange_url),
            )
            .await
            .map(|(server, client)| (server, Some(client)))
        } else {
            exchanger
                .exchange_initial()
                .await
                .map(|server| (server, None))
        };
        let (identity, rendezvous_identity) = match identities {
            Ok(identities) => {
                tracing::info!(
                    target: "maestro.hosted",
                    event = "hosted_runner_startup_stage",
                    stage = "workload_identity_exchange",
                    outcome = "success",
                    runner_session_id = %runner_session_id,
                    organization_id = %identity_config.organization_id,
                    workspace_id = %identity_config.workspace_id,
                    sandbox_id = %identity_config.sandbox_id,
                    placement_generation = identity_config.placement_generation,
                    duration_ms = exchange_started.elapsed().as_millis() as u64,
                    "Hosted runner startup stage completed"
                );
                identities
            }
            Err(error) => {
                tracing::warn!(
                    target: "maestro.hosted",
                    event = "hosted_runner_startup_stage",
                    stage = "workload_identity_exchange",
                    outcome = "error",
                    error_kind = error.as_str(),
                    runner_session_id = %runner_session_id,
                    organization_id = %identity_config.organization_id,
                    workspace_id = %identity_config.workspace_id,
                    sandbox_id = %identity_config.sandbox_id,
                    placement_generation = identity_config.placement_generation,
                    duration_ms = exchange_started.elapsed().as_millis() as u64,
                    "Hosted runner startup stage failed"
                );
                return Err(io::Error::other(error));
            }
        };
        let rendezvous_identity =
            rendezvous_identity.map(workload_identity::ReloadableClientIdentity::new);
        Some((
            exchanger,
            workload_identity::ReloadableServerIdentity::new(identity),
            rendezvous_identity,
            identity_config,
        ))
    } else {
        None
    };
    let listener = TcpListener::bind(config.bind_addr).await?;
    let local_addr = listener.local_addr()?;

    Ok(PreparedHostedRunner {
        startup_started,
        config,
        restore_manifest,
        identity_runtime,
        listener,
        local_addr,
    })
}

pub(crate) fn start_prepared_hosted_runner(
    prepared: PreparedHostedRunner,
    message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
) -> io::Result<HostedRunnerHandle> {
    start_prepared_hosted_runner_with_maintenance_interval(
        prepared,
        message_executor,
        MAINTENANCE_PUMP_INTERVAL,
    )
}

fn start_prepared_hosted_runner_with_maintenance_interval(
    prepared: PreparedHostedRunner,
    message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
    maintenance_interval: Duration,
) -> io::Result<HostedRunnerHandle> {
    let PreparedHostedRunner {
        startup_started,
        config,
        restore_manifest,
        identity_runtime,
        listener,
        local_addr,
    } = prepared;
    let runner_session_id = config.runner_session_id.clone();
    let workspace_id = startup_workspace_id(&config);
    let identity_context = config.workload_identity.as_ref().map(|identity| {
        (
            identity.organization_id.clone(),
            identity.sandbox_id,
            identity.placement_generation,
        )
    });
    let shutdown = CancellationToken::new();
    let shared = SharedRunner::try_new_with_message_executor_and_restore(
        config,
        message_executor,
        restore_manifest,
    )?;
    shared.start_event_pump(maintenance_interval);
    let server_shared = shared.clone();
    let (task, identity_task, tls) =
        if let Some((exchanger, identity, client_identity, workload)) = identity_runtime {
            let server_shutdown = shutdown.clone();
            let server_identity = identity.clone();
            let task = tokio::spawn(async move {
                serve_mtls(listener, server_shared, server_identity, server_shutdown).await;
            });
            let rotation_shutdown = shutdown.clone();
            let rendezvous_config = shared.config.rendezvous.clone();
            let rendezvous_shared = shared.clone();
            let identity_task = tokio::spawn(async move {
                let server_rotation = workload_identity::rotate_server_identity(
                    exchanger.clone(),
                    identity,
                    rotation_shutdown.clone(),
                );
                if let (Some(client_identity), Some(rendezvous_config)) =
                    (client_identity, rendezvous_config)
                {
                    let client_rotation = workload_identity::rotate_client_identity(
                        exchanger,
                        rendezvous_config.identity_exchange_url.clone(),
                        client_identity.clone(),
                        rotation_shutdown.clone(),
                    );
                    let rendezvous = rendezvous_runtime::run(
                        rendezvous_config,
                        workload,
                        client_identity,
                        rendezvous_shared,
                        rotation_shutdown,
                    );
                    tokio::join!(server_rotation, client_rotation, rendezvous);
                } else {
                    server_rotation.await;
                }
            });
            (task, Some(identity_task), true)
        } else {
            let server_shutdown = shutdown.clone();
            let task = tokio::spawn(async move {
                serve(listener, server_shared, server_shutdown).await;
            });
            (task, None, false)
        };

    let (organization_id, sandbox_id, placement_generation) = identity_context
        .map(|(organization_id, sandbox_id, generation)| {
            (organization_id, sandbox_id.to_string(), generation)
        })
        .unwrap_or_default();
    tracing::info!(
        target: "maestro.hosted",
        event = "hosted_runner_startup",
        stage = "ready",
        outcome = "success",
        runner_session_id = %runner_session_id,
        organization_id = %organization_id,
        workspace_id = %workspace_id,
        sandbox_id = %sandbox_id,
        placement_generation,
        tls,
        listen_port = local_addr.port(),
        duration_ms = startup_started.elapsed().as_millis() as u64,
        "Hosted runner is ready"
    );

    Ok(HostedRunnerHandle {
        local_addr,
        shared,
        shutdown,
        task,
        identity_task,
        tls,
    })
}

async fn pump_agent_events(
    shared: SharedRunner,
    cancelled: CancellationToken,
    maintenance_interval: Duration,
) {
    let mut first_tick = true;
    loop {
        let notification = match shared.message_executor.event_notification() {
            Ok(notification) => notification,
            Err(error) => {
                shared.publish_runtime_error("event_pump_failed", error);
                break;
            }
        };
        let can_use_safety_tick = notification.is_some()
            && !shared
                .thread_persistence_retry_pending
                .load(Ordering::Acquire)
            && !shared.message_executor.has_pending_maintenance_work();
        let tick_interval = if first_tick {
            Duration::ZERO
        } else if can_use_safety_tick {
            NOTIFIED_MAINTENANCE_PUMP_INTERVAL.max(maintenance_interval)
        } else {
            maintenance_interval
        };
        first_tick = false;
        tokio::select! {
            () = cancelled.cancelled() => break,
            () = tokio::time::sleep(tick_interval) => {},
            () = async {
                if let Some(notification) = notification {
                    notification.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {},
        }
        let lifecycle = shared.mutation_lifecycle.clone();
        let _lifecycle = tokio::select! {
            () = cancelled.cancelled() => break,
            lifecycle = lifecycle.lock() => lifecycle,
        };
        if matches!(pump_tick(&shared), PumpTick::Stop) {
            break;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PumpTick {
    Continue,
    Stop,
}

/// One event-pump iteration: drain the executor, publish the drained
/// messages, finalize consumed and roll back rejected response keys, and
/// retry any deferred thread journal write. Extracted from
/// [`pump_agent_events`] so tests can drive ticks deterministically instead
/// of racing `EVENT_PUMP_INTERVAL`. The caller holds the mutation lifecycle
/// lock.
fn pump_tick(shared: &SharedRunner) -> PumpTick {
    match shared.message_executor.drain() {
        Ok(drained) => {
            let disconnected_after_ready = match shared.message_executor.disconnected_after_ready()
            {
                Ok(disconnected) => disconnected,
                Err(error) => {
                    shared.publish_runtime_error("event_pump_failed", error);
                    return PumpTick::Stop;
                }
            };
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.draining {
                return PumpTick::Stop;
            }
            for message in drained.messages {
                shared.publish_message(&mut state, message);
            }
            shared.finalize_consumed_response_keys(&mut state, &drained.consumed_response_keys);
            shared.rollback_rejected_response_keys(&mut state, &drained.rejected_response_keys);
            shared.retry_thread_persistence(&mut state);
            if disconnected_after_ready {
                drop(state);
                shared.publish_runtime_error(
                    "event_pump_failed",
                    HostedRunnerError::new(
                        HostedRunnerErrorCode::RuntimeFailed,
                        "native headless agent disconnected after becoming ready",
                    ),
                );
                return PumpTick::Stop;
            }
            PumpTick::Continue
        }
        Err(error) => {
            shared.publish_runtime_error("event_pump_failed", error);
            PumpTick::Stop
        }
    }
}

impl SharedRunner {
    fn retry_thread_persistence(&self, state: &mut RunnerState) {
        if !self
            .thread_persistence_retry_pending
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        // The retry mechanism itself: a raw call is intentional, failure
        // re-arms the retry flag for the next pump tick.
        #[allow(clippy::disallowed_methods)]
        match self.persist_thread(state) {
            Ok(()) if state.runtime_failed => {
                let error_type = state.last_error_type.clone();
                self.refresh_runtime_receipt(
                    state,
                    RuntimeReceiptKind::Failed,
                    None,
                    error_type.as_deref(),
                    None,
                );
            }
            Ok(()) => {}
            Err(error) => {
                self.thread_persistence_retry_pending
                    .store(true, Ordering::Release);
                self.schedule_thread_persistence_recovery();
                tracing::warn!(
                    event = "event_pump_idempotency_persistence_retry_failed",
                    error = %error,
                    "thread journal persistence retry failed; retaining the retry boundary for the independent recovery supervisor",
                );
            }
        }
    }

    /// Keep retrying a failed journal boundary even when the event pump that
    /// normally consumes the retry flag has stopped. A single supervisor is
    /// shared across all callers so a transient failure cannot create an
    /// unbounded set of retry tasks.
    fn schedule_thread_persistence_recovery(&self) {
        if self.thread_persistence_recovery_cancellation.is_cancelled() {
            return;
        }
        if !self
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
        {
            return;
        }
        if self
            .thread_persistence_recovery_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let shared = self.clone();
        let running = self.thread_persistence_recovery_running.clone();
        let cancellation = self.thread_persistence_recovery_cancellation.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            running.store(false, Ordering::Release);
            tracing::error!(
                event = "thread_journal_recovery_supervisor_unavailable",
                "thread journal persistence retry remains pending without a Tokio runtime"
            );
            return;
        }

        let task = tokio::spawn(async move {
            loop {
                if cancellation.is_cancelled() {
                    break;
                }
                if !shared
                    .thread_persistence_retry_pending
                    .load(Ordering::Acquire)
                {
                    break;
                }
                let lifecycle = shared.mutation_lifecycle.clone();
                let _lifecycle = tokio::select! {
                    () = cancellation.cancelled() => break,
                    lifecycle = lifecycle.lock() => lifecycle,
                };
                if cancellation.is_cancelled() {
                    break;
                }
                {
                    let mut state = shared
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    shared.retry_thread_persistence(&mut state);
                }
                if shared
                    .thread_persistence_retry_pending
                    .load(Ordering::Acquire)
                {
                    tokio::select! {
                        () = cancellation.cancelled() => break,
                        () = tokio::time::sleep(THREAD_PERSISTENCE_RECOVERY_RETRY_DELAY) => {}
                    }
                } else {
                    break;
                }
            }

            running.store(false, Ordering::Release);
            if !cancellation.is_cancelled()
                && shared
                    .thread_persistence_retry_pending
                    .load(Ordering::Acquire)
            {
                shared.schedule_thread_persistence_recovery();
            }
        });
        let mut task_slot = self
            .thread_persistence_recovery_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *task_slot = Some(task);
    }

    pub(super) async fn stop_thread_persistence_recovery(&self) {
        self.thread_persistence_recovery_cancellation.cancel();
        self.thread_persistence_retry_pending
            .store(false, Ordering::Release);
        let task = self
            .thread_persistence_recovery_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.thread_persistence_recovery_running
            .store(false, Ordering::Release);
    }

    /// Roll back the in-memory records for rejected response keys.
    /// Infallible by design: the removals always happen, and a failed journal
    /// write is deferred to the event pump's retry. Callers must not treat a
    /// rejection rollback as a reason to stop the runtime.
    fn rollback_rejected_response_keys(&self, state: &mut RunnerState, keys: &[String]) {
        let mut changed = false;
        for key in keys {
            changed |= state.pending_response_idempotency.remove(key).is_some();
            let owner_count = state.response_request_owners.len();
            state
                .response_request_owners
                .retain(|_, owner| owner != key);
            changed |= state.response_request_owners.len() != owner_count;
            let prior_len = state.pending_response_idempotency_order.len();
            state
                .pending_response_idempotency_order
                .retain(|entry| entry != key);
            changed |= state.pending_response_idempotency_order.len() != prior_len;
        }
        if changed {
            self.persist_thread_or_defer(state, "rollback_rejected_response_keys");
        }
    }

    /// Promote consumed response keys to durable idempotency records.
    /// Infallible by design: the in-memory promotion always happens, and a
    /// failed journal write is deferred to the event pump's retry.
    fn finalize_consumed_response_keys(&self, state: &mut RunnerState, keys: &[String]) {
        let mut changed = false;
        for key in keys {
            let Some(message) = state.pending_response_idempotency.remove(key) else {
                continue;
            };
            state
                .pending_response_idempotency_order
                .retain(|entry| entry != key);
            if !state.response_idempotency_keys.contains(key) {
                if state.response_idempotency_order.len() >= MAX_RESPONSE_IDEMPOTENCY_RECORDS {
                    if let Some(evicted_key) = state.response_idempotency_order.pop_front() {
                        state.response_idempotency_keys.remove(&evicted_key);
                        state.response_idempotency_digests.remove(&evicted_key);
                        state
                            .response_request_owners
                            .retain(|_, owner| owner != &evicted_key);
                    }
                }
                state.response_idempotency_order.push_back(key.clone());
            }
            state.response_idempotency_keys.insert(key.clone());
            state
                .response_idempotency_digests
                .insert(key.clone(), response_message_digest(&message));
            changed = true;
        }
        if changed {
            self.persist_thread_or_defer(state, "finalize_consumed_response_keys");
        }
    }

    fn start_event_pump(&self, maintenance_interval: Duration) {
        let shared = self.clone();
        let cancelled = self.event_pump_cancellation.clone();
        let task = tokio::spawn(async move {
            pump_agent_events(shared, cancelled, maintenance_interval).await;
        });
        *self
            .event_pump_task
            .try_lock()
            .expect("event pump is not stopped while it starts") = Some(task);
    }

    async fn stop_event_pump(&self) -> Result<(), HostedRunnerError> {
        self.event_pump_cancellation.cancel();
        let mut task = self.event_pump_task.lock().await;
        if let Some(join_handle) = task.as_mut() {
            if let Err(error) = join_handle.await {
                let error = HostedRunnerError::internal(format!(
                    "event pump task terminated unexpectedly: {error}"
                ));
                self.publish_runtime_error("event_pump_join_failed", error.clone());
                task.take();
                return Err(error);
            }
        }
        task.take();
        Ok(())
    }

    fn ensure_mutation_allowed(&self) -> HostedResult<()> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.ready || state.draining {
            return Err(runtime_availability_error(
                &state,
                "hosted runner is draining or not ready",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn last_published_cursor(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cursor
    }

    #[cfg(test)]
    fn event_pump_is_finished(&self) -> bool {
        self.event_pump_task
            .try_lock()
            .map(|task| {
                task.as_ref()
                    .is_none_or(tokio::task::JoinHandle::is_finished)
            })
            .unwrap_or(false)
    }

    fn publish_runtime_error(&self, error_type: &str, error: HostedRunnerError) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.runtime_failed = true;
        state.ready = false;
        state.last_status = Some("Runtime failed".to_string());
        state.last_error = Some(error.message.clone());
        state.last_error_type = Some(error_type.to_string());
        self.publish_message_with_fatal_category(
            &mut state,
            FromAgentMessage::Error {
                request_id: None,
                message: error.message,
                fatal: true,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Fatal),
            },
            Some(error_type),
        );
        if self
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
        {
            // A fatal event can be the reason the event pump is stopping, so
            // its normal next-tick retry is not available. Stage a durable
            // failure snapshot and retry once while this error path still
            // owns the mutation lock.
            state.last_error_type = Some(error_type.to_string());
            state.thread.set_runtime_failure_type(error_type);
            let failure_snapshot = StreamEnvelope::Snapshot {
                snapshot: self.public_snapshot(&state),
            };
            state.envelopes.push_back(failure_snapshot.clone());
            state.controller_envelopes.push_back(failure_snapshot);
            while state.envelopes.len() > MAX_EVENTS {
                state.envelopes.pop_front();
            }
            while state.controller_envelopes.len() > MAX_EVENTS {
                state.controller_envelopes.pop_front();
            }
            self.retry_thread_persistence(&mut state);
        }
    }
}

impl HostedError {
    fn new(code: HostedRunnerErrorCode, message: impl Into<String>) -> Self {
        Self {
            status: code.http_status(),
            code,
            message: message.into(),
            details: None,
        }
    }

    fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl From<HostedRunnerError> for HostedError {
    fn from(error: HostedRunnerError) -> Self {
        Self::new(error.code, error.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedRunnerErrorCode {
    InvalidConfig,
    InvalidSnapshotManifest,
    BadRequest,
    NotFound,
    StaleSession,
    StaleConnection,
    AccessDenied,
    RuntimeNotReady,
    LeaseConflict,
    RuntimeOwnedElsewhere,
    WorkspaceViolation,
    UnsupportedCapability,
    RuntimeFailed,
    ResponseCapacity,
    IdempotencyConflict,
    Internal,
}

impl HostedRunnerErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_config",
            Self::InvalidSnapshotManifest => "invalid_snapshot_manifest",
            Self::BadRequest => "bad_request",
            Self::NotFound => "not_found",
            Self::StaleSession => "stale_session",
            Self::StaleConnection => "stale_connection",
            Self::AccessDenied => "access_denied",
            Self::RuntimeNotReady => "runtime_not_ready",
            Self::LeaseConflict => "controller_lease_held",
            Self::RuntimeOwnedElsewhere => "runtime_owned_elsewhere",
            Self::WorkspaceViolation => "workspace_violation",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::RuntimeFailed => "runtime_failed",
            Self::ResponseCapacity => "response_capacity_exhausted",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::Internal => "internal_error",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidConfig | Self::InvalidSnapshotManifest | Self::BadRequest => 400,
            Self::AccessDenied | Self::WorkspaceViolation => 403,
            Self::NotFound | Self::StaleSession | Self::StaleConnection => 404,
            Self::RuntimeNotReady | Self::ResponseCapacity => 503,
            Self::LeaseConflict | Self::RuntimeOwnedElsewhere => 409,
            Self::UnsupportedCapability => 501,
            Self::RuntimeFailed | Self::Internal => 500,
            Self::IdempotencyConflict => 409,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedRunnerError {
    pub code: HostedRunnerErrorCode,
    pub message: String,
}

impl HostedRunnerError {
    fn new(code: HostedRunnerErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_config(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::InvalidConfig, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::BadRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::NotFound, message)
    }

    pub fn runtime_not_ready(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::RuntimeNotReady, message)
    }

    pub fn lease_conflict(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::LeaseConflict, message)
    }

    pub fn workspace_violation(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::WorkspaceViolation, message)
    }

    pub fn unsupported_capability(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::UnsupportedCapability, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(HostedRunnerErrorCode::Internal, message)
    }

    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }
}

impl std::fmt::Display for HostedRunnerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for HostedRunnerError {}

async fn serve(listener: TcpListener, shared: SharedRunner, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((socket, peer_addr)) = accepted else {
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = handle_socket(socket, shared, peer_addr).await;
                });
            }
        }
    }
}

async fn serve_mtls(
    listener: TcpListener,
    shared: SharedRunner,
    identity: workload_identity::ReloadableServerIdentity,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((socket, peer_addr)) = accepted else {
                    continue;
                };
                let shared = shared.clone();
                let identity = identity.clone();
                let connection_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let Some((tls_config, identity_changed)) =
                        identity.snapshot(Utc::now()).await
                    else {
                        return;
                    };
                    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
                    let socket = tokio::select! {
                        () = connection_shutdown.cancelled() => return,
                        () = identity_changed.cancelled() => return,
                        result = acceptor.accept(socket) => {
                            let Ok(socket) = result else {
                                return;
                            };
                            socket
                        }
                    };
                    tokio::select! {
                        () = connection_shutdown.cancelled() => {}
                        () = identity_changed.cancelled() => {}
                        _ = handle_socket(socket, shared, peer_addr) => {}
                    }
                });
            }
        }
    }
}

async fn handle_socket<S>(
    mut socket: S,
    shared: SharedRunner,
    peer_addr: SocketAddr,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(request) = read_request(&mut socket).await? else {
        return Ok(());
    };

    let response = route_request(request, shared, peer_addr).await;
    match response {
        Ok(ResponseBody::Json { status, body }) => {
            write_json_value(&mut socket, status, body).await
        }
        Ok(ResponseBody::Sse {
            replay,
            mut rx,
            shared,
            mut filter,
            controller_authorization,
        }) => {
            write_sse_headers(&mut socket).await?;
            if controller_authorization
                .as_ref()
                .is_some_and(|authorization| !shared.controller_stream_is_authorized(authorization))
            {
                return Ok(());
            }
            for envelope in replay {
                for envelope in filter.apply(envelope) {
                    if !write_sse_event_if_authorized(
                        &mut socket,
                        &shared,
                        controller_authorization.as_ref(),
                        &envelope,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
            }
            loop {
                match rx.recv().await {
                    Ok(envelope) => {
                        if controller_authorization
                            .as_ref()
                            .is_some_and(|authorization| {
                                !shared.controller_stream_is_authorized(authorization)
                            })
                        {
                            break;
                        }
                        for envelope in filter.apply(envelope) {
                            if !write_sse_event_if_authorized(
                                &mut socket,
                                &shared,
                                controller_authorization.as_ref(),
                                &envelope,
                            )
                            .await?
                            {
                                return Ok(());
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let envelope = shared.reset_envelope(format!("broadcast_lag:{skipped}"));
                        for envelope in filter.apply(envelope) {
                            if !write_sse_event_if_authorized(
                                &mut socket,
                                &shared,
                                controller_authorization.as_ref(),
                                &envelope,
                            )
                            .await?
                            {
                                return Ok(());
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            Ok(())
        }
        Err(error) => write_error(&mut socket, error).await,
    }
}

enum ResponseBody {
    Json {
        status: u16,
        body: serde_json::Value,
    },
    Sse {
        replay: Vec<StreamEnvelope>,
        rx: broadcast::Receiver<StreamEnvelope>,
        // Keep the common JSON response representation small. `SharedRunner`
        // owns the runtime state and event-pump handles, so storing it inline
        // makes this enum unnecessarily large even though only SSE responses
        // need it.
        shared: Box<SharedRunner>,
        filter: Box<TranscriptStreamFilter>,
        controller_authorization: Option<ControllerStreamAuthorization>,
    },
}

struct ControllerStreamAuthorization {
    connection_id: String,
    subscription_id: String,
    cancellation: CancellationToken,
}

async fn write_sse_event_if_authorized<S>(
    socket: &mut S,
    shared: &SharedRunner,
    authorization: Option<&ControllerStreamAuthorization>,
    envelope: &StreamEnvelope,
) -> io::Result<bool>
where
    S: AsyncWrite + Unpin,
{
    let Some(authorization) = authorization else {
        write_sse_event(socket, envelope).await?;
        return Ok(true);
    };
    if !shared.controller_stream_is_authorized(authorization) {
        return Ok(false);
    }
    tokio::select! {
        biased;
        () = authorization.cancellation.cancelled() => Ok(false),
        result = write_sse_event(socket, envelope) => result.map(|()| true),
    }
}

struct TranscriptStreamFilter {
    grade: crate::transcript::TranscriptGrade,
    resume_cursor: u64,
    response_chunks: HashMap<String, (u64, String)>,
    active_responses: HashSet<String>,
    deferred_blocks: Vec<StreamEnvelope>,
}

impl TranscriptStreamFilter {
    fn new(grade: crate::transcript::TranscriptGrade, resume_cursor: u64) -> Self {
        Self {
            grade,
            resume_cursor,
            response_chunks: HashMap::new(),
            active_responses: HashSet::new(),
            deferred_blocks: Vec::new(),
        }
    }

    fn apply(&mut self, envelope: StreamEnvelope) -> Vec<StreamEnvelope> {
        self.apply_unfiltered(envelope)
            .into_iter()
            .filter(|envelope| match envelope {
                StreamEnvelope::Message { cursor, .. } | StreamEnvelope::Heartbeat { cursor } => {
                    self.resume_cursor == 0 || *cursor > self.resume_cursor
                }
                StreamEnvelope::Snapshot { snapshot } => {
                    self.resume_cursor == 0 || snapshot.cursor > self.resume_cursor
                }
                StreamEnvelope::Reset { .. } => true,
            })
            .collect()
    }

    fn apply_unfiltered(&mut self, envelope: StreamEnvelope) -> Vec<StreamEnvelope> {
        let StreamEnvelope::Message { cursor, message } = envelope else {
            if matches!(envelope, StreamEnvelope::Reset { .. }) {
                self.response_chunks.clear();
                self.active_responses.clear();
                self.deferred_blocks.clear();
            }
            return vec![envelope];
        };
        match *message {
            FromAgentMessage::ResponseStart { response_id } => {
                let mut envelopes = std::mem::take(&mut self.deferred_blocks);
                self.response_chunks.clear();
                self.active_responses.clear();
                self.active_responses.insert(response_id.clone());
                envelopes.push(StreamEnvelope::Message {
                    cursor,
                    message: Box::new(FromAgentMessage::ResponseStart { response_id }),
                });
                envelopes
            }
            FromAgentMessage::ResponseChunk {
                response_id,
                content,
                is_thinking,
            } => {
                if self.grade == crate::transcript::TranscriptGrade::Delta {
                    return vec![StreamEnvelope::Message {
                        cursor,
                        message: Box::new(FromAgentMessage::ResponseChunk {
                            response_id,
                            content,
                            is_thinking,
                        }),
                    }];
                }
                if self.grade != crate::transcript::TranscriptGrade::Off && !is_thinking {
                    let buffered = self
                        .response_chunks
                        .entry(response_id)
                        .or_insert_with(|| (cursor, String::new()));
                    buffered.0 = cursor;
                    buffered.1.push_str(&content);
                }
                Vec::new()
            }
            FromAgentMessage::ResponseEnd {
                response_id,
                usage,
                tools_summary,
                duration_ms,
                ttft_ms,
            } => {
                let mut envelopes = Vec::new();
                self.active_responses.clear();
                envelopes.append(&mut self.deferred_blocks);
                if let Some((_chunk_cursor, content)) = self.response_chunks.remove(&response_id) {
                    if !content.is_empty() {
                        envelopes.push(StreamEnvelope::Message {
                            // Response publication reserves this cursor for the
                            // reconstructed aggregate, leaving the advancing
                            // completion cursor independently resumable.
                            cursor: cursor.saturating_sub(1),
                            message: Box::new(FromAgentMessage::ResponseChunk {
                                response_id: response_id.clone(),
                                content,
                                is_thinking: false,
                            }),
                        });
                    }
                }
                self.response_chunks.clear();
                envelopes.push(StreamEnvelope::Message {
                    cursor,
                    message: Box::new(FromAgentMessage::ResponseEnd {
                        response_id,
                        usage,
                        tools_summary,
                        duration_ms,
                        ttft_ms,
                    }),
                });
                envelopes
            }
            message => {
                let terminal_boundary = matches!(
                    &message,
                    FromAgentMessage::Error { fatal: true, .. }
                        | FromAgentMessage::Error { terminal: true, .. }
                        | FromAgentMessage::ProviderError { .. }
                        | FromAgentMessage::TurnCompleted { .. }
                        | FromAgentMessage::TurnInterrupted { .. }
                );
                let level = transcript_level(&message);
                if level.is_none_or(|level| self.grade.includes(level)) {
                    let envelope = StreamEnvelope::Message {
                        cursor,
                        message: Box::new(message),
                    };
                    if terminal_boundary {
                        self.response_chunks.clear();
                        self.active_responses.clear();
                        let mut envelopes = std::mem::take(&mut self.deferred_blocks);
                        envelopes.push(envelope);
                        return envelopes;
                    }
                    if level.is_some()
                        && self.grade != crate::transcript::TranscriptGrade::Delta
                        && !self.active_responses.is_empty()
                    {
                        self.deferred_blocks.push(envelope);
                        Vec::new()
                    } else {
                        vec![envelope]
                    }
                } else {
                    Vec::new()
                }
            }
        }
    }
}

fn transcript_level(message: &FromAgentMessage) -> Option<crate::transcript::TranscriptLevel> {
    use crate::transcript::TranscriptLevel;
    match message {
        FromAgentMessage::ToolCall {
            requires_approval: false,
            ..
        }
        | FromAgentMessage::ToolStart { .. }
        | FromAgentMessage::ToolEnd(tool_wire::ToolEnd { .. })
        | FromAgentMessage::Compaction { .. } => Some(TranscriptLevel::Block),
        FromAgentMessage::ToolOutput { .. } | FromAgentMessage::Status { .. } => {
            Some(TranscriptLevel::Delta)
        }
        _ => None,
    }
}

async fn route_request(
    request: HttpRequest,
    shared: SharedRunner,
    peer_addr: SocketAddr,
) -> HostedResult<ResponseBody> {
    let parent = TraceHeaders::from_values(
        request.headers.get("traceparent").map(String::as_str),
        request.headers.get("tracestate").map(String::as_str),
    );
    let span = server_span(&request.method, &route_class(&request.path), &parent);
    let started = std::time::Instant::now();
    let result = route_request_inner(request, shared, peer_addr)
        .instrument(span.clone())
        .await;
    match &result {
        Ok(ResponseBody::Json { status, .. }) => {
            span.record("http.response.status_code", i64::from(*status));
            record_outcome(
                &span,
                if *status < 400 { "success" } else { "error" },
                started.elapsed(),
                (*status >= 500).then_some("http_server_error"),
            );
        }
        Ok(ResponseBody::Sse { .. }) => {
            span.record("http.response.status_code", 200_i64);
            record_outcome(&span, "stream", started.elapsed(), None);
        }
        Err(error) => {
            span.record("http.response.status_code", i64::from(error.status));
            record_outcome(&span, "error", started.elapsed(), Some(error.code.as_str()));
        }
    }
    result
}

async fn route_request_inner(
    request: HttpRequest,
    shared: SharedRunner,
    peer_addr: SocketAddr,
) -> HostedResult<ResponseBody> {
    if request.path.starts_with("/api/headless/")
        || (matches!(
            request.path.as_str(),
            HOSTED_RUNNER_DRAIN_PATH | HOSTED_RUNNER_RECEIPT_PATH
        ) && !peer_addr.ip().is_loopback())
    {
        require_auth_header(&request.headers, shared.config.auth_token.as_deref())?;
    }
    let mutation_lifecycle = (request.method == "POST"
        && request.path != HOSTED_RUNNER_DRAIN_PATH
        && !request.path.ends_with("/messages")
        && !request.path.ends_with("/message"))
    .then(|| shared.mutation_lifecycle.clone());
    let _mutation = match mutation_lifecycle.as_ref() {
        Some(lifecycle) => {
            let mutation = lifecycle.lock().await;
            shared.ensure_mutation_allowed()?;
            Some(mutation)
        }
        None => None,
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", HOSTED_RUNNER_IDENTITY_PATH) => json_response(200, shared.identity()),
        ("GET", "/readyz" | "/healthz") => {
            let state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.ready && !state.draining {
                // Health probes are intentionally unauthenticated. Keep them
                // to a boolean liveness/readiness result; the authenticated
                // receipt route owns generation and tenant metadata.
                json_response(200, json!({"ok": true}))
            } else {
                Err(runtime_availability_error(
                    &state,
                    "hosted runner is draining or not ready",
                ))
            }
        }
        ("GET", HOSTED_RUNNER_RECEIPT_PATH) => shared.runtime_receipt().map_or_else(
            || {
                Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeNotReady,
                    "hosted runtime has not produced a durable receipt",
                ))
            },
            |receipt| json_response(200, receipt),
        ),
        ("POST", HOSTED_RUNNER_DRAIN_PATH) => {
            let input = parse_json::<DrainRequest>(&request.body)?;
            handle_drain(shared, input).await
        }
        ("POST", "/api/headless/connections") => {
            shared.ensure_attachable()?;
            let input = parse_json::<ConnectionCreateRequest>(&request.body)?;
            handle_connection_create(shared, input)
        }
        ("GET", path)
            if path.starts_with("/api/headless/threads/")
                && !path.ends_with("/events")
                && !path.ends_with("/turns") =>
        {
            let thread_id = thread_id_from_path(path, "")?;
            handle_thread_state(shared, thread_id)
        }
        ("GET", path)
            if path.starts_with("/api/headless/threads/") && path.ends_with("/events") =>
        {
            let thread_id = thread_id_from_path(path, "/events")?;
            handle_events(
                shared,
                thread_id,
                request.query,
                EventsRouteIdentity::Thread,
            )
        }
        ("POST", path)
            if path.starts_with("/api/headless/threads/") && path.ends_with("/initial-actions") =>
        {
            let thread_id = thread_id_from_path(path, "/initial-actions")?;
            let input = parse_json::<initial_actions::InitialActionRequest>(&request.body)?;
            initial_actions::handle_initial_action(shared, thread_id, request.headers, input)
        }
        ("POST", path)
            if path.starts_with("/api/headless/threads/") && path.ends_with("/turns") =>
        {
            let thread_id = thread_id_from_path(path, "/turns")?;
            let input = parse_json::<AppendTurnRequest>(&request.body)?;
            handle_append_turn(shared, thread_id, request.headers, input).await
        }
        ("GET", path)
            if path.starts_with("/api/headless/sessions/") && path.ends_with("/state") =>
        {
            let session_id = session_id_from_path(path, "/state")?;
            handle_state(shared, session_id)
        }
        ("POST", path)
            if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") =>
        {
            shared.ensure_attachable()?;
            let session_id = session_id_from_path(path, "/subscribe")?;
            let input = parse_json::<SubscribeRequest>(&request.body)?;
            handle_subscribe(shared, session_id, input)
        }
        ("GET", path)
            if path.starts_with("/api/headless/sessions/") && path.ends_with("/events") =>
        {
            let session_id = session_id_from_path(path, "/events")?;
            handle_events(
                shared,
                session_id,
                request.query,
                EventsRouteIdentity::Session,
            )
        }
        ("POST", path)
            if path.starts_with("/api/headless/sessions/")
                && (path.ends_with("/messages") || path.ends_with("/message")) =>
        {
            let session_id = if path.ends_with("/messages") {
                session_id_from_path(path, "/messages")?
            } else {
                session_id_from_path(path, "/message")?
            };
            let message = parse_json::<ToAgentMessage>(&request.body)?;
            handle_message(shared, session_id, request.headers, message).await
        }
        ("POST", path)
            if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") =>
        {
            let session_id = session_id_from_path(path, "/heartbeat")?;
            let input = parse_json::<HeartbeatRequest>(&request.body)?;
            handle_heartbeat(shared, session_id, input)
        }
        ("POST", path)
            if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect") =>
        {
            let session_id = session_id_from_path(path, "/disconnect")?;
            let input = parse_json::<DisconnectRequest>(&request.body)?;
            handle_disconnect(shared, session_id, input)
        }
        _ => Err(HostedError::new(
            HostedRunnerErrorCode::NotFound,
            "route not found",
        )),
    }
}

fn require_auth_header(
    headers: &HashMap<String, String>,
    expected_token: Option<&str>,
) -> HostedResult<()> {
    let Some(expected_token) = normalize_auth_token(expected_token) else {
        return Ok(());
    };
    let bearer_token = headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|value| normalize_auth_token(Some(value)));
    let runner_token = headers
        .get("x-maestro-hosted-runner-token")
        .and_then(|value| normalize_auth_token(Some(value)));
    let legacy_api_token = headers
        .get("x-maestro-api-key")
        .or_else(|| headers.get("x-composer-api-key"))
        .and_then(|value| normalize_auth_token(Some(value)));
    if bearer_token == Some(expected_token)
        || runner_token == Some(expected_token)
        || legacy_api_token == Some(expected_token)
    {
        return Ok(());
    }
    Err(HostedError::new(
        HostedRunnerErrorCode::AccessDenied,
        "missing or invalid hosted runner auth token",
    ))
}

fn normalize_auth_token(token: Option<&str>) -> Option<&str> {
    token.map(str::trim).filter(|value| !value.is_empty())
}

async fn handle_drain(shared: SharedRunner, input: DrainRequest) -> HostedResult<ResponseBody> {
    let _lifecycle = shared.mutation_lifecycle.lock().await;
    let export_paths = input
        .export_paths
        .clone()
        .unwrap_or_else(|| vec![".".to_string()]);
    for export_path in &export_paths {
        let _ = resolve_workspace_path(
            &shared.config.workspace_root,
            None,
            Some(export_path.as_str()),
        )?;
    }
    let retry_finalization = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.drain_finalization_pending || state.pending_executor_drain_result.is_some() {
            // The executor and event pump were already drained. Restore the
            // state that preceded the failed final journal write and retry
            // only the durable hand-off; a second drain() would be an
            // invalid replay of an already-consumed executor boundary.
            state.runtime_failed = state.drain_runtime_failed_before_finalization;
            state.drain_executor_pending = false;
            if !state.runtime_failed {
                state.last_error = None;
                state.last_error_type = None;
            }
            state.last_status = Some("Drained".to_string());
            true
        } else if state.drain_executor_pending {
            // The generation-level Draining boundary is durable, but the
            // executor hand-off failed or was interrupted. Retry that phase
            // instead of reporting an unrecoverable already-draining state.
            false
        } else if state.draining {
            return Err(HostedError::new(
                HostedRunnerErrorCode::RuntimeNotReady,
                "hosted runner is already draining",
            ));
        } else {
            let previous_ready = state.ready;
            let previous_draining = state.draining;
            let previous_last_status = state.last_status.clone();
            state.draining = true;
            state.ready = false;
            state.last_status = Some("Draining".to_string());
            if let Err(error) = shared.persist_receipt_boundary(
                &mut state,
                RuntimeReceiptKind::Draining,
                "drain_start",
            ) {
                // The pump is still running, so this request can safely be
                // retried. Do not leave an in-memory Draining state behind
                // when its receipt boundary never reached the journal.
                state.ready = previous_ready;
                state.draining = previous_draining;
                state.last_status = previous_last_status;
                return Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    format!("failed to persist draining runtime state: {error}"),
                ));
            }
            state.drain_executor_pending = true;
            false
        }
    };

    if !retry_finalization {
        shared.stop_event_pump().await.map_err(HostedError::from)?;
        // This call is intentionally repeated for a retry after a failed
        // executor drain. `stop_event_pump` is idempotent once the first
        // attempt has stopped the task, and also fences a newly restarted
        // pump before resuming the pending executor hand-off.
        let replay_cursor = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cursor;
        let drained = match shared.message_executor.drain() {
            Ok(drained) => drained,
            Err(error) => return Err(HostedError::from(error)),
        };
        {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.pending_executor_drain_result = Some(PendingExecutorDrainResult {
                replay_cursor,
                result: drained,
            });
            state.executor_drain_result_applied_count = 0;
            state.executor_drain_result_applied = false;
            state.drain_executor_pending = false;
            // Mark this as an in-memory finalization hand-off before writing
            // the private result. If that write fails, the next request can
            // retry the write from `pending_executor_drain_result` without
            // invoking the non-idempotent executor drain a second time.
            state.drain_finalization_pending = true;
            state.drain_runtime_failed_before_finalization = state.runtime_failed;
            state.last_status = Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS.to_string());
            if let Some(pending) = state.pending_executor_drain_result.as_ref() {
                if let Err(error) = shared.persist_executor_drain_result(&state, pending) {
                    state.last_error = Some(format!(
                        "executor drain result handoff persistence failed: {error}"
                    ));
                    state.last_error_type = Some("internal".to_string());
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::RuntimeFailed,
                        "failed to retain executor drain result",
                    ));
                }
            }
            let drained_messages = state
                .pending_executor_drain_result
                .as_ref()
                .map(|pending| pending.result.messages.len())
                .unwrap_or_default();
            if state.cursor > 0 || !state.connections.is_empty() || drained_messages > 0 {
                let reason = input
                    .reason
                    .as_deref()
                    .unwrap_or("platform_requested_drain");
                shared.publish_message(
                    &mut state,
                    FromAgentMessage::Status {
                        message: format!("Hosted runner is draining: {reason}"),
                    },
                );
            }
            if !shared.apply_pending_executor_drain_result(&mut state) {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    "failed to apply retained executor drain result",
                ));
            }
        }
    }

    if retry_finalization {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pending) = state.pending_executor_drain_result.clone() {
            if let Err(error) = shared.persist_executor_drain_result(&state, &pending) {
                state.last_status =
                    Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS.to_string());
                state.last_error = Some(format!(
                    "executor drain result handoff persistence failed: {error}"
                ));
                state.last_error_type = Some("internal".to_string());
                return Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    "failed to retain executor drain result",
                ));
            }
            if !shared.apply_pending_executor_drain_result(&mut state) {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    "failed to apply retained executor drain result",
                ));
            }
        }
    }

    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.drain_finalization_pending = true;
        state.drain_runtime_failed_before_finalization = state.runtime_failed;
        state.last_status = Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS.to_string());
        if let Err(error) = shared.persist_drain_finalization_boundary(&mut state) {
            state.runtime_failed = true;
            state.ready = false;
            state.last_status = Some("Runtime failed".to_string());
            state.last_error = Some(format!(
                "durable executor-complete drain boundary write failed: {error}"
            ));
            state.last_error_type = Some("internal".to_string());
            return Err(HostedError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                "failed to persist executor-complete drain boundary",
            ));
        }
    }
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A restart may have loaded the private handoff after the marker was
        // written but before the final snapshot was accepted. Reconcile that
        // result before composing the manifest; the normal in-process path
        // already applied it and has no pending value here.
        if !shared.apply_pending_executor_drain_result(&mut state) {
            return Err(HostedError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                "failed to apply retained executor drain result",
            ));
        }
    }
    let (manifest_path, manifest) = write_snapshot_manifest(&shared, &input).await?;
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.last_status = Some("Drained".to_string());
        let snapshot_lineage = manifest
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .map(manifests::snapshot_lineage_from_created_at)
            .transpose()?
            .ok_or_else(|| {
                HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    "drain manifest is missing its creation time",
                )
            })?;
        state.thread.set_snapshot_lineage(&snapshot_lineage);
        if !shared.publish_snapshot(&mut state) {
            state.drain_finalization_pending = true;
            return Err(HostedError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                "failed to persist final drained runtime state",
            ));
        }
        state.drain_finalization_pending = false;
        shared.refresh_runtime_receipt(
            &mut state,
            RuntimeReceiptKind::Drained,
            None,
            None,
            Some(snapshot_lineage),
        );
        if let Err(error) = shared.clear_executor_drain_result(&state) {
            tracing::warn!(
                event = "executor_drain_result_cleanup_failed",
                error = %error,
                "final drained state is durable but its crash-recovery handoff could not be removed",
            );
        }
        state.pending_executor_drain_result = None;
        state.executor_drain_result_applied_count = 0;
        state.executor_drain_result_applied = false;
    }
    let runtime_receipt = shared.runtime_receipt();
    json_response(
        200,
        json!({
            "protocol_version": HOSTED_RUNNER_DRAIN_PROTOCOL_VERSION,
            "status": "drained",
            "runner_session_id": shared.config.runner_session_id,
            "requested_by": input.requested_by,
            "reason": input.reason,
            "manifest_path": manifest_path.to_string_lossy(),
            "manifest": manifest,
            "runtime_receipt": runtime_receipt,
        }),
    )
}

fn handle_connection_create(
    shared: SharedRunner,
    input: ConnectionCreateRequest,
) -> HostedResult<ResponseBody> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_connection_session_id(&shared, &mut state, input.session_id.as_deref())?;
    let connection_id = input
        .connection_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("conn_{}", Uuid::new_v4().simple()));
    let role = input.role.unwrap_or(ConnectionRole::Controller);
    if !crate::headless::messages::client_protocol_version_is_supported(
        input.protocol_version.as_deref(),
    ) {
        return Err(HostedError::new(
            HostedRunnerErrorCode::UnsupportedCapability,
            crate::headless::messages::unsupported_client_protocol_version_message(
                input.protocol_version.as_deref().unwrap_or_default(),
            ),
        ));
    }
    // Requests predating capability negotiation always sent a protocol version.
    // Protocol-less in-process callers keep the secure default used by existing
    // tests and embedders; wire clients explicitly opt in or follow the legacy lane.
    let connection_capability_required = input.connection_capability_required
        || (!state.connections.contains_key(&connection_id) && input.protocol_version.is_none());
    let connection_capability = upsert_connection(
        &mut state,
        ConnectionUpsert {
            connection_id: connection_id.clone(),
            connection_capability: input.connection_capability,
            connection_capability_required,
            role,
            client_protocol_version: input.protocol_version,
            client_info: input.client_info,
            capabilities: input.capabilities.map(Into::into),
            opt_out_notifications: input.opt_out_notifications,
            take_control: input.take_control,
        },
    )?;
    let snapshot = shared.public_snapshot(&state);
    let controller_lease_granted = role == ConnectionRole::Controller
        && state.controller_connection_id.as_deref() == Some(&connection_id);
    let lease_expires_at = state.connections.get(&connection_id).map(lease_expires_at);
    json_response(
        200,
        json!({
            "session_id": state.session_id,
            "connection_id": connection_id,
            "connection_capability": connection_capability,
            "connection_capability_required": connection_capability_required,
            "role": role,
            "controller_lease_granted": controller_lease_granted,
            "controller_connection_id": state.controller_connection_id,
            "lease_expires_at": lease_expires_at,
            "heartbeat_interval_ms": DEFAULT_HEARTBEAT_INTERVAL_MS,
            "governed_tool_grant_algorithms": governed_grant_algorithms_for_executor(shared.message_executor.as_ref()),
            "snapshot": snapshot,
        }),
    )
}

fn handle_subscribe(
    shared: SharedRunner,
    session_id: &str,
    input: SubscribeRequest,
) -> HostedResult<ResponseBody> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_session_id(&shared.binding, Some(session_id))?;
    let role = input.role.unwrap_or(ConnectionRole::Controller);
    if !crate::headless::messages::client_protocol_version_is_supported(
        input.protocol_version.as_deref(),
    ) {
        return Err(HostedError::new(
            HostedRunnerErrorCode::UnsupportedCapability,
            crate::headless::messages::unsupported_client_protocol_version_message(
                input.protocol_version.as_deref().unwrap_or_default(),
            ),
        ));
    }
    let connection_id = input
        .connection_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("conn_{}", Uuid::new_v4().simple()));
    let connection_capability_required = input.connection_capability_required
        || (!state.connections.contains_key(&connection_id) && input.protocol_version.is_none());
    if let Some(existing) = state.connections.get(&connection_id) {
        if existing.authority_mode == ConnectionAuthorityMode::LegacySubscription
            && !existing.subscription_ids.is_empty()
        {
            let has_private_subscription_proof = input
                .subscription_id
                .as_deref()
                .and_then(|subscription_id| {
                    state
                        .subscriptions
                        .get(subscription_id)
                        .map(|subscription| (subscription_id, subscription))
                })
                .is_some_and(|(subscription_id, subscription)| {
                    existing.subscription_ids.contains(subscription_id)
                        && subscription.connection_id == connection_id
                        && subscription.authority_mode
                            == ConnectionAuthorityMode::LegacySubscription
                });
            if !has_private_subscription_proof {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::AccessDenied,
                    "existing legacy connection requires a private subscription",
                ));
            }
        }
    }
    let connection_capability = upsert_connection(
        &mut state,
        ConnectionUpsert {
            connection_id: connection_id.clone(),
            connection_capability: input.connection_capability,
            connection_capability_required,
            role,
            client_protocol_version: input.protocol_version,
            client_info: input.client_info,
            capabilities: input.capabilities.map(Into::into),
            opt_out_notifications: input.opt_out_notifications,
            take_control: input.take_control,
        },
    )?;
    let subscription_id = format!("sub_{}", Uuid::new_v4().simple());
    state.subscriptions.insert(
        subscription_id.clone(),
        SubscriptionRecord {
            connection_id: connection_id.clone(),
            connection_capability: connection_capability.clone(),
            authority_mode: if connection_capability_required {
                ConnectionAuthorityMode::Capability
            } else {
                ConnectionAuthorityMode::LegacySubscription
            },
            role,
            attached: true,
        },
    );
    if let Some(connection) = state.connections.get_mut(&connection_id) {
        connection.subscription_ids.insert(subscription_id.clone());
    }
    if role == ConnectionRole::Controller {
        state.controller_connection_id = Some(connection_id.clone());
    }
    state.last_status = Some("Attached".to_string());
    let connection_count = state.connections.len();
    let controller_connection_id = state.controller_connection_id.clone();
    let lease_expires_at = state.connections.get(&connection_id).map(lease_expires_at);
    shared.publish_message(
        &mut state,
        FromAgentMessage::ConnectionInfo {
            connection_id: Some(connection_id.clone()),
            client_protocol_version: None,
            client_info: None,
            capabilities: None,
            opt_out_notifications: None,
            role: Some(role),
            connection_count: Some(connection_count),
            controller_connection_id,
            lease_expires_at: lease_expires_at.clone(),
            connections: None,
        },
    );
    let snapshot = shared.public_snapshot(&state);
    let controller_pending_events = if role == ConnectionRole::Controller
        && state.controller_connection_id.as_deref() == Some(connection_id.as_str())
    {
        shared.controller_pending_events(&mut state)
    } else {
        Vec::new()
    };
    let controller_subscription_id =
        (role == ConnectionRole::Controller).then(|| subscription_id.clone());
    json_response(
        200,
        json!({
            "connection_id": connection_id,
            "connection_capability": connection_capability,
            "connection_capability_required": connection_capability_required,
            "subscription_id": subscription_id,
            "role": role,
            "controller_lease_granted": role == ConnectionRole::Controller,
            "controller_subscription_id": controller_subscription_id,
            "controller_pending_events": controller_pending_events,
            "controller_connection_id": snapshot.state.controller_connection_id,
            "lease_expires_at": lease_expires_at,
            "heartbeat_interval_ms": DEFAULT_HEARTBEAT_INTERVAL_MS,
            "governed_tool_grant_algorithms": governed_grant_algorithms_for_executor(shared.message_executor.as_ref()),
            "snapshot": snapshot,
        }),
    )
}

fn handle_state(shared: SharedRunner, session_id: &str) -> HostedResult<ResponseBody> {
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_session_id(&shared.binding, Some(session_id))?;
    let mut snapshot = serde_json::to_value(shared.public_snapshot(&state)).map_err(|error| {
        HostedError::new(
            HostedRunnerErrorCode::RuntimeFailed,
            format!("failed to serialize hosted runtime snapshot: {error}"),
        )
    })?;
    if let Some(object) = snapshot.as_object_mut() {
        object.insert(
            "runtime_receipt".to_string(),
            serde_json::to_value(state.runtime_receipt.clone()).unwrap_or(serde_json::Value::Null),
        );
    }
    json_response(200, snapshot)
}

fn handle_thread_state(shared: SharedRunner, thread_id: &str) -> HostedResult<ResponseBody> {
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_thread_id(&shared.binding, Some(thread_id))?;
    json_response(
        200,
        json!({
            "protocol_version": THREAD_PROTOCOL_VERSION,
            "thread_id": thread_id,
            "runtime_generation": shared.config.runtime_generation,
            "phase": state.thread.phase(),
            "active_turn_id": state
                .thread
                .view(state.cursor, shared.config.runtime_generation)
                .active_turn_id,
            "cursor": state.cursor,
            "turns": state
                .thread
                .view(state.cursor, shared.config.runtime_generation)
                .turns,
            "runtime": shared.public_snapshot(&state),
            "runtime_receipt": state.runtime_receipt,
        }),
    )
}

fn append_turn_message(input: AppendTurnRequest) -> ToAgentMessage {
    let AppendTurnRequest {
        protocol_version: _,
        turn_id: _,
        kind,
        content,
        attachments,
        code_mode,
        tool_grant,
        managed_inference_authorization,
    } = input;
    match (kind, code_mode, tool_grant) {
        (ThreadTurnKind::UserMessage, Some(code_mode), Some(tool_grant)) => {
            ToAgentMessage::GovernedPrompt {
                content,
                attachments,
                code_mode,
                tool_grant,
                managed_inference_authorization,
            }
        }
        (ThreadTurnKind::Steer, Some(code_mode), Some(tool_grant)) => {
            ToAgentMessage::GovernedSteer {
                content,
                attachments,
                code_mode,
                tool_grant,
                managed_inference_authorization,
            }
        }
        (ThreadTurnKind::UserMessage, _, _) => ToAgentMessage::Prompt {
            content,
            attachments,
            managed_inference_authorization,
        },
        (ThreadTurnKind::Steer, _, _) => ToAgentMessage::Steer {
            content,
            attachments,
            managed_inference_authorization,
        },
    }
}

async fn handle_append_turn(
    shared: SharedRunner,
    thread_id: &str,
    headers: HashMap<String, String>,
    input: AppendTurnRequest,
) -> HostedResult<ResponseBody> {
    let response_protocol_version = input.protocol_version.clone();
    input
        .validate()
        .map_err(|message| HostedError::new(HostedRunnerErrorCode::BadRequest, message))?;
    require_runtime_generation(&headers, shared.config.runtime_generation)?;
    let (connection_header_id, subscription_id, connection_capability) =
        connection_from_headers(&headers);
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_thread_id(&shared.binding, Some(thread_id))?;
        let connection_id = resolve_authorized_connection_id(
            &state,
            connection_header_id,
            subscription_id,
            connection_capability.as_deref(),
        )?;
        assert_controller(&state, Some(connection_id.as_str()))?;
        if input.protocol_version == GOVERNED_THREAD_PROTOCOL_VERSION
            && state
                .connections
                .get(&connection_id)
                .and_then(|connection| connection.capabilities.as_ref())
                .and_then(|capabilities| capabilities.governed_code_mode)
                != Some(true)
        {
            return Err(HostedError::new(
                HostedRunnerErrorCode::UnsupportedCapability,
                "governed thread v2 requires negotiated governed_code_mode capability",
            ));
        }
        if !state.ready || state.draining {
            return Err(runtime_availability_error(
                &state,
                "hosted thread runtime is not accepting turns",
            ));
        }
        if let Some(turn) = state.thread.turn(&input.turn_id) {
            if !turn.matches(&input) {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::LeaseConflict,
                    "turnId was already used with a different payload",
                ));
            }
            tracing::info!(
                target: "maestro.hosted",
                event = "hosted_thread_turn_replayed",
                causal_receipt_id = shared.config.causal_receipt_id.as_deref().unwrap_or(""),
                thread_id = %thread_id,
                turn_id = %turn.turn_id,
                run_id = %turn.run_id,
                phase = ?turn.phase,
                replayed = true,
            );
            return json_response(
                200,
                json!({
                    "protocol_version": response_protocol_version,
                    "thread_id": thread_id,
                    "turn_id": turn.turn_id,
                    "run_id": turn.run_id,
                    "phase": turn.phase,
                    "cursor": state.cursor,
                    "replayed": true,
                }),
            );
        }
        if input.kind == ThreadTurnKind::UserMessage && state.thread.has_active_turn() {
            return Err(HostedError::new(
                HostedRunnerErrorCode::LeaseConflict,
                "a turn is already active; append an explicit steer turn instead",
            ));
        }
        if input.protocol_version == GOVERNED_THREAD_PROTOCOL_VERSION {
            let grant = input.tool_grant.as_ref().ok_or_else(|| {
                HostedError::new(
                    HostedRunnerErrorCode::BadRequest,
                    "governed turn requires toolGrant",
                )
            })?;
            let identity = shared.config.workload_identity.as_ref().ok_or_else(|| {
                HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    "governed turns require workload-bound organization identity",
                )
            })?;
            let run_id = state.thread.planned_run_id(&input);
            verify_governed_tool_grant(
                grant,
                &GovernedGrantVerificationContext {
                    organization_id: &identity.organization_id,
                    workspace_id: &identity.workspace_id,
                    thread_id,
                    turn_id: &input.turn_id,
                    run_id: &run_id,
                    runtime_generation: shared.config.runtime_generation,
                },
                Utc::now().timestamp_millis(),
            )
            .map_err(|error| {
                HostedError::new(
                    HostedRunnerErrorCode::BadRequest,
                    format!("governed tool grant rejected: {error:#}"),
                )
            })?;
        }
        let cursor = state.cursor;
        state.thread.append(input.clone(), cursor);
        if let Err(error) = shared.persist_thread_for_request(&state) {
            state.thread.rollback_unpersisted_append(&input.turn_id);
            return Err(HostedError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                format!("failed to persist accepted thread turn: {error}"),
            ));
        }
        // A terminal receipt describes the turn that just finished. Once a
        // new turn is durably accepted, that receipt is no longer current;
        // leave the endpoint empty until the new turn reaches its own
        // lifecycle boundary instead of exposing stale terminal evidence.
        if state
            .runtime_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.kind == RuntimeReceiptKind::Terminal)
        {
            state.runtime_receipt = None;
        }
    }

    let turn_id = input.turn_id.clone();
    let mut dispatch_input = input;
    {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(receipt) = state.thread.initial_actions.get(&turn_id) {
            dispatch_input.content = receipt.context_for(&dispatch_input.content)?;
        }
    }
    let message = append_turn_message(dispatch_input);
    let execution = handle_message_inner(shared.clone(), thread_id, headers, message).await;
    if let Err(error) = execution {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cursor = state.cursor;
        state.thread.mark_failed(cursor);
        shared.persist_thread_best_effort(&state, "turn_dispatch_failed_cleanup");
        return Err(error);
    }
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cursor = state.cursor;
    state.thread.mark_dispatched(cursor);
    // Dispatch already crossed the runtime boundary. Returning an error here
    // invites a duplicate request while leaving the live turn active. Keep
    // serving the authoritative in-memory state and let the event pump retry
    // the full journal snapshot. If the process crashes before that retry,
    // restore terminalizes the durable Accepted record as Interrupted.
    shared.persist_thread_or_defer(&state, "turn_dispatch_completed");
    let turn = state
        .thread
        .turn(&turn_id)
        .expect("accepted turn remains in append-only thread");
    tracing::info!(
        target: "maestro.hosted",
        event = "hosted_thread_turn_dispatched",
        causal_receipt_id = shared.config.causal_receipt_id.as_deref().unwrap_or(""),
        thread_id = %thread_id,
        turn_id = %turn.turn_id,
        run_id = %turn.run_id,
        phase = ?turn.phase,
        replayed = false,
    );
    json_response(
        200,
        json!({
            "protocol_version": response_protocol_version,
            "thread_id": thread_id,
            "turn_id": turn.turn_id,
            "run_id": turn.run_id,
            "phase": turn.phase,
            "cursor": state.cursor,
            "replayed": false,
        }),
    )
}

fn require_runtime_generation(
    headers: &HashMap<String, String>,
    expected_generation: u64,
) -> HostedResult<()> {
    let generation = headers
        .get("x-maestro-runtime-generation")
        .ok_or_else(|| {
            HostedError::new(
                HostedRunnerErrorCode::BadRequest,
                "x-maestro-runtime-generation is required",
            )
        })?
        .parse::<u64>()
        .map_err(|_| {
            HostedError::new(
                HostedRunnerErrorCode::BadRequest,
                "x-maestro-runtime-generation must be an unsigned integer",
            )
        })?;
    if generation != expected_generation {
        return Err(HostedError::new(
            HostedRunnerErrorCode::RuntimeOwnedElsewhere,
            "runtime generation is stale",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum EventsRouteIdentity {
    Thread,
    Session,
}

fn handle_events(
    shared: SharedRunner,
    session_id: &str,
    query: HashMap<String, String>,
    identity: EventsRouteIdentity,
) -> HostedResult<ResponseBody> {
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match identity {
        EventsRouteIdentity::Thread => ensure_thread_id(&shared.binding, Some(session_id))?,
        EventsRouteIdentity::Session => ensure_session_id(&shared.binding, Some(session_id))?,
    }
    let (grade, controller_authorization) = match query.get("subscriptionId") {
        Some(subscription_id) => {
            let subscription = state.subscriptions.get(subscription_id).ok_or_else(|| {
                HostedError::new(
                    HostedRunnerErrorCode::StaleConnection,
                    "Headless subscription not found",
                )
            })?;
            let connection = state
                .connections
                .get(&subscription.connection_id)
                .ok_or_else(|| {
                    HostedError::new(
                        HostedRunnerErrorCode::StaleConnection,
                        "Headless connection not found",
                    )
                })?;
            let controller_authorization = (connection.role == ConnectionRole::Controller
                && subscription.role == ConnectionRole::Controller
                && state.controller_connection_id.as_deref() == Some(connection.id.as_str()))
            .then(|| ControllerStreamAuthorization {
                connection_id: connection.id.clone(),
                subscription_id: subscription_id.clone(),
                cancellation: state.controller_stream_cancellation.clone(),
            });
            (
                connection
                    .capabilities
                    .as_ref()
                    .and_then(|capabilities| capabilities.transcript_grade)
                    .unwrap_or_default(),
                controller_authorization,
            )
        }
        None => (crate::transcript::TranscriptGrade::default(), None),
    };
    let controller_stream = controller_authorization.is_some();
    drop(state);
    if query
        .get("cursor")
        .map(|value| value.trim_start().starts_with('-'))
        .unwrap_or(false)
    {
        let (replay, rx) = if controller_stream {
            shared.reset_and_subscribe_controller("replay_gap")
        } else {
            shared.reset_and_subscribe("replay_gap")
        };
        return Ok(ResponseBody::Sse {
            replay,
            rx,
            shared: Box::new(shared),
            filter: Box::new(TranscriptStreamFilter::new(grade, 0)),
            controller_authorization,
        });
    }
    let cursor = query
        .get("cursor")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let (replay, rx) = if controller_stream && grade == crate::transcript::TranscriptGrade::Delta {
        shared.subscribe_controller_from(cursor)
    } else if controller_stream {
        shared.subscribe_controller_coarse_from(cursor)
    } else if grade == crate::transcript::TranscriptGrade::Delta {
        shared.subscribe_from(cursor)
    } else {
        shared.subscribe_coarse_from(cursor)
    };
    Ok(ResponseBody::Sse {
        replay,
        rx,
        shared: Box::new(shared),
        filter: Box::new(TranscriptStreamFilter::new(grade, cursor)),
        controller_authorization,
    })
}

async fn handle_message(
    shared: SharedRunner,
    session_id: &str,
    headers: HashMap<String, String>,
    message: ToAgentMessage,
) -> HostedResult<ResponseBody> {
    // Keep this guard across the executor call and the completion journal
    // update. In particular, a retry for a pending idempotency key must wait
    // for the original attempt to finish instead of entering
    // `reconcile_pending` concurrently with it.
    let _lifecycle = shared.mutation_lifecycle.lock().await;
    if !shared.inbound_commands_enabled() {
        return Err(HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "inbound command path is disabled by the active rendezvous authority",
        ));
    }
    handle_message_inner(shared.clone(), session_id, headers, message).await
}

async fn handle_message_inner(
    shared: SharedRunner,
    session_id: &str,
    headers: HashMap<String, String>,
    message: ToAgentMessage,
) -> HostedResult<ResponseBody> {
    message
        .validate_managed_inference_authorization()
        .map_err(|error| HostedError::new(HostedRunnerErrorCode::BadRequest, error))?;
    let requested_response_idempotency_key = headers
        .get("x-maestro-idempotency-key")
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    let response_idempotency_key = is_control_response_message(&message)
        .then_some(requested_response_idempotency_key)
        .flatten();
    let response_idempotency_digest = response_idempotency_key
        .as_ref()
        .map(|_| response_message_digest(&message));
    let response_request_id = response_ack_request_id(&message).map(str::to_owned);
    let (connection_header_id, subscription_id, connection_capability) =
        connection_from_headers(&headers);
    let connection_id;
    let mut executor_request = None;
    let mut execution = HostedRunnerHeadlessMessageExecution::TransportOnly;
    let mut published_messages = 0usize;
    let mut reconcile_pending_response = false;
    let mut finalize_response_idempotency = true;
    let mut response_message =
        "Rust hosted runner accepted the headless message; agent execution is not attached yet"
            .to_string();
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_session_id(&shared.binding, Some(session_id))?;
        let resolved_connection_id = resolve_authorized_connection_id(
            &state,
            connection_header_id.clone(),
            subscription_id.clone(),
            connection_capability.as_deref(),
        )?;
        assert_controller(&state, Some(resolved_connection_id.as_str()))?;
        if let ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
            client_instance_id,
            ..
        }) = &message
        {
            if client_instance_id != &resolved_connection_id {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::AccessDenied,
                    "governed client tool result owner does not match the authenticated connection",
                ));
            }
        }
        if !state.ready || state.draining {
            return Err(runtime_availability_error(
                &state,
                "hosted runner is draining",
            ));
        }
        if let Some(idempotency_key) = response_idempotency_key.as_ref() {
            if let Some(request_id) = response_request_id.as_ref() {
                if let Some(owner) = state.response_request_owners.get(request_id) {
                    if owner != idempotency_key {
                        return Err(HostedError::new(
                            HostedRunnerErrorCode::IdempotencyConflict,
                            format!(
                                "protocol request {request_id} is already owned by another idempotency key"
                            ),
                        ));
                    }
                }
            }
            if state.response_idempotency_keys.contains(idempotency_key) {
                if let (Some(expected), Some(actual)) = (
                    state.response_idempotency_digests.get(idempotency_key),
                    response_idempotency_digest.as_ref(),
                ) {
                    if expected != actual {
                        return Err(HostedError::new(
                            HostedRunnerErrorCode::IdempotencyConflict,
                            "idempotency key was already used for a different response",
                        ));
                    }
                }
                if let Some(request_id) = response_request_id.as_ref() {
                    if !state.response_request_owners.contains_key(request_id) {
                        state
                            .response_request_owners
                            .insert(request_id.clone(), idempotency_key.clone());
                        shared.persist_thread_for_request(&state).map_err(|error| {
                            HostedError::new(
                                HostedRunnerErrorCode::RuntimeFailed,
                                format!("failed to persist response request ownership: {error}"),
                            )
                        })?;
                    }
                }
                let snapshot = shared.public_snapshot(&state);
                tracing::info!(
                    target: "maestro.hosted",
                    event = "hosted_message_replayed",
                    causal_receipt_id = shared.config.causal_receipt_id.as_deref().unwrap_or(""),
                    session_id = %state.session_id,
                    connection_id = %resolved_connection_id,
                    message_kind = hosted_message_kind(&message),
                    response_request_id_present = response_request_id.is_some(),
                    replayed = true,
                    cursor = snapshot.cursor,
                );
                return json_response(
                    200,
                    json!({
                        "ok": true,
                        "success": true,
                        "accepted": true,
                        "replayed": true,
                        "cursor": snapshot.cursor,
                        "execution": HostedRunnerHeadlessMessageExecution::TransportOnly,
                        "published_messages": 0,
                        "message": "Rust hosted runner replayed an idempotent headless response",
                        "snapshot": snapshot,
                    }),
                );
            }
            if let Some(pending) = state.pending_response_idempotency.get(idempotency_key) {
                let expected = response_message_digest(pending);
                if response_idempotency_digest.as_deref() != Some(expected.as_str()) {
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::IdempotencyConflict,
                        "idempotency key was already used for a different response",
                    ));
                }
                reconcile_pending_response = true;
            }
            upsert_pending_response_idempotency(
                &mut state,
                idempotency_key.clone(),
                message.clone(),
            )?;
            let inserted_request_owner = response_request_id.as_ref().is_some_and(|request_id| {
                state
                    .response_request_owners
                    .insert(request_id.clone(), idempotency_key.clone())
                    .is_none()
            });
            if let Err(error) = shared.persist_thread_for_request(&state) {
                state.pending_response_idempotency.remove(idempotency_key);
                state
                    .pending_response_idempotency_order
                    .retain(|key| key != idempotency_key);
                if inserted_request_owner {
                    state
                        .response_request_owners
                        .retain(|_, owner| owner != idempotency_key);
                }
                return Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    format!("failed to persist pending response: {error}"),
                ));
            }
        }
        connection_id = Some(resolved_connection_id.clone());
        match &message {
            ToAgentMessage::Hello {
                protocol_version,
                client_info,
                capabilities,
                role,
                opt_out_notifications,
                controller_binding,
            } => {
                if !crate::headless::messages::client_protocol_version_is_supported(
                    protocol_version.as_deref(),
                ) {
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::UnsupportedCapability,
                        crate::headless::messages::unsupported_client_protocol_version_message(
                            protocol_version.as_deref().unwrap_or_default(),
                        ),
                    ));
                }
                let resolved_role = state
                    .connections
                    .get(&resolved_connection_id)
                    .map(|connection| role.unwrap_or(connection.role))
                    .unwrap_or(ConnectionRole::Controller);
                if resolved_role == ConnectionRole::Viewer
                    && state.controller_connection_id.as_deref()
                        == Some(resolved_connection_id.as_str())
                {
                    revoke_controller_streams(&mut state);
                    state.controller_connection_id = None;
                }
                for subscription in state
                    .subscriptions
                    .values_mut()
                    .filter(|subscription| subscription.connection_id == resolved_connection_id)
                {
                    subscription.role = resolved_role;
                }
                let lease_expires_at =
                    state
                        .connections
                        .get_mut(&resolved_connection_id)
                        .map(|connection| {
                            connection.client_protocol_version = protocol_version.clone();
                            connection.client_info = client_info.clone();
                            connection.capabilities = capabilities.clone();
                            connection.opt_out_notifications =
                                opt_out_notifications.clone().unwrap_or_default();
                            connection.role = resolved_role;
                            connection.last_seen_at = Utc::now();
                            lease_expires_at(connection)
                        });
                let controller_connection_id = state.controller_connection_id.clone();
                let accepted_binding = controller_binding_from_hello_extension(
                    controller_binding.as_ref(),
                    HEADLESS_PROTOCOL_VERSION,
                    &ControllerScopeExpectation {
                        organization_id: shared
                            .config
                            .workload_identity
                            .as_ref()
                            .map(|identity| identity.organization_id.clone()),
                        workspace_id: Some(startup_workspace_id(&shared.config)),
                        thread_id: Some(state.session_id.clone()),
                        channel_id: None,
                        request_id: None,
                    },
                )
                .map_err(|error| {
                    HostedError::new(
                        HostedRunnerErrorCode::BadRequest,
                        format!("invalid controller binding: {error}"),
                    )
                })?;
                if let Some(binding) = accepted_binding {
                    match state.controller_binding.as_ref() {
                        Some(existing) if existing.binding_sha256 != binding.binding_sha256 => {
                            return Err(HostedError::new(
                                HostedRunnerErrorCode::LeaseConflict,
                                "controller binding cannot be replaced",
                            ));
                        }
                        Some(_) => {}
                        None => state.controller_binding = Some(binding),
                    }
                }
                let controller_binding_version = state
                    .controller_binding
                    .as_ref()
                    .map(|binding| binding.binding_version.clone());
                let controller_binding_sha256 = state
                    .controller_binding
                    .as_ref()
                    .map(|binding| binding.binding_sha256.clone());
                let server_capabilities =
                    server_capabilities_for_executor(shared.message_executor.as_ref());
                shared.publish_message(
                    &mut state,
                    FromAgentMessage::HelloOk {
                        protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
                        controller_binding_version,
                        controller_binding_sha256,
                        connection_id: Some(resolved_connection_id.clone()),
                        client_protocol_version: protocol_version.clone(),
                        client_info: client_info.clone(),
                        capabilities: capabilities.clone(),
                        server_capabilities,
                        opt_out_notifications: opt_out_notifications.clone(),
                        role: Some(resolved_role),
                        controller_connection_id,
                        lease_expires_at,
                    },
                );
                if controller_binding.is_some() {
                    executor_request = Some((
                        Arc::clone(&shared.message_executor),
                        message_context(
                            &state,
                            &resolved_connection_id,
                            subscription_id.clone(),
                            &shared.config.workspace_root,
                            response_idempotency_key.clone(),
                        )?,
                    ));
                }
            }
            ToAgentMessage::Init {
                system_prompt: _,
                append_system_prompt: _,
                thinking_level: _,
                approval_mode: _,
                history: _,
            } => {
                executor_request = Some((
                    Arc::clone(&shared.message_executor),
                    message_context(
                        &state,
                        &resolved_connection_id,
                        subscription_id.clone(),
                        &shared.config.workspace_root,
                        response_idempotency_key.clone(),
                    )?,
                ));
            }
            ToAgentMessage::GovernedInit {
                system_prompt: _,
                append_system_prompt: _,
                thinking_level: _,
                approval_mode: _,
                history: _,
                code_mode: _,
                tool_grant: _,
            } => {
                executor_request = Some((
                    Arc::clone(&shared.message_executor),
                    message_context(
                        &state,
                        &resolved_connection_id,
                        subscription_id.clone(),
                        &shared.config.workspace_root,
                        response_idempotency_key.clone(),
                    )?,
                ));
            }
            ToAgentMessage::ApplyWorkspaceCapabilitySet { .. }
            | ToAgentMessage::ConfigurePromptExperiment { .. } => {
                executor_request = Some((
                    Arc::clone(&shared.message_executor),
                    message_context(
                        &state,
                        &resolved_connection_id,
                        subscription_id.clone(),
                        &shared.config.workspace_root,
                        response_idempotency_key.clone(),
                    )?,
                ));
            }
            ToAgentMessage::Prompt { .. }
            | ToAgentMessage::Steer { .. }
            | ToAgentMessage::GovernedPrompt { .. }
            | ToAgentMessage::GovernedSteer { .. } => {
                state.last_status = Some("Prompt accepted".to_string());
                executor_request = Some((
                    Arc::clone(&shared.message_executor),
                    message_context(
                        &state,
                        &resolved_connection_id,
                        subscription_id.clone(),
                        &shared.config.workspace_root,
                        response_idempotency_key.clone(),
                    )?,
                ));
            }
            ToAgentMessage::UtilityCommandTerminate { command_id, .. } => {
                state.active_utility_commands.remove(command_id);
                shared.publish_message(
                    &mut state,
                    FromAgentMessage::UtilityCommandExited {
                        command_id: command_id.clone(),
                        success: false,
                        exit_code: None,
                        signal: None,
                        reason: Some("terminated".to_string()),
                    },
                );
            }
            ToAgentMessage::UtilityCommandStdin { .. }
            | ToAgentMessage::UtilityCommandResize { .. }
            | ToAgentMessage::RestoreConversation { .. }
            | ToAgentMessage::ToolResponse { .. }
            | ToAgentMessage::ClientToolResult { .. }
            | ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
                ..
            })
            | ToAgentMessage::ManagedAuthorizationResult { .. }
            | ToAgentMessage::ServerRequestResponse { .. }
            | ToAgentMessage::Interrupt
            | ToAgentMessage::Cancel
            | ToAgentMessage::Shutdown => {
                executor_request = Some((
                    Arc::clone(&shared.message_executor),
                    message_context(
                        &state,
                        &resolved_connection_id,
                        subscription_id.clone(),
                        &shared.config.workspace_root,
                        response_idempotency_key.clone(),
                    )?,
                ));
            }
            ToAgentMessage::UtilityCommandStart { .. }
            | ToAgentMessage::UtilityFileSearch { .. }
            | ToAgentMessage::UtilityFileRead { .. }
            | ToAgentMessage::UtilityFileWatchStart { .. }
            | ToAgentMessage::UtilityFileWatchStop { .. } => {}
        }
    }

    if let Some((executor, context)) = executor_request {
        let result = match if reconcile_pending_response {
            executor
                .reconcile_pending_async(&context, message.clone())
                .await
        } else {
            executor.execute_async(&context, message.clone()).await
        } {
            Ok(result) => result,
            Err(error) => {
                if let Some(idempotency_key) = response_idempotency_key.as_ref() {
                    let mut state = shared
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.pending_response_idempotency.remove(idempotency_key);
                    state
                        .response_request_owners
                        .retain(|_, owner| owner != idempotency_key);
                    state
                        .pending_response_idempotency_order
                        .retain(|key| key != idempotency_key);
                    shared.persist_thread_best_effort(&state, "response_execution_failed_cleanup");
                }
                return Err(HostedError::from(error));
            }
        };
        published_messages = result.messages.len();
        execution = result.execution;
        response_message = result.message;
        finalize_response_idempotency = result.idempotency_finalized;

        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure_session_id(&shared.binding, Some(session_id))?;
        let resolved_connection_id = resolve_authorized_connection_id(
            &state,
            Some(context.connection_id.clone()),
            context.subscription_id.clone(),
            connection_capability.as_deref(),
        )?;
        assert_controller(&state, Some(resolved_connection_id.as_str()))?;
        if !state.ready || state.draining {
            return Err(runtime_availability_error(
                &state,
                "hosted runner is draining",
            ));
        }
        shared.prune_pending_controller_events(&mut state);
        for message in result.messages {
            shared.publish_message(&mut state, message);
        }
        if matches!(
            &message,
            ToAgentMessage::Init { .. } | ToAgentMessage::GovernedInit { .. }
        ) && !state.ready
        {
            return Err(runtime_availability_error(
                &state,
                "native runtime rejected initialization",
            ));
        }
        match &message {
            ToAgentMessage::Init {
                system_prompt,
                append_system_prompt,
                thinking_level,
                approval_mode,
                history,
            } => {
                let previous_last_init = state.last_init.replace(InitConfig {
                    system_prompt: system_prompt.clone(),
                    append_system_prompt: append_system_prompt.clone(),
                    thinking_level: *thinking_level,
                    approval_mode: *approval_mode,
                    history: history.clone(),
                    code_mode: None,
                    tool_grant: None,
                });
                let previous_last_status = state.last_status.replace("Initialized".to_string());
                if let Err(error) = shared.persist_thread_for_request(&state) {
                    state.last_init = previous_last_init;
                    state.last_status = previous_last_status;
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::RuntimeFailed,
                        format!("failed to persist applied init: {error}"),
                    ));
                }
            }
            ToAgentMessage::GovernedInit {
                system_prompt,
                append_system_prompt,
                thinking_level,
                approval_mode,
                history,
                code_mode,
                tool_grant,
            } => {
                let previous_last_init = state.last_init.replace(InitConfig {
                    system_prompt: system_prompt.clone(),
                    append_system_prompt: append_system_prompt.clone(),
                    thinking_level: *thinking_level,
                    approval_mode: *approval_mode,
                    history: history.clone(),
                    code_mode: Some(*code_mode),
                    tool_grant: Some(tool_grant.clone()),
                });
                let previous_last_status = state
                    .last_status
                    .replace("Governed code initialized".to_string());
                if let Err(error) = shared.persist_thread_for_request(&state) {
                    state.last_init = previous_last_init;
                    state.last_status = previous_last_status;
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::RuntimeFailed,
                        format!("failed to persist applied governed init: {error}"),
                    ));
                }
            }
            _ => {}
        }
    }

    match message {
        ToAgentMessage::UtilityCommandStart {
            command_id,
            command,
            cwd,
            env,
            shell_mode,
            terminal_mode,
            columns,
            rows,
            ..
        } => {
            let utility_command_task = run_utility_command(
                shared.clone(),
                UtilityCommandInvocation {
                    connection_id: connection_id.clone(),
                    command_id,
                    command,
                    cwd,
                    env: env.unwrap_or_default(),
                    shell_mode: shell_mode.unwrap_or(UtilityCommandShellMode::Shell),
                    terminal_mode: terminal_mode.unwrap_or(UtilityCommandTerminalMode::Pipe),
                    columns,
                    rows,
                },
            )
            .await?;
            drop(utility_command_task);
        }
        ToAgentMessage::UtilityFileRead {
            read_id,
            path,
            cwd,
            offset,
            limit,
        } => handle_file_read(shared.clone(), read_id, path, cwd, offset, limit).await?,
        ToAgentMessage::UtilityFileSearch {
            search_id,
            query,
            cwd,
            limit,
        } => handle_file_search(shared.clone(), search_id, query, cwd, limit).await?,
        ToAgentMessage::UtilityFileWatchStart {
            watch_id,
            root_dir,
            include_patterns,
            exclude_patterns,
            debounce_ms,
        } => handle_file_watch_start(
            shared.clone(),
            connection_id.clone(),
            watch_id,
            root_dir,
            include_patterns,
            exclude_patterns,
            debounce_ms.unwrap_or(250),
        )?,
        ToAgentMessage::UtilityFileWatchStop { watch_id } => {
            handle_file_watch_stop(shared.clone(), watch_id)?;
        }
        _ => {}
    }

    // Reconcile the durable pending record only after the executor accepted
    // the response and every synchronous delivery side effect succeeded. A
    // retry after a restart can therefore recover the original payload instead
    // of being mistaken for a completed response.
    if let Some(idempotency_key) = response_idempotency_key
        .as_ref()
        .filter(|_| finalize_response_idempotency)
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_response_idempotency.remove(idempotency_key);
        state
            .pending_response_idempotency_order
            .retain(|key| key != idempotency_key);
        if !state.response_idempotency_keys.contains(idempotency_key) {
            if state.response_idempotency_order.len() >= MAX_RESPONSE_IDEMPOTENCY_RECORDS {
                if let Some(evicted_key) = state.response_idempotency_order.pop_front() {
                    state.response_idempotency_keys.remove(&evicted_key);
                    state.response_idempotency_digests.remove(&evicted_key);
                    state
                        .response_request_owners
                        .retain(|_, owner| owner != &evicted_key);
                }
            }
            state
                .response_idempotency_order
                .push_back(idempotency_key.clone());
        }
        state
            .response_idempotency_keys
            .insert(idempotency_key.clone());
        if let Some(digest) = response_idempotency_digest.as_ref() {
            state
                .response_idempotency_digests
                .insert(idempotency_key.clone(), digest.clone());
        }
        let mut idempotency_persisted = true;
        if let Err(error) = shared.persist_thread_for_request(&state) {
            // The executor has already accepted and delivered this response.
            // Keep the in-memory completion marker and return success so a
            // transient journal failure cannot make the native transport retry
            // an already-executed approval, input, tool result, or retry.
            idempotency_persisted = false;
            shared
                .thread_persistence_retry_pending
                .store(true, Ordering::Release);
            // Finalization may be the last activity on this connection. Retry
            // the retained completion even if no more runtime events arrive.
            shared.schedule_thread_persistence_recovery();
            eprintln!("failed to persist response idempotency key: {error}");
        }
        if !idempotency_persisted {
            response_message.push_str("; response idempotency is currently memory-only");
        }
    } else if response_idempotency_key.is_some() {
        response_message.push_str("; response remains pending native consumption");
    }

    let snapshot = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.public_snapshot(&state)
    };
    let cursor = snapshot.cursor;
    json_response(
        200,
        json!({
            "ok": true,
            "success": true,
            "accepted": true,
            "replayed": false,
            "cursor": cursor,
            "execution": execution,
            "published_messages": published_messages,
            "message": response_message,
            "snapshot": snapshot,
        }),
    )
}

fn response_message_digest(message: &ToAgentMessage) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(message).expect("headless messages are serializable"))
    )
}

fn upsert_pending_response_idempotency(
    state: &mut RunnerState,
    idempotency_key: String,
    message: ToAgentMessage,
) -> HostedResult<()> {
    let live_pending_keys = state
        .pending_response_idempotency
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let mut seen_pending_keys = HashSet::new();
    state
        .pending_response_idempotency_order
        .retain(|key| live_pending_keys.contains(key) && seen_pending_keys.insert(key.clone()));
    let ordered_pending_keys = state
        .pending_response_idempotency_order
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let mut unordered_pending_keys = live_pending_keys
        .difference(&ordered_pending_keys)
        .cloned()
        .collect::<Vec<_>>();
    unordered_pending_keys.sort();
    state
        .pending_response_idempotency_order
        .extend(unordered_pending_keys);

    let is_new_pending_key = !state
        .pending_response_idempotency
        .contains_key(&idempotency_key);
    if is_new_pending_key
        && state.pending_response_idempotency.len() >= MAX_RESPONSE_IDEMPOTENCY_RECORDS
    {
        return Err(HostedError::new(
            HostedRunnerErrorCode::ResponseCapacity,
            "pending response capacity is full; retry after a native response is consumed",
        ));
    }
    state
        .pending_response_idempotency
        .insert(idempotency_key.clone(), message);
    if is_new_pending_key {
        state
            .pending_response_idempotency_order
            .push_back(idempotency_key);
    }
    Ok(())
}

fn is_control_response_message(message: &ToAgentMessage) -> bool {
    matches!(
        message,
        ToAgentMessage::ToolResponse { .. }
            | ToAgentMessage::ClientToolResult { .. }
            | ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult { .. })
            | ToAgentMessage::ManagedAuthorizationResult { .. }
            | ToAgentMessage::ServerRequestResponse { .. }
    )
}

fn handle_heartbeat(
    shared: SharedRunner,
    session_id: &str,
    input: HeartbeatRequest,
) -> HostedResult<ResponseBody> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_session_id(&shared.binding, Some(session_id))?;
    let connection_id = resolve_authorized_connection_id(
        &state,
        input.connection_id,
        input.subscription_id,
        input.connection_capability.as_deref(),
    )?;
    let controller_lease_granted =
        state.controller_connection_id.as_deref() == Some(connection_id.as_str());
    let controller_connection_id = state.controller_connection_id.clone();
    let connection = state.connections.get_mut(&connection_id).ok_or_else(|| {
        HostedError::new(
            HostedRunnerErrorCode::StaleConnection,
            "Headless connection not found",
        )
    })?;
    connection.last_seen_at = Utc::now();
    let lease_expires_at = lease_expires_at(connection);
    json_response(
        200,
        json!({
            "connection_id": connection_id,
            "controller_lease_granted": controller_lease_granted,
            "controller_connection_id": controller_connection_id,
            "lease_expires_at": lease_expires_at,
            "heartbeat_interval_ms": DEFAULT_HEARTBEAT_INTERVAL_MS,
        }),
    )
}

fn handle_disconnect(
    shared: SharedRunner,
    session_id: &str,
    input: DisconnectRequest,
) -> HostedResult<ResponseBody> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_session_id(&shared.binding, Some(session_id))?;
    let connection_id = resolve_disconnect_connection_id(
        &state,
        input.connection_id,
        input.subscription_id,
        input.connection_capability.as_deref(),
    )?;
    let mut disconnected_subscription_ids = Vec::new();
    if let Some(connection) = state.connections.remove(&connection_id) {
        for subscription_id in connection.subscription_ids {
            state.subscriptions.remove(&subscription_id);
            disconnected_subscription_ids.push(subscription_id);
        }
    }
    if state.controller_connection_id.as_deref() == Some(connection_id.as_str()) {
        revoke_controller_streams(&mut state);
        state.controller_connection_id = None;
    }
    let _ = shared.publish_snapshot(&mut state);
    json_response(
        200,
        json!({
            "success": true,
            "connection_id": connection_id,
            "controller_connection_id": state.controller_connection_id,
            "disconnected_subscription_ids": disconnected_subscription_ids,
        }),
    )
}

fn upsert_connection(
    state: &mut RunnerState,
    input: ConnectionUpsert,
) -> HostedResult<Option<String>> {
    let ConnectionUpsert {
        connection_id,
        connection_capability,
        connection_capability_required,
        role,
        client_protocol_version,
        client_info,
        capabilities,
        opt_out_notifications,
        take_control,
    } = input;
    if let Some(existing) = state.connections.get(&connection_id) {
        match existing.authority_mode {
            ConnectionAuthorityMode::Capability => {
                let authorized = connection_capability_required
                    && connection_capability.as_deref().is_some_and(|provided| {
                        existing
                            .connection_capability
                            .as_deref()
                            .is_some_and(|expected| {
                                constant_time_equal(provided.as_bytes(), expected.as_bytes())
                            })
                    });
                if !authorized {
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::AccessDenied,
                        "existing headless connection requires its private capability",
                    ));
                }
            }
            ConnectionAuthorityMode::LegacySubscription => {
                let preserves_legacy_authority = !connection_capability_required
                    && connection_capability.is_none()
                    && role == existing.role
                    && !take_control;
                if !preserves_legacy_authority {
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::AccessDenied,
                        "legacy headless connection authority cannot be changed in place",
                    ));
                }
            }
        }
    }
    let was_controller = state.controller_connection_id.as_deref() == Some(connection_id.as_str());
    if role == ConnectionRole::Controller {
        if let Some(controller_connection_id) = state.controller_connection_id.as_ref() {
            if controller_connection_id != &connection_id && !take_control {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::RuntimeOwnedElsewhere,
                    "Controller lease is already held by another connection",
                ));
            }
        }
        if state.controller_connection_id.as_deref() != Some(connection_id.as_str()) {
            revoke_controller_streams(state);
        }
        state.controller_connection_id = Some(connection_id.clone());
    } else if was_controller {
        revoke_controller_streams(state);
        state.controller_connection_id = None;
    }
    let now = Utc::now();
    let existing = state.connections.remove(&connection_id);
    let authority_mode = existing
        .as_ref()
        .map(|connection| connection.authority_mode)
        .unwrap_or(if connection_capability_required {
            ConnectionAuthorityMode::Capability
        } else {
            ConnectionAuthorityMode::LegacySubscription
        });
    let connection_capability = match authority_mode {
        ConnectionAuthorityMode::LegacySubscription => None,
        ConnectionAuthorityMode::Capability => existing
            .as_ref()
            .and_then(|connection| connection.connection_capability.clone())
            .or_else(|| {
                connection_capability.filter(|capability| {
                    capability
                        .strip_prefix("cap_")
                        .is_some_and(|value| Uuid::parse_str(value).is_ok())
                })
            })
            .or_else(|| Some(format!("cap_{}", Uuid::new_v4().simple()))),
    };
    let subscription_ids = existing
        .as_ref()
        .map(|connection| connection.subscription_ids.clone())
        .unwrap_or_default();
    let client_protocol_version = client_protocol_version.or_else(|| {
        existing
            .as_ref()
            .and_then(|connection| connection.client_protocol_version.clone())
    });
    let client_info = client_info.or_else(|| {
        existing
            .as_ref()
            .and_then(|connection| connection.client_info.clone())
    });
    let capabilities = capabilities.or_else(|| {
        existing
            .as_ref()
            .and_then(|connection| connection.capabilities.clone())
    });
    let opt_out_notifications = if opt_out_notifications.is_empty() {
        existing
            .as_ref()
            .map(|connection| connection.opt_out_notifications.clone())
            .unwrap_or_default()
    } else {
        opt_out_notifications
    };
    state.connections.insert(
        connection_id.clone(),
        ConnectionRecord {
            id: connection_id,
            connection_capability: connection_capability.clone(),
            authority_mode,
            role,
            client_protocol_version,
            client_info,
            capabilities,
            opt_out_notifications,
            subscription_ids,
            last_seen_at: now,
        },
    );
    Ok(connection_capability)
}

fn revoke_controller_streams(state: &mut RunnerState) {
    state.controller_stream_cancellation.cancel();
    state.controller_stream_cancellation = CancellationToken::new();
}

fn lease_expires_at(connection: &ConnectionRecord) -> String {
    (connection.last_seen_at + ChronoDuration::milliseconds(CONNECTION_IDLE_MS))
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

async fn run_utility_command(
    shared: SharedRunner,
    invocation: UtilityCommandInvocation,
) -> HostedResult<tokio::task::JoinHandle<()>> {
    let UtilityCommandInvocation {
        connection_id,
        command_id,
        command,
        cwd,
        env,
        shell_mode,
        terminal_mode,
        columns,
        rows,
    } = invocation;
    let cwd_path = resolve_workspace_path(&shared.config.workspace_root, None, cwd.as_deref())?;
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = ActiveUtilityCommandSnapshot {
            command_id: command_id.clone(),
            command: command.clone(),
            cwd: Some(cwd_path.to_string_lossy().to_string()),
            shell_mode,
            terminal_mode,
            pid: None,
            columns,
            rows,
            owner_connection_id: connection_id.clone(),
            output: String::new(),
        };
        state
            .active_utility_commands
            .insert(command_id.clone(), snapshot);
        shared.publish_message(
            &mut state,
            FromAgentMessage::UtilityCommandStarted {
                command_id: command_id.clone(),
                command: command.clone(),
                cwd: Some(cwd_path.to_string_lossy().to_string()),
                shell_mode,
                terminal_mode,
                pid: None,
                columns,
                rows,
                owner_connection_id: connection_id.clone(),
            },
        );
    }

    let task = tokio::spawn(async move {
        let output = spawn_command(&command, &cwd_path, env, shell_mode).await;
        let _lifecycle = shared.mutation_lifecycle.lock().await;
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.draining || !state.ready {
            return;
        }
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let success = output.status.success();
                let exit_code = output.status.code();
                if let Some(active) = state.active_utility_commands.get_mut(&command_id) {
                    active.output.push_str(&stdout);
                    active.output.push_str(&stderr);
                }
                if !stdout.is_empty() {
                    shared.publish_message(
                        &mut state,
                        FromAgentMessage::UtilityCommandOutput {
                            command_id: command_id.clone(),
                            stream: UtilityCommandStream::Stdout,
                            content: stdout,
                        },
                    );
                }
                if !stderr.is_empty() {
                    shared.publish_message(
                        &mut state,
                        FromAgentMessage::UtilityCommandOutput {
                            command_id: command_id.clone(),
                            stream: UtilityCommandStream::Stderr,
                            content: stderr,
                        },
                    );
                }
                state.active_utility_commands.remove(&command_id);
                shared.publish_message(
                    &mut state,
                    FromAgentMessage::UtilityCommandExited {
                        command_id,
                        success,
                        exit_code,
                        signal: None,
                        reason: None,
                    },
                );
            }
            Err(error) => {
                state.active_utility_commands.remove(&command_id);
                shared.publish_message(
                    &mut state,
                    FromAgentMessage::UtilityCommandExited {
                        command_id,
                        success: false,
                        exit_code: None,
                        signal: None,
                        reason: Some(error.message),
                    },
                );
            }
        }
    });
    Ok(task)
}

async fn spawn_command(
    command: &str,
    cwd: &Path,
    env: HashMap<String, String>,
    shell_mode: UtilityCommandShellMode,
) -> HostedResult<std::process::Output> {
    let mut child = if shell_mode == UtilityCommandShellMode::Direct {
        let Some(parts) = shlex::split(command) else {
            return Err(HostedError::new(
                HostedRunnerErrorCode::UnsupportedCapability,
                "could not parse direct command",
            ));
        };
        let mut iter = parts.into_iter();
        let Some(program) = iter.next() else {
            return Err(HostedError::new(
                HostedRunnerErrorCode::BadRequest,
                "command is empty",
            ));
        };
        let mut child = Command::new(program);
        child.args(iter);
        child
    } else {
        let mut child = Command::new("sh");
        child.arg("-lc").arg(command);
        child
    };
    child.current_dir(cwd).envs(env);
    child
        .output()
        .await
        .map_err(|error| HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.to_string()))
}

async fn handle_file_read(
    shared: SharedRunner,
    read_id: String,
    path: String,
    cwd: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
) -> HostedResult<()> {
    let full_path =
        resolve_workspace_path(&shared.config.workspace_root, cwd.as_deref(), Some(&path))?;
    let content = tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|error| HostedError::new(HostedRunnerErrorCode::NotFound, error.to_string()))?;
    let lines: Vec<&str> = content.lines().collect();
    let requested_offset = offset.unwrap_or(1).max(1) as usize;
    let start = if lines.is_empty() {
        0
    } else {
        requested_offset - 1
    };
    let limit = limit.unwrap_or(200) as usize;
    let selected = lines
        .iter()
        .skip(start)
        .take(limit)
        .copied()
        .collect::<Vec<_>>();
    let rendered = selected.join("\n");
    let relative_path = relative_workspace_path(&shared.config.workspace_root, &full_path);
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shared.publish_message(
        &mut state,
        FromAgentMessage::UtilityFileReadResult {
            read_id,
            path: full_path.to_string_lossy().to_string(),
            relative_path,
            cwd: shared.config.workspace_root.to_string_lossy().to_string(),
            content: rendered,
            start_line: if lines.is_empty() {
                0
            } else {
                start as u32 + 1
            },
            end_line: (start + selected.len()) as u32,
            total_lines: lines.len() as u32,
            truncated: start + selected.len() < lines.len(),
        },
    );
    Ok(())
}

async fn handle_file_search(
    shared: SharedRunner,
    search_id: String,
    query: String,
    cwd: Option<String>,
    limit: Option<u32>,
) -> HostedResult<()> {
    let root = resolve_workspace_path(&shared.config.workspace_root, cwd.as_deref(), Some("."))?;
    let results = search_workspace_files(
        &shared.config.workspace_root,
        &root,
        &query,
        limit.unwrap_or(50) as usize,
    );
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shared.publish_message(
        &mut state,
        FromAgentMessage::UtilityFileSearchResults {
            search_id,
            query,
            cwd: root.to_string_lossy().to_string(),
            results,
            truncated: false,
        },
    );
    Ok(())
}

fn handle_file_watch_start(
    shared: SharedRunner,
    connection_id: Option<String>,
    watch_id: String,
    root_dir: Option<String>,
    include_patterns: Option<Vec<String>>,
    exclude_patterns: Option<Vec<String>>,
    debounce_ms: u32,
) -> HostedResult<()> {
    let root = resolve_workspace_path(
        &shared.config.workspace_root,
        None,
        root_dir.as_deref().or(Some(".")),
    )?;
    let root_dir = root.to_string_lossy().to_string();
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.active_file_watches.insert(
        watch_id.clone(),
        ActiveFileWatchSnapshot {
            watch_id: watch_id.clone(),
            root_dir: root_dir.clone(),
            include_patterns: include_patterns.clone(),
            exclude_patterns: exclude_patterns.clone(),
            debounce_ms,
            owner_connection_id: connection_id.clone(),
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::UtilityFileWatchStarted {
            watch_id,
            root_dir,
            include_patterns,
            exclude_patterns,
            debounce_ms,
            owner_connection_id: connection_id,
        },
    );
    Ok(())
}

fn handle_file_watch_stop(shared: SharedRunner, watch_id: String) -> HostedResult<()> {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.active_file_watches.remove(&watch_id);
    shared.publish_message(
        &mut state,
        FromAgentMessage::UtilityFileWatchStopped {
            watch_id,
            reason: Some("Stopped by controller".to_string()),
        },
    );
    Ok(())
}

fn search_workspace_files(
    workspace_root: &Path,
    root: &Path,
    query: &str,
    limit: usize,
) -> Vec<UtilityFileSearchMatch> {
    let mut results = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = relative_workspace_path(workspace_root, &path);
            if query.is_empty() || relative.contains(query) {
                results.push(UtilityFileSearchMatch {
                    path: relative,
                    score: 100,
                });
                if results.len() >= limit {
                    return results;
                }
            }
        }
    }
    results
}

fn ensure_connection_session_id(
    shared: &SharedRunner,
    state: &mut RunnerState,
    requested: Option<&str>,
) -> HostedResult<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if shared.binding.matches(requested) {
        return Ok(());
    }
    let failure = shared.record_identity_failure(state, "headless_connection", requested)?;
    Err(HostedError::new(
        HostedRunnerErrorCode::StaleSession,
        "Headless session binding does not belong to this hosted runner",
    )
    .with_details(failure.details()))
}

fn ensure_thread_id(binding: &HostedRunnerBinding, requested: Option<&str>) -> HostedResult<()> {
    if requested.is_some_and(|thread_id| thread_id != binding.runner_session_id.as_str()) {
        return Err(HostedError::new(
            HostedRunnerErrorCode::StaleSession,
            "Headless thread not found",
        ));
    }
    Ok(())
}

fn ensure_session_id(binding: &HostedRunnerBinding, requested: Option<&str>) -> HostedResult<()> {
    if requested.is_some_and(|session_id| !binding.matches(session_id)) {
        return Err(HostedError::new(
            HostedRunnerErrorCode::StaleSession,
            "Headless session not found",
        ));
    }
    Ok(())
}

fn assert_controller(state: &RunnerState, connection_id: Option<&str>) -> HostedResult<()> {
    let Some(connection_id) = connection_id else {
        return Err(HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "missing headless connection id",
        ));
    };
    let Some(connection) = state.connections.get(connection_id) else {
        return Err(HostedError::new(
            HostedRunnerErrorCode::StaleConnection,
            "Headless connection not found",
        ));
    };
    if connection.role == ConnectionRole::Viewer {
        return Err(HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "Viewer headless connections cannot send messages",
        ));
    }
    if state.controller_connection_id.as_deref() != Some(connection_id) {
        return Err(HostedError::new(
            HostedRunnerErrorCode::RuntimeOwnedElsewhere,
            "Controller lease is currently held by another connection",
        ));
    }
    Ok(())
}

fn resolve_connection_id(
    state: &RunnerState,
    connection_id: Option<String>,
    subscription_id: Option<String>,
) -> HostedResult<String> {
    if let Some(connection_id) = connection_id {
        return Ok(connection_id);
    }
    if let Some(subscription_id) = subscription_id {
        if let Some(subscription) = state.subscriptions.get(&subscription_id) {
            return Ok(subscription.connection_id.clone());
        }
    }
    Err(HostedError::new(
        HostedRunnerErrorCode::StaleConnection,
        "Headless connection not found",
    ))
}

fn resolve_authorized_connection_id(
    state: &RunnerState,
    connection_id: Option<String>,
    subscription_id: Option<String>,
    connection_capability: Option<&str>,
) -> HostedResult<String> {
    let subscription_id = subscription_id.ok_or_else(|| {
        HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "headless operation requires a private subscription",
        )
    })?;
    let resolved_connection_id =
        resolve_connection_id(state, connection_id.clone(), Some(subscription_id.clone()))?;
    let subscription = state.subscriptions.get(&subscription_id).ok_or_else(|| {
        HostedError::new(
            HostedRunnerErrorCode::StaleConnection,
            "Headless subscription not found",
        )
    })?;
    if subscription.connection_id != resolved_connection_id {
        return Err(HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "Headless subscription does not belong to the message connection",
        ));
    }
    let connection = state
        .connections
        .get(&resolved_connection_id)
        .ok_or_else(|| {
            HostedError::new(
                HostedRunnerErrorCode::StaleConnection,
                "Headless connection not found",
            )
        })?;
    let authorized = match (connection.authority_mode, subscription.authority_mode) {
        (
            ConnectionAuthorityMode::LegacySubscription,
            ConnectionAuthorityMode::LegacySubscription,
        ) => connection_capability.is_none(),
        (ConnectionAuthorityMode::Capability, ConnectionAuthorityMode::Capability) => {
            connection_capability.is_some_and(|provided| {
                connection
                    .connection_capability
                    .as_deref()
                    .is_some_and(|connection_expected| {
                        subscription.connection_capability.as_deref().is_some_and(
                            |subscription_expected| {
                                constant_time_equal(
                                    provided.as_bytes(),
                                    connection_expected.as_bytes(),
                                ) && constant_time_equal(
                                    provided.as_bytes(),
                                    subscription_expected.as_bytes(),
                                )
                            },
                        )
                    })
            })
        }
        _ => false,
    };
    if !authorized {
        return Err(HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "missing or invalid headless connection capability",
        ));
    }
    Ok(resolved_connection_id)
}

fn resolve_disconnect_connection_id(
    state: &RunnerState,
    connection_id: Option<String>,
    subscription_id: Option<String>,
    connection_capability: Option<&str>,
) -> HostedResult<String> {
    if subscription_id.is_some() {
        return resolve_authorized_connection_id(
            state,
            connection_id,
            subscription_id,
            connection_capability,
        );
    }
    let connection_id = connection_id.ok_or_else(|| {
        HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "headless disconnect requires a private connection capability",
        )
    })?;
    let connection = state.connections.get(&connection_id).ok_or_else(|| {
        HostedError::new(
            HostedRunnerErrorCode::StaleConnection,
            "Headless connection not found",
        )
    })?;
    let authorized = connection.authority_mode == ConnectionAuthorityMode::Capability
        && connection_capability.is_some_and(|provided| {
            connection
                .connection_capability
                .as_deref()
                .is_some_and(|expected| {
                    constant_time_equal(provided.as_bytes(), expected.as_bytes())
                })
        });
    if !authorized {
        return Err(HostedError::new(
            HostedRunnerErrorCode::AccessDenied,
            "missing or invalid headless connection capability",
        ));
    }
    Ok(connection_id)
}

fn message_context(
    state: &RunnerState,
    connection_id: &str,
    subscription_id: Option<String>,
    workspace_root: &Path,
    response_idempotency_key: Option<String>,
) -> HostedResult<HostedRunnerHeadlessMessageContext> {
    let connection = state.connections.get(connection_id).ok_or_else(|| {
        HostedError::new(
            HostedRunnerErrorCode::StaleConnection,
            "Headless connection not found",
        )
    })?;
    Ok(HostedRunnerHeadlessMessageContext {
        session_id: state.session_id.clone(),
        connection_id: connection.id.clone(),
        subscription_id,
        role: connection.role,
        controller_connection_id: state.controller_connection_id.clone(),
        client_protocol_version: connection.client_protocol_version.clone(),
        client_info: connection.client_info.clone(),
        capabilities: connection.capabilities.clone(),
        opt_out_notifications: (!connection.opt_out_notifications.is_empty())
            .then(|| connection.opt_out_notifications.clone()),
        lease_expires_at: lease_expires_at(connection),
        workspace_root: workspace_root.to_path_buf(),
        response_idempotency_key,
    })
}

fn connection_from_headers(
    headers: &HashMap<String, String>,
) -> (Option<String>, Option<String>, Option<String>) {
    (
        headers
            .get("x-maestro-headless-connection-id")
            .or_else(|| headers.get("x-composer-headless-connection-id"))
            .or_else(|| headers.get("x-evalops-headless-connection-id"))
            .cloned(),
        headers
            .get("x-maestro-headless-subscriber-id")
            .or_else(|| headers.get("x-maestro-headless-subscription-id"))
            .or_else(|| headers.get("x-composer-headless-subscriber-id"))
            .or_else(|| headers.get("x-composer-headless-subscription-id"))
            .or_else(|| headers.get("x-evalops-headless-subscriber-id"))
            .or_else(|| headers.get("x-evalops-headless-subscription-id"))
            .cloned(),
        headers
            .get("x-maestro-headless-connection-capability")
            .or_else(|| headers.get("x-composer-headless-connection-capability"))
            .or_else(|| headers.get("x-evalops-headless-connection-capability"))
            .cloned(),
    )
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..right.len() {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right
                    .get(index)
                    .copied()
                    .expect("index bounded by right length"),
        );
    }
    difference == 0
}

fn session_id_from_path<'a>(path: &'a str, suffix: &str) -> HostedResult<&'a str> {
    let Some(prefix_removed) = path.strip_prefix("/api/headless/sessions/") else {
        return Err(HostedError::new(
            HostedRunnerErrorCode::NotFound,
            "route not found",
        ));
    };
    let Some(session_id) = prefix_removed.strip_suffix(suffix) else {
        return Err(HostedError::new(
            HostedRunnerErrorCode::NotFound,
            "route not found",
        ));
    };
    Ok(session_id.trim_end_matches('/'))
}

fn thread_id_from_path<'a>(path: &'a str, suffix: &str) -> HostedResult<&'a str> {
    let Some(prefix_removed) = path.strip_prefix("/api/headless/threads/") else {
        return Err(HostedError::new(
            HostedRunnerErrorCode::NotFound,
            "route not found",
        ));
    };
    let Some(thread_id) = prefix_removed.strip_suffix(suffix) else {
        return Err(HostedError::new(
            HostedRunnerErrorCode::NotFound,
            "route not found",
        ));
    };
    let thread_id = thread_id.trim_end_matches('/');
    if thread_id.is_empty() || thread_id.contains('/') {
        return Err(HostedError::new(
            HostedRunnerErrorCode::NotFound,
            "route not found",
        ));
    }
    Ok(thread_id)
}

fn resolve_workspace_path(
    workspace_root: &Path,
    cwd: Option<&str>,
    requested: Option<&str>,
) -> HostedResult<PathBuf> {
    let base = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => workspace_root.join(cwd),
        _ => workspace_root.to_path_buf(),
    };
    let requested = requested.unwrap_or(".");
    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        base.join(requested)
    };
    // Compare canonical paths on both sides. Config-built roots are already
    // canonical, but tests and embedders can construct the public config with
    // a symlinked path (for example macOS's /var -> /private/var alias).
    let normalized_workspace_root = canonicalize_existing_prefix(workspace_root)?;
    let normalized = canonicalize_existing_prefix(&candidate)?;
    if !normalized.starts_with(&normalized_workspace_root) {
        return Err(HostedError::new(
            HostedRunnerErrorCode::WorkspaceViolation,
            "Path is outside hosted workspace root",
        ));
    }
    Ok(normalized)
}

fn canonicalize_existing_prefix(path: &Path) -> HostedResult<PathBuf> {
    let mut current = path.to_path_buf();
    let mut missing_components = Vec::<OsString>::new();
    loop {
        match dunce::canonicalize(&current) {
            Ok(mut canonical) => {
                for component in missing_components.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(component) = current.file_name().map(OsString::from) else {
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::NotFound,
                        "requested workspace path does not exist",
                    ));
                };
                missing_components.push(component);
                if !current.pop() {
                    return Err(HostedError::new(
                        HostedRunnerErrorCode::NotFound,
                        "requested workspace path does not exist",
                    ));
                }
            }
            Err(error) => {
                return Err(HostedError::new(
                    HostedRunnerErrorCode::NotFound,
                    error.to_string(),
                ));
            }
        }
    }
}

fn relative_workspace_path(workspace_root: &Path, path: &Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string()
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> HostedResult<T> {
    if body.is_empty() {
        return serde_json::from_slice(b"{}").map_err(|error| {
            HostedError::new(HostedRunnerErrorCode::BadRequest, error.to_string())
        });
    }
    serde_json::from_slice(body)
        .map_err(|error| HostedError::new(HostedRunnerErrorCode::BadRequest, error.to_string()))
}

fn json_response<T: Serialize>(status: u16, body: T) -> HostedResult<ResponseBody> {
    let body = serde_json::to_value(body).map_err(|error| {
        HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.to_string())
    })?;
    Ok(ResponseBody::Json { status, body })
}

async fn read_request<S>(socket: &mut S) -> io::Result<Option<HttpRequest>>
where
    S: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let mut header_end = None;
    loop {
        let mut chunk = [0_u8; 1024];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            header_end = Some(position);
            break;
        }
        if buffer.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    }
    let Some(header_end) = header_end else {
        return Ok(None);
    };
    let headers_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = headers_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("").to_string();
    let target = request_parts.next().unwrap_or("/");
    let (path, query) = parse_target(target);
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0_u8; content_length - body.len()];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(key.to_string(), value.to_string());
    }
    (path.to_string(), query)
}

async fn write_json_value<S>(socket: &mut S, status: u16, body: serde_json::Value) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_response(socket, status, "application/json", &bytes).await
}

async fn write_error<S>(socket: &mut S, error: HostedError) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let code = error.code.as_str();
    let mut json_body = json!({
        "error": error.message,
        "error_type": code,
        "code": code,
    });
    if let Some(details) = error.details {
        json_body["details"] = details;
    }
    let body = serde_json::to_vec(&json_body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_response(socket, error.status, "application/json", &body).await
}

async fn write_response<S>(
    socket: &mut S,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let reason = status_reason(status);
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(headers.as_bytes()).await?;
    socket.write_all(body).await?;
    // TLS may accept plaintext while ciphertext is still buffered. Drain it
    // before handle_socket drops this single-response connection.
    socket.flush().await
}

async fn write_sse_headers<S>(socket: &mut S) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n",
        )
        .await?;
    socket.flush().await
}

async fn write_sse_event<S>(socket: &mut S, envelope: &StreamEnvelope) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let payload = serde_json::to_string(envelope)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    socket.write_all(b"data: ").await?;
    socket.write_all(payload.as_bytes()).await?;
    socket.write_all(b"\n\n").await?;
    socket.flush().await
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests;
