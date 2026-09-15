//! Async transport layer for agent communication
//!
//! Provides tokio-based async communication with the Node.js agent subprocess.
//! This is the recommended transport for async applications.

use maestro_runtime_contracts::tool_wire;
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, Instant, timeout};
use tokio_util::sync::CancellationToken;

use super::local_controller_capabilities;
use super::messages::{
    AgentEvent, AgentState, ClientInfo, ConnectionRole, ControllerBindingHello, FromAgentMessage,
    InitConfig, ToAgentMessage,
};

#[cfg(target_os = "linux")]
const EXECUTABLE_BUSY_RETRIES: usize = 10;

#[cfg(target_os = "linux")]
const EXECUTABLE_BUSY_RETRY_DELAY: Duration = Duration::from_millis(10);

async fn spawn_agent_command(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(target_os = "linux")]
    {
        for retries in 0..=EXECUTABLE_BUSY_RETRIES {
            match cmd.spawn() {
                Ok(child) => return Ok(child),
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && retries < EXECUTABLE_BUSY_RETRIES =>
                {
                    tokio::time::sleep(EXECUTABLE_BUSY_RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the final retry returns its error")
    }
    #[cfg(not(target_os = "linux"))]
    {
        cmd.spawn()
    }
}

/// Configuration for the async agent transport
#[derive(Debug, Clone)]
pub struct AsyncTransportConfig {
    /// Path to the Maestro CLI (default: "maestro")
    pub cli_path: String,
    /// Working directory for the agent
    pub cwd: Option<String>,
    /// Additional arguments to pass to the agent
    pub extra_args: Vec<String>,
    /// Environment variables to set
    pub env: Vec<(String, String)>,
    /// Read timeout for messages (default: no timeout)
    pub read_timeout: Option<Duration>,
    /// Buffer size for stdout reader (default: 1MB)
    pub buffer_size: usize,
    /// Immutable resident controller binding forwarded to a local child hello.
    pub controller_binding: Option<ControllerBindingHello>,
}

impl Default for AsyncTransportConfig {
    fn default() -> Self {
        Self {
            cli_path: "maestro".to_string(),
            cwd: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            read_timeout: None,
            buffer_size: 1024 * 1024, // 1MB
            controller_binding: None,
        }
    }
}

/// Structured classification for remote HTTP/SSE transport failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteErrorKind {
    Other,
    StaleSession,
    StaleConnection,
    StaleSubscriber,
    ControllerLeaseConflict,
    RoleConflict,
    AccessDenied,
    OwnershipConflict,
    RuntimeNotReady,
}

/// Error type for async transport operations
#[derive(Debug)]
pub enum AsyncTransportError {
    /// Failed to spawn the agent process
    SpawnFailed(std::io::Error),
    /// Failed to send message to agent
    SendFailed(String),
    /// Failed to parse message from agent
    ParseFailed(String),
    /// Agent process exited unexpectedly
    ProcessExited(Option<i32>),
    /// Communication channel closed
    ChannelClosed,
    /// A governed tool approval already received a controller decision.
    ToolDecisionAlreadySent(String),
    /// Operation timed out
    Timeout,
    /// Operation was cancelled
    Cancelled,
    /// Remote HTTP/SSE transport error
    Remote(String),
    /// Remote HTTP/SSE transport error with structured status metadata
    RemoteStatus {
        status: u16,
        message: String,
        retryable: bool,
        kind: RemoteErrorKind,
    },
}

const MAX_CONSECUTIVE_PARSE_ERRORS: usize = 5;

impl std::fmt::Display for AsyncTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncTransportError::SpawnFailed(e) => write!(f, "Failed to spawn agent: {e}"),
            AsyncTransportError::SendFailed(e) => write!(f, "Failed to send to agent: {e}"),
            AsyncTransportError::ParseFailed(e) => {
                write!(f, "Failed to parse agent message: {e}")
            }
            AsyncTransportError::ProcessExited(code) => {
                write!(f, "Agent process exited with code: {code:?}")
            }
            AsyncTransportError::ChannelClosed => write!(f, "Communication channel closed"),
            AsyncTransportError::ToolDecisionAlreadySent(execution_id) => {
                write!(
                    f,
                    "Tool execution {execution_id} already received a decision"
                )
            }
            AsyncTransportError::Timeout => write!(f, "Operation timed out"),
            AsyncTransportError::Cancelled => write!(f, "Operation was cancelled"),
            AsyncTransportError::Remote(e) => write!(f, "Remote transport error: {e}"),
            AsyncTransportError::RemoteStatus { message, .. } => {
                write!(f, "Remote transport error: {message}")
            }
        }
    }
}

impl std::error::Error for AsyncTransportError {}

impl AsyncTransportError {
    /// Whether the failed transport operation should be retried automatically.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RemoteStatus { retryable, .. } => *retryable,
            Self::ToolDecisionAlreadySent(_) => false,
            _ => true,
        }
    }

    /// Whether this error should consume the dedicated stale remote-reference
    /// retry budget used for transient session/connection/subscriber misses.
    #[must_use]
    pub fn uses_stale_reference_retry_budget(&self) -> bool {
        matches!(
            self,
            Self::RemoteStatus {
                kind: RemoteErrorKind::StaleSession
                    | RemoteErrorKind::StaleConnection
                    | RemoteErrorKind::StaleSubscriber,
                ..
            }
        )
    }
}

/// Handle for async communication with the agent process
pub struct AsyncAgentTransport {
    /// Sender for outgoing messages
    message_tx: mpsc::UnboundedSender<ToAgentMessage>,
    /// Receiver for incoming raw protocol messages
    event_rx: mpsc::UnboundedReceiver<Result<FromAgentMessage, AsyncTransportError>>,
    /// Wake listeners when the incoming queue receives a transport event.
    event_notification: Arc<Notify>,
    /// Current agent state
    state: AgentState,
    /// Governed approval calls which already received a controller decision.
    decided_tool_executions: Mutex<HashSet<String>>,
    /// Cancellation token for graceful shutdown
    cancel_token: CancellationToken,
    /// Handle to the reader task
    _reader_handle: tokio::task::JoinHandle<()>,
    /// Handle to the writer task
    _writer_handle: tokio::task::JoinHandle<()>,
}

impl AsyncAgentTransport {
    /// Spawn a new agent process and connect to it
    pub async fn spawn(config: AsyncTransportConfig) -> Result<Self, AsyncTransportError> {
        let mut cmd = Command::new(&config.cli_path);
        cmd.arg("--headless")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(cwd);
        }

        for arg in &config.extra_args {
            cmd.arg(arg);
        }

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut child = spawn_agent_command(&mut cmd)
            .await
            .map_err(AsyncTransportError::SpawnFailed)?;

        let stdin = child.stdin.take().ok_or_else(|| {
            AsyncTransportError::SpawnFailed(std::io::Error::other("Failed to get stdin"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AsyncTransportError::SpawnFailed(std::io::Error::other("Failed to get stdout"))
        })?;

        // Channels
        let (message_tx, message_rx) = mpsc::unbounded_channel::<ToAgentMessage>();
        let (event_tx, event_rx) =
            mpsc::unbounded_channel::<Result<FromAgentMessage, AsyncTransportError>>();
        let event_notification = Arc::new(Notify::new());

        let cancel_token = CancellationToken::new();

        // Spawn writer task
        let writer_cancel = cancel_token.clone();
        let writer_event_tx = event_tx.clone();
        let writer_event_notification = Arc::clone(&event_notification);
        let writer_handle = tokio::spawn(async move {
            Self::writer_loop(
                stdin,
                message_rx,
                writer_event_tx,
                writer_event_notification,
                writer_cancel,
            )
            .await;
        });

        // Spawn reader task
        let reader_cancel = cancel_token.clone();
        let reader_event_notification = Arc::clone(&event_notification);
        let reader_handle = tokio::spawn(async move {
            Self::reader_loop(
                stdout,
                child,
                event_tx,
                reader_event_notification,
                reader_cancel,
                config.buffer_size,
                config.read_timeout,
            )
            .await;
        });

        let transport = Self {
            message_tx,
            event_rx,
            event_notification,
            state: AgentState::default(),
            decided_tool_executions: Mutex::new(HashSet::new()),
            cancel_token,
            _reader_handle: reader_handle,
            _writer_handle: writer_handle,
        };
        transport.send(ToAgentMessage::Hello {
            protocol_version: Some(super::HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: Some(ClientInfo {
                name: "maestro-tui-rs".to_string(),
                version: option_env!("CARGO_PKG_VERSION").map(str::to_string),
            }),
            capabilities: Some(local_controller_capabilities()),
            role: Some(ConnectionRole::Controller),
            opt_out_notifications: None,
            controller_binding: config.controller_binding.clone(),
        })?;
        Ok(transport)
    }

    /// Writer loop - sends messages to agent stdin
    async fn writer_loop(
        mut stdin: tokio::process::ChildStdin,
        mut rx: mpsc::UnboundedReceiver<ToAgentMessage>,
        error_tx: mpsc::UnboundedSender<Result<FromAgentMessage, AsyncTransportError>>,
        event_notification: Arc<Notify>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            let json = match serde_json::to_string(&msg) {
                                Ok(j) => j,
                                Err(e) => {
                                    let _ = super::send_transport_event(
                                        &error_tx,
                                        &event_notification,
                                        Err(AsyncTransportError::ParseFailed(e.to_string())),
                                    );
                                    continue;
                                }
                            };

                            if let Err(e) = stdin.write_all(json.as_bytes()).await {
                                let _ = super::send_transport_event(
                                    &error_tx,
                                    &event_notification,
                                    Err(AsyncTransportError::SendFailed(e.to_string())),
                                );
                                break;
                            }

                            if let Err(e) = stdin.write_all(b"\n").await {
                                let _ = super::send_transport_event(
                                    &error_tx,
                                    &event_notification,
                                    Err(AsyncTransportError::SendFailed(e.to_string())),
                                );
                                break;
                            }

                            if let Err(e) = stdin.flush().await {
                                let _ = super::send_transport_event(
                                    &error_tx,
                                    &event_notification,
                                    Err(AsyncTransportError::SendFailed(e.to_string())),
                                );
                                break;
                            }
                        }
                        None => break, // Channel closed
                    }
                }
            }
        }
    }

    /// Reader loop - reads messages from agent stdout
    async fn reader_loop(
        stdout: tokio::process::ChildStdout,
        mut child: Child,
        tx: mpsc::UnboundedSender<Result<FromAgentMessage, AsyncTransportError>>,
        event_notification: Arc<Notify>,
        cancel: CancellationToken,
        buffer_size: usize,
        read_timeout: Option<Duration>,
    ) {
        let reader = BufReader::with_capacity(buffer_size, stdout);
        let mut lines = reader.lines();
        let mut parse_error_streak = 0;
        let mut should_kill = false;

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    should_kill = true;
                    break;
                },
                line_result = async {
                    if let Some(timeout_duration) = read_timeout {
                        match timeout(timeout_duration, lines.next_line()).await {
                            Ok(res) => res.map_err(|e| AsyncTransportError::SendFailed(e.to_string())),
                            Err(_) => Err(AsyncTransportError::Timeout),
                        }
                    } else {
                        lines
                            .next_line()
                            .await
                            .map_err(|e| AsyncTransportError::SendFailed(e.to_string()))
                    }
                } => {
                    match line_result {
                        Ok(Some(line)) if line.trim().is_empty() => continue,
                        Ok(Some(line)) => {
                            match serde_json::from_str::<FromAgentMessage>(&line) {
                                Ok(msg) => {
                                    parse_error_streak = 0;
                                    if !super::send_transport_event(
                                        &tx,
                                        &event_notification,
                                        Ok(msg),
                                    ) {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    // Log but continue - don't break on parse errors
                                    eprintln!("Parse error: {} - {}", e, &line[..line.len().min(100)]);
                                    parse_error_streak += 1;
                                    if parse_error_streak >= MAX_CONSECUTIVE_PARSE_ERRORS {
                                        let _ = super::send_transport_event(
                                            &tx,
                                            &event_notification,
                                            Err(AsyncTransportError::ParseFailed(e.to_string())),
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            // EOF - process closed stdout
                            break;
                        }
                        Err(e) => {
                            let is_timeout = matches!(e, AsyncTransportError::Timeout);
                            let _ = super::send_transport_event(&tx, &event_notification, Err(e));
                            if is_timeout {
                                should_kill = true;
                            }
                            break;
                        }
                    }
                }
            }
        }

        if should_kill {
            let _ = child.kill().await;
        }

        // Process ended, get exit code (avoid hanging if child is stuck)
        let code = match timeout(Duration::from_secs(1), child.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => None,
        };
        let _ = super::send_transport_event(
            &tx,
            &event_notification,
            Err(AsyncTransportError::ProcessExited(code)),
        );
    }

    pub(crate) fn event_notification(&self) -> Arc<Notify> {
        Arc::clone(&self.event_notification)
    }

    /// Send a message to the agent
    pub fn send(&self, msg: ToAgentMessage) -> Result<(), AsyncTransportError> {
        self.message_tx
            .send(msg)
            .map_err(|_| AsyncTransportError::ChannelClosed)
    }

    /// Send a user prompt
    pub fn prompt(&self, content: impl Into<String>) -> Result<(), AsyncTransportError> {
        self.send(ToAgentMessage::Prompt {
            content: content.into(),
            attachments: None,
            managed_inference_authorization: None,
        })
    }

    /// Configure the agent before sending prompts
    pub fn init(&self, config: InitConfig) -> Result<(), AsyncTransportError> {
        let message = match (config.code_mode, config.tool_grant) {
            (Some(code_mode), Some(tool_grant)) => ToAgentMessage::GovernedInit {
                system_prompt: config.system_prompt,
                append_system_prompt: config.append_system_prompt,
                thinking_level: config.thinking_level,
                approval_mode: config.approval_mode,
                history: config.history,
                code_mode,
                tool_grant,
            },
            _ => ToAgentMessage::Init {
                system_prompt: config.system_prompt,
                append_system_prompt: config.append_system_prompt,
                thinking_level: config.thinking_level,
                approval_mode: config.approval_mode,
                history: config.history,
            },
        };
        self.send(message)
    }

    /// Send a prompt with file attachments
    pub fn prompt_with_attachments(
        &self,
        content: impl Into<String>,
        attachments: Vec<String>,
    ) -> Result<(), AsyncTransportError> {
        self.send(ToAgentMessage::Prompt {
            content: content.into(),
            attachments: Some(attachments),
            managed_inference_authorization: None,
        })
    }

    /// Interrupt the current operation
    pub fn interrupt(&self) -> Result<(), AsyncTransportError> {
        self.send(ToAgentMessage::Interrupt)
    }

    /// Cancel the current operation
    pub fn cancel(&self) -> Result<(), AsyncTransportError> {
        self.send(ToAgentMessage::Cancel)
    }

    /// Approve a tool call
    pub fn approve_tool(&self, call_id: impl Into<String>) -> Result<(), AsyncTransportError> {
        self.respond_to_tool(call_id.into(), true)
    }

    /// Deny a tool call
    pub fn deny_tool(&self, call_id: impl Into<String>) -> Result<(), AsyncTransportError> {
        self.respond_to_tool(call_id.into(), false)
    }

    fn respond_to_tool(&self, call_id: String, approved: bool) -> Result<(), AsyncTransportError> {
        let pending = self
            .state
            .pending_approvals
            .iter()
            .find(|approval| approval.call_id == call_id);
        let tool_execution_id = pending.and_then(|approval| approval.tool_execution_id.clone());
        let reserved_execution_id = tool_execution_id.clone();

        if let Some(execution_id) = &tool_execution_id {
            let mut decided = self
                .decided_tool_executions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !decided.insert(execution_id.clone()) {
                return Err(AsyncTransportError::ToolDecisionAlreadySent(
                    execution_id.clone(),
                ));
            }
        }

        let result = self.send(ToAgentMessage::ToolResponse {
            call_id: call_id.clone(),
            tool_execution_id,
            approved,
            result: None,
        });
        if result.is_err() {
            if let Some(execution_id) = &reserved_execution_id {
                self.decided_tool_executions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(execution_id);
            }
        }
        result
    }

    /// Shut down the agent
    pub fn shutdown(&self) -> Result<(), AsyncTransportError> {
        let result = self.send(ToAgentMessage::Shutdown);
        self.decided_tool_executions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.cancel_token.cancel();
        result
    }

    /// Shut down the agent and wait for both transport tasks and the child.
    pub async fn shutdown_and_wait(self) -> Result<(), AsyncTransportError> {
        let shutdown_result = self.shutdown();
        let Self {
            _reader_handle: reader_handle,
            _writer_handle: writer_handle,
            ..
        } = self;
        let (reader_result, writer_result) = tokio::join!(reader_handle, writer_handle);
        reader_result.map_err(|error| {
            AsyncTransportError::SendFailed(format!("agent reader task failed to join: {error}"))
        })?;
        writer_result.map_err(|error| {
            AsyncTransportError::SendFailed(format!("agent writer task failed to join: {error}"))
        })?;
        shutdown_result
    }

    /// Try to receive an event without blocking
    pub fn try_recv(&mut self) -> Option<Result<AgentEvent, AsyncTransportError>> {
        loop {
            let result = self.try_recv_message()?;
            match self.apply_transport_result(result) {
                Ok(Some(event)) => return Some(Ok(event)),
                Ok(None) => continue,
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// Receive an event, blocking until one is available
    pub async fn recv(&mut self) -> Result<AgentEvent, AsyncTransportError> {
        loop {
            let result = self.recv_message().await?;
            match self.apply_transport_result(Ok(result))? {
                Some(event) => return Ok(event),
                None => continue,
            }
        }
    }

    pub(super) fn try_recv_message(
        &mut self,
    ) -> Option<Result<FromAgentMessage, AsyncTransportError>> {
        match self.event_rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.decided_tool_executions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                Some(Err(AsyncTransportError::ChannelClosed))
            }
        }
    }

    pub(super) async fn recv_message(&mut self) -> Result<FromAgentMessage, AsyncTransportError> {
        match self.event_rx.recv().await {
            Some(result) => result,
            None => {
                self.decided_tool_executions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                Err(AsyncTransportError::ChannelClosed)
            }
        }
    }

    /// Receive an event with a timeout
    pub async fn recv_timeout(
        &mut self,
        duration: Duration,
    ) -> Result<AgentEvent, AsyncTransportError> {
        let deadline = Instant::now() + duration;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(AsyncTransportError::Timeout)?;
            let result = match timeout(remaining, self.event_rx.recv()).await {
                Ok(Some(result)) => result,
                Ok(None) => {
                    self.decided_tool_executions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clear();
                    return Err(AsyncTransportError::ChannelClosed);
                }
                Err(_) => return Err(AsyncTransportError::Timeout),
            };
            match self.apply_transport_result(result)? {
                Some(event) => return Ok(event),
                None => continue,
            }
        }
    }

    fn apply_transport_result(
        &mut self,
        result: Result<FromAgentMessage, AsyncTransportError>,
    ) -> Result<Option<AgentEvent>, AsyncTransportError> {
        match result {
            Ok(message) => {
                if let FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
                    tool_execution_id: Some(execution_id),
                    ..
                }) = &message
                {
                    self.decided_tool_executions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(execution_id);
                }
                Ok(self.state.handle_message(message))
            }
            Err(error) => {
                self.decided_tool_executions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                Err(error)
            }
        }
    }

    /// Get a reference to the current agent state
    #[must_use]
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Check if the agent is ready
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state.is_ready
    }

    /// Check if the agent is currently responding
    #[must_use]
    pub fn is_responding(&self) -> bool {
        self.state.is_responding
    }

    /// Get the model name
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.state.model.as_deref()
    }

    /// Get the provider name
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.state.provider.as_deref()
    }

    /// Get the cancellation token for external cancellation
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

/// Builder for creating an `AsyncAgentTransport`
pub struct AsyncAgentTransportBuilder {
    config: AsyncTransportConfig,
}

impl AsyncAgentTransportBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: AsyncTransportConfig::default(),
        }
    }

    /// Set the CLI path
    pub fn cli_path(mut self, path: impl Into<String>) -> Self {
        self.config.cli_path = path.into();
        self
    }

    /// Set the working directory
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.config.cwd = Some(cwd.into());
        self
    }

    /// Add an extra argument
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.config.extra_args.push(arg.into());
        self
    }

    /// Add environment variable
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.push((key.into(), value.into()));
        self
    }

    /// Set read timeout
    #[must_use]
    pub fn read_timeout(mut self, duration: Duration) -> Self {
        self.config.read_timeout = Some(duration);
        self
    }

    /// Set buffer size
    #[must_use]
    pub fn buffer_size(mut self, size: usize) -> Self {
        self.config.buffer_size = size;
        self
    }

    /// Build and spawn the transport
    pub async fn spawn(self) -> Result<AsyncAgentTransport, AsyncTransportError> {
        AsyncAgentTransport::spawn(self.config).await
    }
}

impl Default for AsyncAgentTransportBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::headless::ServerRequestType;

    use super::super::messages::FromAgentMessage;
    use super::*;

    #[test]
    fn async_config_defaults() {
        let config = AsyncTransportConfig::default();
        assert_eq!(config.cli_path, "maestro");
        assert!(config.cwd.is_none());
        assert!(config.extra_args.is_empty());
        assert!(config.read_timeout.is_none());
        assert_eq!(config.buffer_size, 1024 * 1024);
    }

    #[test]
    fn async_builder_sets_options() {
        let builder = AsyncAgentTransportBuilder::new()
            .cli_path("/usr/bin/composer")
            .cwd("/home/user/project")
            .arg("--model")
            .arg("claude-3-opus")
            .env("API_KEY", "secret")
            .read_timeout(Duration::from_secs(30))
            .buffer_size(2 * 1024 * 1024);

        assert_eq!(builder.config.cli_path, "/usr/bin/composer");
        assert_eq!(builder.config.cwd, Some("/home/user/project".to_string()));
        assert_eq!(builder.config.extra_args.len(), 2);
        assert_eq!(builder.config.env.len(), 1);
        assert_eq!(builder.config.read_timeout, Some(Duration::from_secs(30)));
        assert_eq!(builder.config.buffer_size, 2 * 1024 * 1024);
    }

    #[test]
    fn error_display() {
        let err = AsyncTransportError::SpawnFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(err.to_string().contains("spawn"));

        let err = AsyncTransportError::Timeout;
        assert!(err.to_string().contains("timed out"));

        let err = AsyncTransportError::ProcessExited(Some(1));
        assert!(err.to_string().contains("exited"));
    }

    #[test]
    fn local_controller_capabilities_include_interactive_server_requests() {
        assert_eq!(
            local_controller_capabilities().server_requests,
            Some(vec![
                ServerRequestType::Approval,
                ServerRequestType::UserInput,
                ServerRequestType::ToolRetry,
            ])
        );
        assert_eq!(
            local_controller_capabilities().transcript_grade,
            Some(crate::transcript::TranscriptGrade::Delta)
        );
    }

    #[tokio::test]
    async fn tool_responses_forward_pending_execution_id() {
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let (_event_tx, event_rx) =
            mpsc::unbounded_channel::<Result<FromAgentMessage, AsyncTransportError>>();
        let mut state = AgentState::default();
        state
            .pending_approvals
            .push(crate::headless::messages::PendingApproval {
                call_id: "call-1".to_string(),
                tool_execution_id: Some("execution-1".to_string()),
                request_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({}),
                started_at_ms: None,
            });
        let mut transport = AsyncAgentTransport {
            message_tx,
            event_rx,
            event_notification: Arc::new(Notify::new()),
            state,
            decided_tool_executions: Mutex::new(HashSet::new()),
            cancel_token: CancellationToken::new(),
            _reader_handle: tokio::spawn(async {}),
            _writer_handle: tokio::spawn(async {}),
        };

        transport.deny_tool("call-1").expect("deny tool");

        assert!(matches!(
            message_rx.recv().await.expect("tool response"),
            ToAgentMessage::ToolResponse {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                approved: false,
                result: None,
            } if call_id == "call-1" && tool_execution_id == "execution-1"
        ));

        assert!(
            transport.approve_tool("call-1").is_err(),
            "a second decision for one governed approval must be rejected"
        );
        assert!(
            message_rx.try_recv().is_err(),
            "a rejected duplicate decision must not be sent"
        );

        transport
            .apply_transport_result(Ok(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
                call_id: "call-1".to_string(),
                tool_execution_id: Some("execution-1".to_string()),
                success: false,
                tool: Some("bash".to_string()),
                details: None,
                receipt: None,
            })))
            .expect("terminal event");
        transport
            .state
            .pending_approvals
            .push(crate::headless::messages::PendingApproval {
                call_id: "call-1".to_string(),
                tool_execution_id: Some("execution-2".to_string()),
                request_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({}),
                started_at_ms: None,
            });
        transport
            .approve_tool("call-1")
            .expect("a new execution may reuse the call ID");
        assert!(matches!(
            message_rx.recv().await.expect("new tool response"),
            ToAgentMessage::ToolResponse {
                tool_execution_id: Some(tool_execution_id),
                ..
            } if tool_execution_id == "execution-2"
        ));
    }

    #[tokio::test]
    async fn approve_tool_forwards_pending_execution_id() {
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let (_event_tx, event_rx) =
            mpsc::unbounded_channel::<Result<FromAgentMessage, AsyncTransportError>>();
        let mut state = AgentState::default();
        state
            .pending_approvals
            .push(crate::headless::messages::PendingApproval {
                call_id: "call-1".to_string(),
                tool_execution_id: Some("execution-1".to_string()),
                request_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({}),
                started_at_ms: None,
            });
        let mut transport = AsyncAgentTransport {
            message_tx,
            event_rx,
            event_notification: Arc::new(Notify::new()),
            state,
            decided_tool_executions: Mutex::new(HashSet::new()),
            cancel_token: CancellationToken::new(),
            _reader_handle: tokio::spawn(async {}),
            _writer_handle: tokio::spawn(async {}),
        };

        transport.approve_tool("call-1").expect("approve tool");

        assert!(matches!(
            message_rx.recv().await.expect("tool response"),
            ToAgentMessage::ToolResponse {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                approved: true,
                result: None,
            } if call_id == "call-1" && tool_execution_id == "execution-1"
        ));

        assert!(
            transport.deny_tool("call-1").is_err(),
            "a second decision for one governed approval must be rejected"
        );
        assert!(
            message_rx.try_recv().is_err(),
            "a rejected duplicate decision must not be sent"
        );

        transport
            .state
            .pending_approvals
            .push(crate::headless::messages::PendingApproval {
                call_id: "legacy-call".to_string(),
                tool_execution_id: None,
                request_id: None,
                tool: "read".to_string(),
                args: serde_json::json!({}),
                started_at_ms: None,
            });
        transport
            .approve_tool("legacy-call")
            .expect("legacy approval without an execution ID");
        transport
            .deny_tool("legacy-call")
            .expect("legacy retry without an execution ID");
        for approved in [true, false] {
            assert!(matches!(
                message_rx.recv().await.expect("legacy tool response"),
                ToAgentMessage::ToolResponse {
                    tool_execution_id: None,
                    approved: actual,
                    ..
                } if actual == approved
            ));
        }
    }

    #[tokio::test]
    async fn failed_tool_response_releases_execution_reservation() {
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        drop(message_rx);
        let (_event_tx, event_rx) =
            mpsc::unbounded_channel::<Result<FromAgentMessage, AsyncTransportError>>();
        let mut state = AgentState::default();
        state
            .pending_approvals
            .push(crate::headless::messages::PendingApproval {
                call_id: "call-1".to_string(),
                tool_execution_id: Some("execution-1".to_string()),
                request_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({}),
                started_at_ms: None,
            });
        let transport = AsyncAgentTransport {
            message_tx,
            event_rx,
            event_notification: Arc::new(Notify::new()),
            state,
            decided_tool_executions: Mutex::new(HashSet::new()),
            cancel_token: CancellationToken::new(),
            _reader_handle: tokio::spawn(async {}),
            _writer_handle: tokio::spawn(async {}),
        };

        assert!(transport.approve_tool("call-1").is_err());
        assert!(
            transport.decided_tool_executions.lock().unwrap().is_empty(),
            "failed enqueue must release its reservation"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn spawn_retries_while_agent_executable_is_temporarily_busy() {
        use std::fs::{self, OpenOptions};
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("agent.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod script");
        let writer = OpenOptions::new()
            .write(true)
            .open(&script)
            .expect("open writer");
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            drop(writer);
        });

        let mut command = Command::new(&script);
        let mut child = spawn_agent_command(&mut command)
            .await
            .expect("spawn after writer closes");
        assert!(child.wait().await.expect("wait for script").success());
        release.await.expect("release task");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn spawned_transport_notifies_on_ready_event() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("ready.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"ready\",\"model\":\"test\",\"provider\":\"test\"}'\nsleep 1\n",
        )
        .expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod script");

        let mut transport = AsyncAgentTransport::spawn(AsyncTransportConfig {
            cli_path: script.to_string_lossy().into_owned(),
            ..AsyncTransportConfig::default()
        })
        .await
        .expect("spawn scripted transport");
        let notification = transport.event_notification();

        tokio::time::timeout(Duration::from_secs(1), notification.notified())
            .await
            .expect("ready event should notify transport listeners");
        assert!(matches!(
            transport.try_recv(),
            Some(Ok(AgentEvent::Ready { model, provider, .. }))
                if model == "test" && provider == "test"
        ));

        transport
            .shutdown_and_wait()
            .await
            .expect("shutdown scripted transport");
    }

    #[tokio::test]
    async fn try_recv_skips_messages_without_events() {
        let (_message_tx, message_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) =
            mpsc::unbounded_channel::<Result<FromAgentMessage, AsyncTransportError>>();
        event_tx
            .send(Ok(FromAgentMessage::Ready {
                protocol_version: Some("2026-03-30".to_string()),
                model: "test".to_string(),
                provider: "test".to_string(),
                session_id: None,
            }))
            .expect("send ready");
        event_tx
            .send(Ok(FromAgentMessage::Status {
                message: "ready".to_string(),
            }))
            .expect("send status");

        let cancel_token = CancellationToken::new();
        let noop = tokio::spawn(async {});
        let mut transport = AsyncAgentTransport {
            message_tx: _message_tx,
            event_rx,
            event_notification: Arc::new(Notify::new()),
            state: AgentState::default(),
            decided_tool_executions: Mutex::new(HashSet::new()),
            cancel_token,
            _reader_handle: noop,
            _writer_handle: tokio::spawn(async move {
                let _ = message_rx;
            }),
        };

        let event = transport.try_recv().expect("ready event").expect("ok");
        assert!(matches!(event, AgentEvent::Ready { .. }));

        let event = transport.try_recv().expect("status event").expect("ok");
        assert!(matches!(event, AgentEvent::Status { ref message } if message == "ready"));
    }
}
