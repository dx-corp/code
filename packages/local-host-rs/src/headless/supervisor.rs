//! Agent supervisor with reconnection and health monitoring
//!
//! Wraps the transport layer to provide:
//! - Automatic reconnection on failure
//! - Health monitoring with heartbeats
//! - Graceful degradation

use maestro_runtime_contracts::tool_wire;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::{self as std_mpsc, SyncSender};
use std::time::{Duration, Instant};

use rand::Rng as _;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
// Note: interval/timeout available for future health checking
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::async_transport::RemoteErrorKind;
use super::async_transport::{AsyncAgentTransport, AsyncTransportConfig, AsyncTransportError};
use super::messages::{
    AgentEvent, AgentState, ClientInfo, ConnectionRole, ControllerBindingHello, FromAgentMessage,
    InitConfig, ToAgentMessage,
};
use super::remote_transport::{
    RemoteAgentTransport, RemoteConnectionResumeAuthority, RemoteIncoming, RemoteTransportConfig,
};
use super::session::{SessionRecorder, SessionReplay};
use super::workspace_capabilities::{
    ApplyWorkspaceCapabilitySet, WorkspaceCapabilitySetApplied, accept_workspace_capability_receipt,
};

const MAX_STALE_REMOTE_REFERENCE_RETRIES: u32 = 3;
const MIN_RECONNECT_SLEEP: Duration = Duration::from_millis(1);
const REMOTE_COMPACTION_SILENCE_TIMEOUT: Duration = Duration::from_mins(3);
const RESPONSE_ACK_TIMEOUT: Duration = Duration::from_millis(500);
const RESPONSE_ACK_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_RESPONSE_ACKNOWLEDGEMENTS: usize = 4096;

fn workspace_capability_replay_cursor(request: &ApplyWorkspaceCapabilitySet) -> String {
    format!(
        "{}:{}",
        request.activation_generation, request.capability_set_digest
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseAcknowledgement {
    NotExpected,
    Consumed,
    Queued,
    Rejected,
}

fn jittered_reconnect_delay_for_sample(
    base_delay: Duration,
    jitter_factor: f64,
    jitter_sample: f64,
) -> Duration {
    if jitter_factor <= 0.0 || base_delay.is_zero() {
        return base_delay;
    }

    let capped_sample = jitter_sample.clamp(-1.0, 1.0);
    let jittered_secs = (base_delay.as_secs_f64()
        + base_delay.as_secs_f64() * jitter_factor * capped_sample)
        .max(MIN_RECONNECT_SLEEP.as_secs_f64());
    Duration::from_secs_f64(jittered_secs)
}

fn jittered_reconnect_delay(base_delay: Duration, jitter_factor: f64) -> Duration {
    if jitter_factor <= 0.0 || base_delay.is_zero() {
        return base_delay;
    }

    let mut rng = rand::rng();
    jittered_reconnect_delay_for_sample(base_delay, jitter_factor, rng.random_range(-1.0..=1.0))
}

/// Report a diagnostic to stderr without blocking the caller.
///
/// A plain `writeln!(std::io::stderr(), ...)` on a hot or otherwise
/// blocking-sensitive path (this supervisor's `send`, `apply_snapshot`,
/// `apply_agent_message`, `persist_session_snapshot`; also reused by
/// `tools::registry::execute` for write/edit rollback diagnostics and
/// `agent::native`'s `NativeAgentRunner` for its MCP-init diagnostic, all
/// part of the same swallowed-result audit) takes stderr's process-global
/// lock and performs a real write syscall; if a headless parent pipes
/// stderr and stops draining it, that write blocks until the pipe drains
/// or the process dies. Recorder-failure diagnostics are already
/// rate-limited (see `session_recorder_error_to_report`) to avoid log
/// spam, but even one occurrence stalling a caller's hot path (and,
/// transitively, whatever `await`s it) is a real regression versus the
/// swallowed-error status quo this audit exists to fix. A single process-wide
/// OS thread drains a bounded queue through an independently duplicated stderr
/// handle, so a blocked write never holds Rust's process-global stderr lock.
/// Enqueue is nonblocking and drops diagnostics when the queue is full, so a
/// wedged stderr can consume at most one worker thread and a fixed amount of
/// memory. The worker does not depend on a Tokio runtime being current, so it
/// works uniformly whether or not the caller is itself async.
pub(crate) fn report_diagnostic_nonblocking(message: String) {
    let _ = try_report_diagnostic_nonblocking(message);
}

fn try_report_diagnostic_nonblocking(message: String) -> bool {
    if let Some(sender) = diagnostic_sender() {
        return enqueue_diagnostic(sender, message);
    }

    false
}

fn enqueue_diagnostic(sender: &SyncSender<String>, message: String) -> bool {
    sender.try_send(message).is_ok()
}

fn prepare_diagnostic_for_stderr(message: String, is_terminal: bool) -> String {
    if is_terminal {
        crate::output_sanitize::sanitize_control_chars(&message)
            .replace('\r', "\\r")
            .replace('\n', "\\n")
    } else {
        message
    }
}

#[cfg(unix)]
fn duplicate_stderr_writer() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd as _;

    let stderr = std::io::stderr();
    stderr.as_fd().try_clone_to_owned().map(std::fs::File::from)
}

#[cfg(windows)]
fn duplicate_stderr_writer() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::AsHandle as _;

    let stderr = std::io::stderr();
    stderr
        .as_handle()
        .try_clone_to_owned()
        .map(std::fs::File::from)
}

#[cfg(not(any(unix, windows)))]
fn duplicate_stderr_writer() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "independent stderr handles are unavailable on this platform",
    ))
}

fn diagnostic_sender() -> Option<&'static SyncSender<String>> {
    static SENDER: OnceLock<Option<SyncSender<String>>> = OnceLock::new();
    SENDER
        .get_or_init(|| {
            let mut stderr = duplicate_stderr_writer().ok()?;
            let stderr_is_terminal = crate::terminal_info::is_stderr_tty();
            let (sender, receiver) = std_mpsc::sync_channel::<String>(64);
            std::thread::Builder::new()
                .name("supervisor-diagnostic".to_string())
                .spawn(move || {
                    for message in receiver {
                        let message = prepare_diagnostic_for_stderr(message, stderr_is_terminal);
                        let _ = writeln!(stderr, "{message}");
                    }
                })
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// Supervisor configuration
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Transport configuration
    pub transport: AsyncTransportConfig,
    /// Optional remote transport configuration.
    pub remote: Option<RemoteTransportConfig>,
    /// Maximum reconnection attempts (0 = infinite)
    pub max_reconnect_attempts: u32,
    /// Initial delay between reconnection attempts
    pub reconnect_delay: Duration,
    /// Maximum delay between reconnection attempts
    pub max_reconnect_delay: Duration,
    /// Backoff multiplier for reconnection delay
    pub backoff_multiplier: f64,
    /// Randomized reconnect jitter ratio (0.25 = +/-25%).
    pub reconnect_jitter_factor: f64,
    /// Maximum total wall-clock time spent reconnecting before giving up.
    pub max_reconnect_elapsed: Duration,
    /// Health check interval
    pub health_check_interval: Duration,
    /// Timeout for health check response
    pub health_check_timeout: Duration,
    /// Whether to automatically reconnect on failure
    pub auto_reconnect: bool,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            transport: AsyncTransportConfig::default(),
            remote: None,
            max_reconnect_attempts: 5,
            reconnect_delay: Duration::from_secs(1),
            max_reconnect_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            reconnect_jitter_factor: 0.25,
            max_reconnect_elapsed: Duration::from_mins(10),
            health_check_interval: Duration::from_secs(30),
            health_check_timeout: Duration::from_secs(5),
            auto_reconnect: true,
        }
    }
}

/// Current health status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Agent is healthy and responding
    Healthy,
    /// Agent might be degraded (slow responses)
    Degraded,
    /// Agent is unhealthy/disconnected
    Unhealthy,
    /// Currently reconnecting
    Reconnecting,
    /// Unknown state (not yet checked)
    Unknown,
}

/// Supervisor event
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// Agent event (pass-through)
    Agent(Box<AgentEvent>),
    /// Control protocol messages have no UI event but must reach the hosted controller.
    ControlMessage(Box<FromAgentMessage>),
    /// Native response consumer accepted a control response.
    ResponseAccepted { request_id: String },
    /// Connection established
    Connected,
    /// State was hydrated from replay or a remote snapshot.
    StateHydrated { session_id: Option<String> },
    /// Connection lost
    Disconnected { error: String },
    /// Reconnecting
    Reconnecting { attempt: u32, max_attempts: u32 },
    /// Health status changed
    HealthChanged { status: HealthStatus },
    /// Supervisor shutting down
    ShuttingDown,
}

enum ManagedTransport {
    Local(Box<AsyncAgentTransport>),
    Remote(Box<RemoteAgentTransport>),
}

enum ManagedIncoming {
    Message(Box<FromAgentMessage>),
    Snapshot {
        state: Box<AgentState>,
        last_init: Option<InitConfig>,
    },
    Reset {
        reason: String,
        state: Box<AgentState>,
        last_init: Option<InitConfig>,
    },
    Heartbeat,
}

impl ManagedTransport {
    fn send(&self, msg: ToAgentMessage) -> Result<(), AsyncTransportError> {
        match self {
            Self::Local(transport) => transport.send(msg),
            Self::Remote(transport) => transport.send(msg),
        }
    }

    fn shutdown(&self) -> Result<(), AsyncTransportError> {
        match self {
            Self::Local(transport) => transport.shutdown(),
            Self::Remote(transport) => transport.shutdown(),
        }
    }

    async fn shutdown_and_wait(self) -> Result<(), AsyncTransportError> {
        match self {
            Self::Local(transport) => (*transport).shutdown_and_wait().await,
            Self::Remote(transport) => (*transport).shutdown_and_wait().await,
        }
    }

    fn needs_init_replay(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    fn initial_snapshot(&self) -> Option<(AgentState, Option<InitConfig>)> {
        match self {
            Self::Local(_) => None,
            Self::Remote(transport) => {
                Some((transport.state().clone(), transport.last_init().cloned()))
            }
        }
    }

    fn remote_session_id(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Remote(transport) => Some(transport.session_id()),
        }
    }

    fn remote_resume_authority(&self) -> Option<RemoteConnectionResumeAuthority> {
        match self {
            Self::Local(_) => None,
            Self::Remote(transport) => Some(transport.resume_authority()),
        }
    }

    fn try_recv_incoming(&mut self) -> Option<Result<ManagedIncoming, AsyncTransportError>> {
        match self {
            Self::Local(transport) => transport
                .try_recv_message()
                .map(|result| result.map(|message| ManagedIncoming::Message(Box::new(message)))),
            Self::Remote(transport) => transport.try_recv_incoming().map(|result| {
                result.map(|incoming| match incoming {
                    RemoteIncoming::Message(message) => ManagedIncoming::Message(Box::new(message)),
                    RemoteIncoming::Snapshot { state, last_init } => {
                        ManagedIncoming::Snapshot { state, last_init }
                    }
                    RemoteIncoming::Reset {
                        reason,
                        state,
                        last_init,
                    } => ManagedIncoming::Reset {
                        reason,
                        state,
                        last_init,
                    },
                    RemoteIncoming::Heartbeat => ManagedIncoming::Heartbeat,
                })
            }),
        }
    }

    fn event_notification(&self) -> Arc<Notify> {
        match self {
            Self::Local(transport) => transport.event_notification(),
            Self::Remote(transport) => transport.event_notification(),
        }
    }

    async fn recv_incoming(&mut self) -> Result<ManagedIncoming, AsyncTransportError> {
        match self {
            Self::Local(transport) => transport
                .recv_message()
                .await
                .map(|message| ManagedIncoming::Message(Box::new(message))),
            Self::Remote(transport) => {
                transport
                    .recv_incoming()
                    .await
                    .map(|incoming| match incoming {
                        RemoteIncoming::Message(message) => {
                            ManagedIncoming::Message(Box::new(message))
                        }
                        RemoteIncoming::Snapshot { state, last_init } => {
                            ManagedIncoming::Snapshot { state, last_init }
                        }
                        RemoteIncoming::Reset {
                            reason,
                            state,
                            last_init,
                        } => ManagedIncoming::Reset {
                            reason,
                            state,
                            last_init,
                        },
                        RemoteIncoming::Heartbeat => ManagedIncoming::Heartbeat,
                    })
            }
        }
    }

    fn local_transport(&self) -> Option<&AsyncAgentTransport> {
        match self {
            Self::Local(transport) => Some(transport),
            Self::Remote(_) => None,
        }
    }

    fn local_transport_mut(&mut self) -> Option<&mut AsyncAgentTransport> {
        match self {
            Self::Local(transport) => Some(transport),
            Self::Remote(_) => None,
        }
    }
}

/// Agent supervisor
///
/// Provides a resilient wrapper around the transport with:
/// - Automatic reconnection
/// - Health monitoring
/// - Session recording
pub struct AgentSupervisor {
    /// Configuration
    config: SupervisorConfig,
    /// Current transport (if connected)
    transport: Option<ManagedTransport>,
    /// Last init config to replay after reconnects
    last_init: Option<InitConfig>,
    /// Private provider conversation checkpoint replayed immediately after init.
    semantic_conversation: Option<Vec<maestro_ai::Message>>,
    /// Last capability set proven accepted by a child receipt.
    last_workspace_capability_set: Option<ApplyWorkspaceCapabilitySet>,
    /// Sent capability sets awaiting a matching child receipt.
    pending_workspace_capability_sets: HashMap<String, ApplyWorkspaceCapabilitySet>,
    /// Current supervisor-owned agent state
    state: AgentState,
    /// Event sender
    event_tx: mpsc::UnboundedSender<SupervisorEvent>,
    /// Event receiver
    event_rx: mpsc::UnboundedReceiver<SupervisorEvent>,
    /// Current health status
    health_status: HealthStatus,
    /// Last successful response time
    last_response: Option<Instant>,
    /// Reconnection attempt counter
    reconnect_attempts: u32,
    /// Consecutive retryable stale connection/subscriber failures.
    stale_reference_retries: u32,
    /// Private authority required to reclaim a server-minted remote connection.
    remote_resume_authority: Option<RemoteConnectionResumeAuthority>,
    /// Whether a reconnect should be attempted on the next async receive cycle
    pending_auto_reconnect: bool,
    /// Background teardown for a transport that must finish before the next connect/reconnect.
    pending_transport_shutdown: Option<JoinHandle<()>>,
    /// Session recorder (optional)
    session_recorder: Option<SessionRecorder>,
    /// Whether the current run of session-recorder failures was already reported.
    session_recorder_error_reported: bool,
    /// Cancellation token
    cancel_token: CancellationToken,
    /// Correlated response acknowledgements received before their waiter.
    pending_response_acknowledgements: HashSet<String>,
    /// Correlated protocol rejections received before their waiter.
    pending_response_rejections: HashMap<String, String>,
    /// Request IDs owned by responses sent through the current transport.
    expected_response_acknowledgements: HashMap<String, u64>,
    /// Changes whenever a new child/remote transport takes ownership.
    transport_generation: u64,
}

impl AgentSupervisor {
    pub(crate) async fn wait_for_response_acknowledgement_async(
        supervisor: Arc<std::sync::Mutex<Self>>,
        request_id: String,
        timeout: Duration,
    ) -> (Vec<FromAgentMessage>, ResponseAcknowledgement) {
        let deadline = Instant::now() + timeout;
        let mut messages = Vec::new();
        loop {
            let (drained, acknowledgement) = {
                let mut supervisor = supervisor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                supervisor.wait_for_response_acknowledgement(&request_id)
            };
            messages.extend(drained);
            if !matches!(acknowledgement, ResponseAcknowledgement::Queued) {
                return (messages, acknowledgement);
            }
            if Instant::now() >= deadline {
                return (messages, ResponseAcknowledgement::Queued);
            }
            tokio::time::sleep(
                RESPONSE_ACK_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    /// Create a new supervisor
    #[must_use]
    pub fn new(config: SupervisorConfig) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            config,
            transport: None,
            last_init: None,
            semantic_conversation: None,
            last_workspace_capability_set: None,
            pending_workspace_capability_sets: HashMap::new(),
            state: AgentState::default(),
            event_tx,
            event_rx,
            health_status: HealthStatus::Unknown,
            last_response: None,
            reconnect_attempts: 0,
            stale_reference_retries: 0,
            remote_resume_authority: None,
            pending_auto_reconnect: false,
            pending_transport_shutdown: None,
            session_recorder: None,
            session_recorder_error_reported: false,
            cancel_token: CancellationToken::new(),
            pending_response_acknowledgements: HashSet::new(),
            pending_response_rejections: HashMap::new(),
            expected_response_acknowledgements: HashMap::new(),
            transport_generation: 0,
        }
    }

    /// Attach a session recorder
    #[must_use]
    pub fn with_session_recorder(mut self, recorder: SessionRecorder) -> Self {
        self.session_recorder = Some(recorder);
        self.session_recorder_error_reported = false;
        self
    }

    fn session_recorder_error_to_report(
        &mut self,
        result: std::io::Result<()>,
    ) -> Option<std::io::Error> {
        match result {
            Ok(()) => {
                self.session_recorder_error_reported = false;
                None
            }
            Err(_) if self.session_recorder_error_reported => None,
            Err(error) => {
                self.session_recorder_error_reported = true;
                Some(error)
            }
        }
    }

    fn report_session_recorder_result_with<F>(
        &mut self,
        result: std::io::Result<()>,
        operation: &str,
        reporter: F,
    ) where
        F: FnOnce(String) -> bool,
    {
        if let Some(error) = self.session_recorder_error_to_report(result) {
            self.session_recorder_error_reported =
                reporter(format!("[supervisor] failed to {operation}: {error}"));
        }
    }

    fn report_session_recorder_result(&mut self, result: std::io::Result<()>, operation: &str) {
        self.report_session_recorder_result_with(
            result,
            operation,
            try_report_diagnostic_nonblocking,
        );
    }

    /// Seed the supervisor with a replayed session snapshot.
    #[must_use]
    pub fn with_session_replay(mut self, replay: SessionReplay) -> Self {
        self.restore_session_replay(replay);
        self
    }

    /// Restore the supervisor's saved init config and reconstructed agent state.
    pub fn restore_session_replay(&mut self, replay: SessionReplay) {
        self.state = replay.state;
        self.last_init = replay.last_init;
        self.semantic_conversation = replay.semantic_conversation;
        self.last_workspace_capability_set = replay.last_workspace_capability_set;
        self.pending_workspace_capability_sets.clear();
        self.seed_remote_session_id_if_missing(self.state.session_id.clone());
    }

    async fn spawn_transport(&mut self) -> Result<ManagedTransport, AsyncTransportError> {
        if let Some(remote_config) = self.config.remote.as_mut() {
            if remote_config.session_id.is_none() {
                remote_config.session_id = self.state.session_id.clone();
            }
            match RemoteAgentTransport::connect_with_resume_authority(
                remote_config.clone(),
                self.remote_resume_authority.clone(),
            )
            .await
            {
                Ok(transport) => Ok(ManagedTransport::Remote(Box::new(transport))),
                Err(failure) => {
                    if let Some(resume_authority) = failure.resume_authority {
                        self.remote_resume_authority = Some(resume_authority);
                    }
                    Err(failure.error)
                }
            }
        } else {
            AsyncAgentTransport::spawn(self.config.transport.clone())
                .await
                .map(Box::new)
                .map(ManagedTransport::Local)
        }
    }

    /// Connect to the agent
    pub async fn connect(&mut self) -> Result<(), AsyncTransportError> {
        self.wait_for_pending_transport_shutdown().await;
        let transport = self.spawn_transport().await?;
        self.set_transport(transport)?;
        self.health_status = HealthStatus::Healthy;
        self.reconnect_attempts = 0;
        let _ = self.event_tx.send(SupervisorEvent::Connected);
        let _ = self.event_tx.send(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy,
        });
        Ok(())
    }

    /// Disconnect from the agent
    pub fn disconnect(&mut self) {
        let is_remote_supervisor = self.config.remote.is_some();
        if let Some(transport) = self.transport.take() {
            self.begin_transport_shutdown(transport);
        }
        self.clear_disconnect_state();
        self.last_response = None;
        self.pending_auto_reconnect = false;
        self.stale_reference_retries = 0;
        if is_remote_supervisor {
            self.clear_remote_resume_authority();
        }
        self.health_status = HealthStatus::Unhealthy;
        let _ = self.event_tx.send(SupervisorEvent::Disconnected {
            error: "Disconnected by request".to_string(),
        });
    }

    /// Attempt reconnection with exponential backoff
    pub async fn reconnect(&mut self) -> Result<(), AsyncTransportError> {
        if self.health_status != HealthStatus::Reconnecting {
            self.health_status = HealthStatus::Reconnecting;
            let _ = self.event_tx.send(SupervisorEvent::HealthChanged {
                status: HealthStatus::Reconnecting,
            });
        }
        if let Some(existing) = self.transport.take() {
            let _ = existing.shutdown_and_wait().await;
        }
        self.wait_for_pending_transport_shutdown().await;
        self.last_response = None;

        let max_attempts = self.config.max_reconnect_attempts;
        let mut delay = self.config.reconnect_delay;
        let reconnect_started_at = Instant::now();

        loop {
            self.reconnect_attempts += 1;

            let _ = self.event_tx.send(SupervisorEvent::Reconnecting {
                attempt: self.reconnect_attempts,
                max_attempts,
            });

            match self.spawn_transport().await {
                Ok(transport) => {
                    self.set_transport(transport)?;
                    self.health_status = HealthStatus::Healthy;
                    self.reconnect_attempts = 0;
                    self.stale_reference_retries = 0;
                    self.pending_auto_reconnect = false;
                    let _ = self.event_tx.send(SupervisorEvent::Connected);
                    let _ = self.event_tx.send(SupervisorEvent::HealthChanged {
                        status: HealthStatus::Healthy,
                    });
                    return Ok(());
                }
                Err(e) => {
                    let within_retry_budget = self.consume_retry_budget_for_error(&e);
                    let reconnect_elapsed = reconnect_started_at.elapsed();
                    if !within_retry_budget
                        || (max_attempts > 0 && self.reconnect_attempts >= max_attempts)
                        || reconnect_elapsed >= self.config.max_reconnect_elapsed
                    {
                        self.health_status = HealthStatus::Unhealthy;
                        let _ = self.event_tx.send(SupervisorEvent::HealthChanged {
                            status: HealthStatus::Unhealthy,
                        });
                        return Err(e);
                    }

                    // Wait with backoff, but never sleep past the reconnect budget.
                    let remaining_budget = self
                        .config
                        .max_reconnect_elapsed
                        .saturating_sub(reconnect_elapsed);
                    let sleep_duration =
                        jittered_reconnect_delay(delay, self.config.reconnect_jitter_factor)
                            .min(remaining_budget);
                    if sleep_duration.is_zero() {
                        self.health_status = HealthStatus::Unhealthy;
                        let _ = self.event_tx.send(SupervisorEvent::HealthChanged {
                            status: HealthStatus::Unhealthy,
                        });
                        return Err(e);
                    }

                    tokio::time::sleep(sleep_duration).await;
                    delay = Duration::from_secs_f64(
                        (delay.as_secs_f64() * self.config.backoff_multiplier)
                            .min(self.config.max_reconnect_delay.as_secs_f64()),
                    );
                }
            }
        }
    }

    /// Send a message to the agent
    pub fn send(&mut self, msg: ToAgentMessage) -> Result<(), AsyncTransportError> {
        let Some(transport) = &self.transport else {
            return Err(AsyncTransportError::ChannelClosed);
        };

        transport.send(msg.clone())?;
        if let ToAgentMessage::ApplyWorkspaceCapabilitySet { request } = &msg {
            self.pending_workspace_capability_sets
                .insert(workspace_capability_replay_cursor(request), request.clone());
        }
        self.last_response = Some(Instant::now());
        if let Some(ref mut recorder) = self.session_recorder {
            let result = recorder.record_sent(&msg);
            let replay_state = recorder.replay_state().clone();
            let recorder_last_init = recorder.last_init().cloned();
            self.report_session_recorder_result(result, "record sent message");
            self.state = replay_state;
            self.last_init = recorder_last_init.or_else(|| self.last_init.clone());
        } else {
            self.state.handle_sent_message(&msg);
            self.last_init = match &msg {
                ToAgentMessage::Init {
                    system_prompt,
                    append_system_prompt,
                    thinking_level,
                    approval_mode,
                    history,
                } => Some(InitConfig {
                    system_prompt: system_prompt.clone(),
                    append_system_prompt: append_system_prompt.clone(),
                    thinking_level: *thinking_level,
                    approval_mode: *approval_mode,
                    history: history.clone(),
                    code_mode: None,
                    tool_grant: None,
                }),
                ToAgentMessage::GovernedInit {
                    system_prompt,
                    append_system_prompt,
                    thinking_level,
                    approval_mode,
                    history,
                    code_mode,
                    tool_grant,
                } => Some(InitConfig {
                    system_prompt: system_prompt.clone(),
                    append_system_prompt: append_system_prompt.clone(),
                    thinking_level: *thinking_level,
                    approval_mode: *approval_mode,
                    history: history.clone(),
                    code_mode: Some(*code_mode),
                    tool_grant: Some(tool_grant.clone()),
                }),
                _ => self.last_init.clone(),
            };
        }

        Ok(())
    }

    /// Bind the local child to the immutable resident controller identity.
    /// The configured extension is replayed on every later child spawn.
    pub(crate) fn set_local_controller_binding(
        &mut self,
        binding: ControllerBindingHello,
    ) -> Result<(), AsyncTransportError> {
        if self.config.remote.is_some() {
            return Err(AsyncTransportError::SendFailed(
                "hosted controller binding cannot be installed on a remote child transport"
                    .to_string(),
            ));
        }
        if let Some(existing) = self.config.transport.controller_binding.as_ref() {
            if existing != &binding {
                return Err(AsyncTransportError::SendFailed(
                    "hosted controller binding cannot be replaced".to_string(),
                ));
            }
        } else {
            self.config.transport.controller_binding = Some(binding.clone());
        }
        self.send(ToAgentMessage::Hello {
            protocol_version: Some(super::HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: Some(ClientInfo {
                name: "maestro-tui-rs".to_string(),
                version: option_env!("CARGO_PKG_VERSION").map(str::to_string),
            }),
            capabilities: Some(super::local_controller_capabilities()),
            role: Some(ConnectionRole::Controller),
            opt_out_notifications: None,
            controller_binding: Some(binding),
        })
    }

    /// Send a protocol message and drain any agent messages that are already available.
    pub fn send_and_drain_agent_messages(
        &mut self,
        msg: ToAgentMessage,
    ) -> Result<Vec<FromAgentMessage>, AsyncTransportError> {
        Ok(self.send_and_drain_agent_messages_with_ack(msg)?.0)
    }

    /// Send a message and report whether the native response consumer
    /// explicitly acknowledged accepting a control response.
    pub(crate) fn send_and_drain_agent_messages_with_ack(
        &mut self,
        msg: ToAgentMessage,
    ) -> Result<(Vec<FromAgentMessage>, ResponseAcknowledgement), AsyncTransportError> {
        let expected_acknowledgement = response_ack_request_id(&msg).map(str::to_owned);
        if let Some(request_id) = expected_acknowledgement.as_ref() {
            if !self.register_response_acknowledgement(request_id) {
                return Err(AsyncTransportError::SendFailed(
                    "response acknowledgement capacity is full".to_string(),
                ));
            }
        }
        if let Err(error) = self.send(msg) {
            if let Some(request_id) = expected_acknowledgement.as_ref() {
                self.expected_response_acknowledgements.remove(request_id);
            }
            return Err(error);
        }
        Ok(self.drain_agent_messages_until_ack(
            expected_acknowledgement.as_deref(),
            RESPONSE_ACK_TIMEOUT,
        ))
    }

    fn drain_agent_messages_until_ack(
        &mut self,
        expected_acknowledgement: Option<&str>,
        _timeout: Duration,
    ) -> (Vec<FromAgentMessage>, ResponseAcknowledgement) {
        let Some(expected_acknowledgement) = expected_acknowledgement else {
            return (
                self.drain_available_agent_messages(),
                ResponseAcknowledgement::NotExpected,
            );
        };
        if self
            .pending_response_acknowledgements
            .remove(expected_acknowledgement)
        {
            self.expected_response_acknowledgements
                .remove(expected_acknowledgement);
            return (Vec::new(), ResponseAcknowledgement::Consumed);
        }
        if self
            .pending_response_rejections
            .remove(expected_acknowledgement)
            .is_some()
        {
            self.expected_response_acknowledgements
                .remove(expected_acknowledgement);
            return (Vec::new(), ResponseAcknowledgement::Rejected);
        }
        let messages = self.drain_available_agent_messages();
        if self
            .pending_response_rejections
            .remove(expected_acknowledgement)
            .is_some()
        {
            self.expected_response_acknowledgements
                .remove(expected_acknowledgement);
            return (messages, ResponseAcknowledgement::Rejected);
        }
        if self
            .pending_response_acknowledgements
            .remove(expected_acknowledgement)
        {
            self.expected_response_acknowledgements
                .remove(expected_acknowledgement);
            return (messages, ResponseAcknowledgement::Consumed);
        }
        (messages, ResponseAcknowledgement::Queued)
    }

    /// Drain currently available supervisor agent events as headless protocol messages.
    #[must_use]
    pub fn drain_available_agent_messages(&mut self) -> Vec<FromAgentMessage> {
        let mut messages = Vec::new();
        while let Some(event) = self.poll() {
            match event {
                SupervisorEvent::Agent(agent_event) => {
                    let message = agent_event_to_message(&agent_event);
                    if let FromAgentMessage::Error {
                        request_id: Some(request_id),
                        message: reason,
                        error_type: Some(super::messages::HeadlessErrorType::Protocol),
                        ..
                    } = &message
                    {
                        if self
                            .expected_response_acknowledgements
                            .contains_key(request_id)
                            && self.pending_response_rejections.len()
                                < MAX_RESPONSE_ACKNOWLEDGEMENTS
                        {
                            self.pending_response_rejections
                                .insert(request_id.clone(), reason.clone());
                        }
                    }
                    messages.push(message);
                }
                SupervisorEvent::ControlMessage(message) => messages.push(*message),
                SupervisorEvent::ResponseAccepted { request_id }
                    if self
                        .expected_response_acknowledgements
                        .contains_key(&request_id)
                        && self.pending_response_acknowledgements.len()
                            < MAX_RESPONSE_ACKNOWLEDGEMENTS =>
                {
                    self.pending_response_acknowledgements.insert(request_id);
                }
                _ => {}
            }
        }
        messages
    }

    pub(crate) fn wait_for_response_acknowledgement(
        &mut self,
        request_id: &str,
    ) -> (Vec<FromAgentMessage>, ResponseAcknowledgement) {
        self.drain_agent_messages_until_ack(Some(request_id), RESPONSE_ACK_TIMEOUT)
    }

    pub(crate) fn has_response_acknowledgement(&self, request_id: &str) -> bool {
        self.pending_response_acknowledgements.contains(request_id)
    }

    pub(crate) fn has_response_rejection(&self, request_id: &str) -> bool {
        self.pending_response_rejections.contains_key(request_id)
    }

    pub(crate) fn take_response_rejection(&mut self, request_id: &str) -> Option<String> {
        let rejection = self.pending_response_rejections.remove(request_id);
        if rejection.is_some() {
            self.expected_response_acknowledgements.remove(request_id);
            self.pending_response_acknowledgements.remove(request_id);
        }
        rejection
    }

    pub(crate) fn take_response_acknowledgement(&mut self, request_id: &str) -> bool {
        let acknowledged = self.pending_response_acknowledgements.remove(request_id);
        if acknowledged {
            self.expected_response_acknowledgements.remove(request_id);
        }
        acknowledged
    }

    pub(crate) fn discard_response_acknowledgement(
        &mut self,
        request_id: &str,
        transport_generation: u64,
    ) {
        if self.expected_response_acknowledgements.get(request_id) != Some(&transport_generation) {
            return;
        }
        self.expected_response_acknowledgements.remove(request_id);
        self.pending_response_acknowledgements.remove(request_id);
        self.pending_response_rejections.remove(request_id);
    }

    pub(crate) fn register_response_acknowledgement(&mut self, request_id: &str) -> bool {
        if let Some(transport_generation) = self
            .expected_response_acknowledgements
            .get(request_id)
            .copied()
        {
            if transport_generation == self.transport_generation {
                return true;
            }
            self.pending_response_acknowledgements.remove(request_id);
            self.pending_response_rejections.remove(request_id);
            self.expected_response_acknowledgements
                .insert(request_id.to_string(), self.transport_generation);
            return true;
        }
        if self.expected_response_acknowledgements.len() >= MAX_RESPONSE_ACKNOWLEDGEMENTS {
            return false;
        }
        self.expected_response_acknowledgements
            .insert(request_id.to_string(), self.transport_generation);
        true
    }

    #[cfg(test)]
    pub(crate) fn response_acknowledgement_count(&self) -> usize {
        self.expected_response_acknowledgements.len()
    }

    #[must_use]
    pub(crate) fn transport_generation(&self) -> u64 {
        self.transport_generation
    }

    /// Send a prompt
    pub fn prompt(&mut self, content: impl Into<String>) -> Result<(), AsyncTransportError> {
        self.send(ToAgentMessage::Prompt {
            content: content.into(),
            attachments: None,
            managed_inference_authorization: None,
        })
    }

    /// Configure the agent before sending prompts
    pub fn init(&mut self, config: InitConfig) -> Result<(), AsyncTransportError> {
        self.last_init = Some(config.clone());
        self.send(Self::init_message(&config))
    }

    fn replay_saved_init(&mut self) -> Result<(), AsyncTransportError> {
        if let Some(config) = self.last_init.clone() {
            self.send(Self::init_message(&config))?;
        }
        Ok(())
    }

    fn replay_saved_semantic_conversation(&mut self) -> Result<(), AsyncTransportError> {
        if self.last_init.is_none() {
            return Ok(());
        }
        if let Some(messages) = self.semantic_conversation.clone() {
            self.send(ToAgentMessage::RestoreConversation {
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_string(),
                messages,
            })?;
        }
        Ok(())
    }

    fn replay_saved_workspace_capability_set(&mut self) -> Result<(), AsyncTransportError> {
        if let Some(request) = self.last_workspace_capability_set.clone() {
            self.send(ToAgentMessage::ApplyWorkspaceCapabilitySet { request })?;
        }
        Ok(())
    }

    fn remember_remote_session_id(&mut self, session_id: Option<String>) {
        if let (Some(remote), Some(session_id)) = (self.config.remote.as_mut(), session_id) {
            remote.session_id = Some(session_id);
        }
    }

    fn clear_remote_resume_authority(&mut self) {
        self.remote_resume_authority = None;
        if let Some(remote) = self.config.remote.as_mut() {
            remote.connection_id = None;
        }
    }

    fn seed_remote_session_id_if_missing(&mut self, session_id: Option<String>) {
        if let (Some(remote), Some(session_id)) = (self.config.remote.as_mut(), session_id) {
            if remote.session_id.is_none() {
                remote.session_id = Some(session_id);
            }
        }
    }

    fn set_transport(&mut self, transport: ManagedTransport) -> Result<(), AsyncTransportError> {
        let remote_session_id = transport.remote_session_id().map(str::to_string);
        let remote_resume_authority = transport.remote_resume_authority();
        let should_replay_init = transport.needs_init_replay();
        let snapshot = transport.initial_snapshot();
        self.transport = Some(transport);
        self.transport_generation = self.transport_generation.wrapping_add(1);
        self.pending_response_acknowledgements.clear();
        self.pending_response_rejections.clear();
        self.expected_response_acknowledgements.clear();
        self.stale_reference_retries = 0;
        self.last_response = Some(Instant::now());
        if let Some((state, last_init)) = snapshot {
            self.apply_snapshot(state, last_init);
        }
        self.remember_remote_session_id(
            remote_session_id.or_else(|| self.state.session_id.clone()),
        );
        self.remote_resume_authority = remote_resume_authority;
        if should_replay_init {
            if let Err(error) = self
                .replay_saved_init()
                .and_then(|()| self.replay_saved_semantic_conversation())
                .and_then(|()| self.replay_saved_workspace_capability_set())
            {
                if let Some(transport) = self.transport.take() {
                    let _ = transport.shutdown();
                }
                return Err(error);
            }
        }
        Ok(())
    }

    fn init_message(config: &InitConfig) -> ToAgentMessage {
        match (config.code_mode, config.tool_grant.as_ref()) {
            (Some(code_mode), Some(tool_grant)) => ToAgentMessage::GovernedInit {
                system_prompt: config.system_prompt.clone(),
                append_system_prompt: config.append_system_prompt.clone(),
                thinking_level: config.thinking_level,
                approval_mode: config.approval_mode,
                history: config.history.clone(),
                code_mode,
                tool_grant: tool_grant.clone(),
            },
            _ => ToAgentMessage::Init {
                system_prompt: config.system_prompt.clone(),
                append_system_prompt: config.append_system_prompt.clone(),
                thinking_level: config.thinking_level,
                approval_mode: config.approval_mode,
                history: config.history.clone(),
            },
        }
    }

    fn apply_snapshot(&mut self, state: AgentState, last_init: Option<InitConfig>) {
        let resolved_last_init = last_init.or_else(|| self.last_init.clone());
        self.state = state;
        self.last_init = resolved_last_init.clone();
        self.remember_remote_session_id(self.state.session_id.clone());
        if let Some(ref mut recorder) = self.session_recorder {
            let result = recorder.apply_snapshot(self.state.clone(), resolved_last_init.clone());
            self.report_session_recorder_result(result, "record session snapshot");
        }
        let _ = self.event_tx.send(SupervisorEvent::StateHydrated {
            session_id: self.state.session_id.clone(),
        });
    }

    fn apply_agent_message(&mut self, message: FromAgentMessage) -> Option<SupervisorEvent> {
        if let FromAgentMessage::ResponseAccepted(tool_wire::ResponseAccepted { request_id }) =
            &message
        {
            self.state.handle_message(message.clone());
            return Some(SupervisorEvent::ResponseAccepted {
                request_id: request_id.clone(),
            });
        }
        if let FromAgentMessage::ConversationSnapshot {
            protocol_version,
            messages,
            ..
        } = &message
        {
            // This in-memory reconnect boundary must not depend on the optional
            // recorder. An unsupported live version is fail-closed and clears
            // previously replayed history immediately.
            self.semantic_conversation = (protocol_version
                == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL)
                .then(|| messages.clone());
        }
        let accepted_capability_receipt =
            if let FromAgentMessage::WorkspaceCapabilitySetApplied { receipt } = &message {
                self.accept_workspace_capability_receipt(receipt)
            } else {
                true
            };
        let event = accepted_capability_receipt
            .then(|| self.state.handle_message(message.clone()))
            .flatten();
        if let Some(ref mut recorder) = self.session_recorder {
            let result = recorder.record_received(&message);
            self.report_session_recorder_result(result, "record received message");
        }
        // Pending requests update AgentState without producing a UI AgentEvent.
        // The hosted runtime drains this supervisor as a protocol transport;
        // dropping those messages strands the native tool-response waiter.
        if matches!(
            &message,
            FromAgentMessage::ManagedAuthorizationRequest { .. }
                | FromAgentMessage::ClientToolRequest { .. }
                | FromAgentMessage::GovernedClientToolRequest(
                    tool_wire::GovernedClientToolRequest { .. }
                )
                | FromAgentMessage::ServerRequest { .. }
                | FromAgentMessage::ServerRequestResolved { .. }
        ) {
            return Some(SupervisorEvent::ControlMessage(Box::new(message)));
        }
        event.map(|event| SupervisorEvent::Agent(Box::new(event)))
    }

    fn accept_workspace_capability_receipt(
        &mut self,
        receipt: &WorkspaceCapabilitySetApplied,
    ) -> bool {
        accept_workspace_capability_receipt(
            &mut self.pending_workspace_capability_sets,
            &mut self.last_workspace_capability_set,
            receipt,
        )
    }

    fn clear_transient_progress_state(&mut self) {
        self.state.clear_transient_progress();
        self.persist_session_snapshot();
    }

    fn clear_disconnect_state(&mut self) {
        self.state.clear_transient_progress();
        self.state.clear_pending_request_state();
        self.persist_session_snapshot();
    }

    fn persist_session_snapshot(&mut self) {
        if let Some(ref mut recorder) = self.session_recorder {
            let result = recorder.apply_snapshot(self.state.clone(), self.last_init.clone());
            self.report_session_recorder_result(result, "persist session snapshot");
        }
    }

    fn begin_transport_shutdown(&mut self, transport: ManagedTransport) {
        if let Some(handle) = self.pending_transport_shutdown.take() {
            handle.abort();
        }

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            self.pending_transport_shutdown = Some(handle.spawn(async move {
                let _ = transport.shutdown_and_wait().await;
            }));
        } else {
            let _ = transport.shutdown();
            self.pending_transport_shutdown = None;
        }
    }

    async fn wait_for_pending_transport_shutdown(&mut self) {
        if let Some(handle) = self.pending_transport_shutdown.take() {
            // This handle was just taken from `self` and nothing else holds a
            // reference to abort it, so an `Err` here is a genuine panic in
            // the background shutdown task, not a routine cancellation.
            if let Err(error) = handle.await {
                report_diagnostic_nonblocking(format!(
                    "[supervisor] transport shutdown task failed to join: {error}"
                ));
            }
        }
    }

    fn handle_transport_error(&mut self, error: AsyncTransportError) -> SupervisorEvent {
        if let Some(transport) = self.transport.take() {
            self.begin_transport_shutdown(transport);
        }
        self.clear_transient_progress_state();
        self.last_response = None;
        self.health_status = HealthStatus::Unhealthy;
        SupervisorEvent::Disconnected {
            error: error.to_string(),
        }
    }

    fn schedule_auto_reconnect(&mut self) {
        if self.config.auto_reconnect {
            self.pending_auto_reconnect = true;
        }
    }

    fn consume_retry_budget_for_error(&mut self, error: &AsyncTransportError) -> bool {
        if !error.is_retryable() {
            self.stale_reference_retries = 0;
            return false;
        }

        if error.uses_stale_reference_retry_budget() {
            self.stale_reference_retries += 1;
            return self.stale_reference_retries <= MAX_STALE_REMOTE_REFERENCE_RETRIES;
        }

        self.stale_reference_retries = 0;
        true
    }

    fn handle_transport_disconnect(&mut self, error: AsyncTransportError) -> SupervisorEvent {
        let should_retry =
            self.config.auto_reconnect && self.consume_retry_budget_for_error(&error);
        if let Some(transport) = self.transport.take() {
            self.begin_transport_shutdown(transport);
        }
        self.last_response = None;
        if should_retry {
            self.health_status = HealthStatus::Reconnecting;
            self.schedule_auto_reconnect();
            return SupervisorEvent::HealthChanged {
                status: HealthStatus::Reconnecting,
            };
        }
        self.clear_transient_progress_state();
        self.health_status = HealthStatus::Unhealthy;
        self.pending_auto_reconnect = false;
        SupervisorEvent::Disconnected {
            error: error.to_string(),
        }
    }

    fn mark_response_received(&mut self, emit_health_event: bool) {
        self.last_response = Some(Instant::now());
        if emit_health_event && self.health_status != HealthStatus::Healthy {
            self.health_status = HealthStatus::Healthy;
            let _ = self.event_tx.send(SupervisorEvent::HealthChanged {
                status: HealthStatus::Healthy,
            });
        }
    }

    fn silence_timeouts_enabled(&self) -> bool {
        !matches!(
            self.config
                .remote
                .as_ref()
                .and_then(|remote| remote.role.as_deref()),
            Some("viewer")
        )
    }

    fn remote_compaction_timeout(&self) -> Option<Duration> {
        if !self.silence_timeouts_enabled()
            || self.config.remote.is_none()
            || !self.state.is_responding
        {
            return None;
        }

        self.state
            .last_status
            .as_deref()
            .filter(|status| status.trim().eq_ignore_ascii_case("compacting"))
            .map(|_| REMOTE_COMPACTION_SILENCE_TIMEOUT)
    }

    fn next_health_deadline(&self) -> Option<Instant> {
        if !self.silence_timeouts_enabled() {
            return None;
        }
        let last_response = self.last_response?;
        if let Some(timeout) = self.remote_compaction_timeout() {
            return Some(last_response + timeout);
        }
        match self.health_status {
            HealthStatus::Healthy => Some(last_response + self.config.health_check_interval),
            HealthStatus::Degraded => Some(
                last_response
                    + self.config.health_check_interval
                    + self.config.health_check_timeout,
            ),
            _ => None,
        }
    }

    fn next_health_timeout(&self, now: Instant) -> Option<Duration> {
        self.next_health_deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn due_health_transition(&mut self, now: Instant) -> Option<SupervisorEvent> {
        if !self.silence_timeouts_enabled() {
            return None;
        }
        let last_response = self.last_response?;
        let silence = now.saturating_duration_since(last_response);
        if let Some(timeout) = self.remote_compaction_timeout() {
            if silence < timeout {
                return None;
            }

            return Some(self.transition_to_unhealthy_for_silence());
        }
        match self.health_status {
            HealthStatus::Healthy if silence >= self.config.health_check_interval => {
                self.health_status = HealthStatus::Degraded;
                Some(SupervisorEvent::HealthChanged {
                    status: HealthStatus::Degraded,
                })
            }
            HealthStatus::Degraded
                if silence
                    >= self.config.health_check_interval + self.config.health_check_timeout =>
            {
                Some(self.transition_to_unhealthy_for_silence())
            }
            _ => None,
        }
    }

    fn transition_to_unhealthy_for_silence(&mut self) -> SupervisorEvent {
        self.health_status = HealthStatus::Unhealthy;
        self.last_response = None;
        if self.config.auto_reconnect {
            if let Some(transport) = self.transport.take() {
                self.begin_transport_shutdown(transport);
            }
            self.pending_auto_reconnect = true;
        } else {
            self.pending_auto_reconnect = false;
            self.clear_transient_progress_state();
        }
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Unhealthy,
        }
    }

    fn handle_transport_incoming(&mut self, incoming: ManagedIncoming) -> Option<SupervisorEvent> {
        let emit_health_event = self.transport.is_some();
        match incoming {
            ManagedIncoming::Message(message) => {
                self.mark_response_received(emit_health_event);
                self.apply_agent_message(*message)
            }
            ManagedIncoming::Snapshot { state, last_init } => {
                self.mark_response_received(emit_health_event);
                self.apply_snapshot(*state, last_init);
                None
            }
            ManagedIncoming::Reset {
                reason: _reason,
                state,
                last_init,
            } => {
                self.mark_response_received(emit_health_event);
                self.apply_snapshot(*state, last_init);
                None
            }
            ManagedIncoming::Heartbeat => {
                self.mark_response_received(emit_health_event);
                None
            }
        }
    }

    /// Poll for events (non-blocking)
    pub fn poll(&mut self) -> Option<SupervisorEvent> {
        if let Ok(event) = self.event_rx.try_recv() {
            return Some(event);
        }

        loop {
            let next_result = match self.transport.as_mut() {
                Some(transport) => transport.try_recv_incoming(),
                None => None,
            };
            let Some(result) = next_result else {
                break;
            };
            match result {
                Ok(incoming) => {
                    if let Some(event) = self.handle_transport_incoming(incoming) {
                        return Some(event);
                    }
                }
                Err(error) => return Some(self.handle_transport_disconnect(error)),
            }
        }

        if self.transport.is_some() {
            if let Some(event) = self.due_health_transition(Instant::now()) {
                return Some(event);
            }
        }

        self.event_rx.try_recv().ok()
    }

    /// Wait for the next event
    pub async fn recv(&mut self) -> Option<SupervisorEvent> {
        // Clone the cancel token to avoid borrow conflict
        let cancel_token = self.cancel_token.clone();
        tokio::select! {
            () = cancel_token.cancelled() => {
                Some(SupervisorEvent::ShuttingDown)
            }
            event = self.recv_internal() => {
                event
            }
        }
    }

    async fn recv_internal(&mut self) -> Option<SupervisorEvent> {
        loop {
            if let Ok(event) = self.event_rx.try_recv() {
                return Some(event);
            }

            if self.transport.is_none() {
                if self.pending_auto_reconnect {
                    self.pending_auto_reconnect = false;
                    if let Err(error) = self.reconnect().await {
                        let disconnected = self.handle_transport_error(error);
                        let _ = self.event_tx.send(disconnected);
                    }
                    continue;
                }
                return self.event_rx.recv().await;
            }

            let now = Instant::now();
            if let Some(event) = self.due_health_transition(now) {
                return Some(event);
            }

            let next_timeout = self.next_health_timeout(now);
            let result = {
                let transport = self.transport.as_mut()?;
                if let Some(timeout) = next_timeout {
                    match tokio::time::timeout(timeout, transport.recv_incoming()).await {
                        Ok(result) => result,
                        Err(_) => continue,
                    }
                } else {
                    transport.recv_incoming().await
                }
            };
            match result {
                Ok(incoming) => {
                    if let Some(event) = self.handle_transport_incoming(incoming) {
                        return Some(event);
                    }
                }
                Err(error) => return Some(self.handle_transport_disconnect(error)),
            }
        }
    }

    /// Check current health status
    #[must_use]
    pub fn health(&self) -> HealthStatus {
        self.health_status
    }

    /// Check if connected
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    pub(crate) fn event_notification(&self) -> Option<Arc<Notify>> {
        self.transport
            .as_ref()
            .map(ManagedTransport::event_notification)
    }

    /// Get a reference to the current supervisor-owned agent state.
    #[must_use]
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Get the underlying transport (if connected)
    #[must_use]
    pub fn transport(&self) -> Option<&AsyncAgentTransport> {
        self.transport
            .as_ref()
            .and_then(ManagedTransport::local_transport)
    }

    /// Get mutable transport
    pub fn transport_mut(&mut self) -> Option<&mut AsyncAgentTransport> {
        self.transport
            .as_mut()
            .and_then(ManagedTransport::local_transport_mut)
    }

    /// Shutdown the supervisor
    pub fn shutdown(&mut self) {
        self.cancel_token.cancel();
        self.disconnect();
        let _ = self.event_tx.send(SupervisorEvent::ShuttingDown);
    }

    /// Shutdown the supervisor and wait for its active transport to be reaped.
    pub(crate) async fn shutdown_and_wait(&mut self) {
        self.shutdown();
        self.wait_for_pending_transport_shutdown().await;
    }

    /// Flush session recorder
    pub fn flush_session(&mut self) -> std::io::Result<()> {
        if let Some(ref mut recorder) = self.session_recorder {
            recorder.flush()?;
        }
        Ok(())
    }

    /// Return the active session recorder path, when recording is enabled.
    #[must_use]
    pub fn session_file(&self) -> Option<&Path> {
        self.session_recorder.as_ref().map(SessionRecorder::path)
    }
}

pub(crate) fn response_ack_request_id(message: &ToAgentMessage) -> Option<&str> {
    match message {
        ToAgentMessage::ToolResponse { call_id, .. }
        | ToAgentMessage::ClientToolResult { call_id, .. }
        | ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
            call_id,
            ..
        }) => Some(call_id),
        ToAgentMessage::ServerRequestResponse { request_id, .. }
        | ToAgentMessage::ManagedAuthorizationResult { request_id, .. } => Some(request_id),
        _ => None,
    }
}

/// Builder for `AgentSupervisor`
pub struct SupervisorBuilder {
    config: SupervisorConfig,
    session_recorder: Option<SessionRecorder>,
    session_replay: Option<SessionReplay>,
}

impl SupervisorBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: SupervisorConfig::default(),
            session_recorder: None,
            session_replay: None,
        }
    }

    /// Set the CLI path
    pub fn cli_path(mut self, path: impl Into<String>) -> Self {
        self.config.transport.cli_path = path.into();
        self
    }

    /// Set working directory
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.config.transport.cwd = Some(cwd.into());
        self
    }

    /// Attach to a remote headless runtime instead of spawning a local CLI.
    #[must_use]
    pub fn remote(mut self, config: RemoteTransportConfig) -> Self {
        self.config.remote = Some(config);
        self
    }

    /// Set the remote base URL, enabling remote transport if needed.
    #[must_use]
    pub fn remote_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config
            .remote
            .get_or_insert_with(RemoteTransportConfig::default)
            .base_url = base_url.into();
        self
    }

    /// Set the remote API key, enabling remote transport if needed.
    #[must_use]
    pub fn remote_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.config
            .remote
            .get_or_insert_with(RemoteTransportConfig::default)
            .api_key = Some(api_key.into());
        self
    }

    /// Set the remote session id, enabling remote transport if needed.
    #[must_use]
    pub fn remote_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.config
            .remote
            .get_or_insert_with(RemoteTransportConfig::default)
            .session_id = Some(session_id.into());
        self
    }

    /// Opt out of specific live notification types on the remote subscription.
    #[must_use]
    pub fn remote_opt_out_notification(mut self, notification: impl Into<String>) -> Self {
        let config = self
            .config
            .remote
            .get_or_insert_with(RemoteTransportConfig::default);
        let notification = notification.into();
        if !config.opt_out_notifications.contains(&notification) {
            config.opt_out_notifications.push(notification);
        }
        self
    }

    /// Resume a recorded local session by restoring replay state and continuing
    /// to append to the same JSONL log.
    pub fn resume_recorded_session(
        mut self,
        sessions_dir: impl AsRef<Path>,
        session_id: &str,
    ) -> std::io::Result<Self> {
        // Build the replay snapshot from the recorder's own already-resolved
        // state (`SessionRecorder::resume` already loaded and, if the
        // session's metadata was corrupt, rotated it aside and rebuilt it
        // from the JSONL log) rather than a second, independent
        // `SessionReader::load`. A separate preliminary load here previously
        // raced that rotation: it would rotate the corrupt metadata file
        // aside itself, so by the time `SessionRecorder::resume` ran a
        // moment later it saw a *missing* (not corrupt) file, skipped its
        // own rebuild-from-JSONL path, and the next flush permanently reset
        // the session's historical title, usage totals, and message count
        // instead of preserving them.
        let recorder = SessionRecorder::resume(sessions_dir, session_id)?;
        let replay = recorder.replay();
        self.session_replay = Some(replay);
        self.session_recorder = Some(recorder);
        Ok(self)
    }

    /// Add an argument
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.config.transport.extra_args.push(arg.into());
        self
    }

    /// Add an environment variable for spawned local agents.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.transport.env.push((key.into(), value.into()));
        self
    }

    /// Set max reconnection attempts
    #[must_use]
    pub fn max_reconnect_attempts(mut self, attempts: u32) -> Self {
        self.config.max_reconnect_attempts = attempts;
        self
    }

    /// Set initial reconnection delay
    #[must_use]
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.config.reconnect_delay = delay;
        self
    }

    /// Set randomized reconnect jitter ratio (0.25 = +/-25%).
    #[must_use]
    pub fn reconnect_jitter_factor(mut self, jitter_factor: f64) -> Self {
        self.config.reconnect_jitter_factor = jitter_factor;
        self
    }

    /// Set maximum total wall-clock time allowed for a reconnect loop.
    #[must_use]
    pub fn max_reconnect_elapsed(mut self, elapsed: Duration) -> Self {
        self.config.max_reconnect_elapsed = elapsed;
        self
    }

    /// Enable/disable auto reconnect
    #[must_use]
    pub fn auto_reconnect(mut self, enabled: bool) -> Self {
        self.config.auto_reconnect = enabled;
        self
    }

    /// Attach a session recorder
    #[must_use]
    pub fn session_recorder(mut self, recorder: SessionRecorder) -> Self {
        self.session_recorder = Some(recorder);
        self
    }

    /// Seed the supervisor with a replayed session snapshot.
    #[must_use]
    pub fn session_replay(mut self, replay: SessionReplay) -> Self {
        self.session_replay = Some(replay);
        self
    }

    /// Build the supervisor
    #[must_use]
    pub fn build(self) -> AgentSupervisor {
        let mut supervisor = AgentSupervisor::new(self.config);
        if let Some(recorder) = self.session_recorder {
            supervisor = supervisor.with_session_recorder(recorder);
        }
        if let Some(replay) = self.session_replay {
            supervisor = supervisor.with_session_replay(replay);
        }
        supervisor
    }
}

impl Default for SupervisorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a supervisor agent event into the wire-level headless message shape.
#[must_use]
pub fn agent_event_to_message(event: &AgentEvent) -> FromAgentMessage {
    match event {
        AgentEvent::RawAgentEvent { event_type, event } => FromAgentMessage::RawAgentEvent {
            event_type: event_type.clone(),
            event: event.clone(),
        },
        AgentEvent::ManagedGatewayReceipt {
            request_id,
            record_id,
            lineage_id,
            record_status,
        } => FromAgentMessage::ManagedGatewayReceipt {
            request_id: request_id.clone(),
            record_id: record_id.clone(),
            lineage_id: lineage_id.clone(),
            record_status: record_status.clone(),
            prompt_experiment: None,
        },
        AgentEvent::WorkspaceCapabilitySetApplied { receipt } => {
            FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: receipt.clone(),
            }
        }
        AgentEvent::DelegationEvent { event } => FromAgentMessage::DelegationEvent {
            event: event.clone(),
        },
        AgentEvent::Ready {
            protocol_version,
            model,
            provider,
            session_id,
        } => FromAgentMessage::Ready {
            protocol_version: protocol_version.clone(),
            model: model.clone(),
            provider: provider.clone(),
            session_id: session_id.clone(),
        },
        AgentEvent::SessionInfo {
            session_id,
            cwd,
            git_branch,
        } => FromAgentMessage::SessionInfo {
            session_id: session_id.clone(),
            cwd: cwd.clone(),
            git_branch: git_branch.clone(),
        },
        AgentEvent::ResponseStart { response_id } => FromAgentMessage::ResponseStart {
            response_id: response_id.clone(),
        },
        AgentEvent::ResponseChunk {
            response_id,
            content,
            is_thinking,
        } => FromAgentMessage::ResponseChunk {
            response_id: response_id.clone(),
            content: content.clone(),
            is_thinking: *is_thinking,
        },
        AgentEvent::ResponseEnd {
            response_id,
            usage,
            tools_summary,
            duration_ms,
            ttft_ms,
            ..
        } => FromAgentMessage::ResponseEnd {
            response_id: response_id.clone(),
            usage: usage.clone(),
            tools_summary: tools_summary.clone(),
            duration_ms: *duration_ms,
            ttft_ms: *ttft_ms,
        },
        AgentEvent::TurnCompleted {
            response_id,
            coding_completion,
            coding_child_records,
        } => FromAgentMessage::TurnCompleted {
            response_id: response_id.clone(),
            coding_completion: coding_completion.clone(),
            coding_child_records: coding_child_records.clone(),
        },
        AgentEvent::TurnInterrupted {
            response_id,
            reason,
        } => FromAgentMessage::TurnInterrupted {
            response_id: response_id.clone(),
            reason: reason.clone(),
        },
        AgentEvent::CodexSessionState {
            state,
            thread_id,
            profile,
        } => FromAgentMessage::CodexSessionState {
            state: state.clone(),
            thread_id: thread_id.clone(),
            profile: profile.clone(),
        },
        AgentEvent::CodexTurnState {
            state,
            thread_id,
            turn_id,
        } => FromAgentMessage::CodexTurnState {
            state: state.clone(),
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        },
        AgentEvent::CodexUsageState { source, usage } => FromAgentMessage::CodexUsageState {
            source: source.clone(),
            usage: usage.clone(),
        },
        AgentEvent::CodexCompatibility {
            protocol_version,
            resume,
            steering,
        } => FromAgentMessage::CodexCompatibility {
            protocol_version: protocol_version.clone(),
            resume: *resume,
            steering: *steering,
        },
        AgentEvent::ToolCall {
            call_id,
            tool,
            args,
        } => FromAgentMessage::ToolCall {
            call_id: call_id.clone(),
            tool_execution_id: None,
            tool: tool.clone(),
            args: args.clone(),
            requires_approval: false,
        },
        AgentEvent::ApprovalRequired {
            call_id,
            tool,
            args,
        } => FromAgentMessage::ToolCall {
            call_id: call_id.clone(),
            tool_execution_id: None,
            tool: tool.clone(),
            args: args.clone(),
            requires_approval: true,
        },
        AgentEvent::ToolStart { call_id, .. } => FromAgentMessage::ToolStart {
            call_id: call_id.clone(),
        },
        AgentEvent::ToolOutput { call_id, content } => FromAgentMessage::ToolOutput {
            call_id: call_id.clone(),
            content: content.clone(),
        },
        AgentEvent::ToolEnd {
            call_id,
            tool_execution_id,
            success,
            receipt,
            ..
        } => {
            let tool = receipt.as_ref().map(|receipt| receipt.tool_name.clone());
            FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
                call_id: call_id.clone(),
                tool_execution_id: tool_execution_id.clone(),
                success: *success,
                tool,
                details: None,
                receipt: receipt.clone(),
            })
        }
        AgentEvent::Error {
            request_id,
            message,
            fatal,
            terminal,
            error_type,
        } => FromAgentMessage::Error {
            request_id: request_id.clone(),
            message: message.clone(),
            fatal: *fatal,
            terminal: *terminal,
            error_type: *error_type,
        },
        AgentEvent::ProviderError { kind, message } => FromAgentMessage::ProviderError {
            kind: *kind,
            message: message.clone(),
        },
        AgentEvent::Status { message } => FromAgentMessage::Status {
            message: message.clone(),
        },
        AgentEvent::Compaction {
            summary,
            first_kept_entry_index,
            tokens_before,
            auto,
            custom_instructions,
            continuation,
            timestamp,
        } => FromAgentMessage::Compaction {
            summary: summary.clone(),
            first_kept_entry_index: *first_kept_entry_index,
            tokens_before: *tokens_before,
            auto: *auto,
            custom_instructions: custom_instructions.clone(),
            continuation: continuation.clone(),
            timestamp: timestamp.clone(),
        },
    }
}

#[cfg(test)]
mod tests;
