//! MCP Client Implementation
//!
//! This module provides the client for communicating with MCP servers.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::managed_setup::{McpDecision, McpPolicy};

use super::config::{
    McpServerConfig, McpTransport, expand_env_vars_for_scope, server_requires_workspace_approval,
};
use super::http::HttpConnection;
use super::notifications::{MAX_POLL_NOTIFICATIONS, NotificationQueue, notification_channel};
use super::pending::{PendingRequestGuard, PendingResponses};
use super::protocol::{
    ClientInfo, InitializeResult, McpIncomingMessage, McpNotification, McpPrompt, McpRequest,
    McpResource, McpResponse, McpTool, McpToolAnnotations, McpToolFingerprint, McpToolResult,
    PromptGetResult, PromptsListResult, ResourceReadResult, ResourcesListResult, ToolsListResult,
    cap_tool_result_bytes, contains_unsafe_instructions, contains_unsafe_schema_metadata,
    sanitize_tool_description, validate_mcp_name,
};

async fn await_stdio_delivery_or_cancellation<F>(
    delivery: F,
    cancel: &CancellationToken,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::pin!(delivery);
    let cancellation = cancel.cancelled();
    tokio::pin!(cancellation);

    enum InitialPoll<T> {
        Cancelled,
        Completed(T),
        Started,
    }

    let initial = poll_fn(|cx| {
        if cancellation.as_mut().poll(cx).is_ready() {
            return Poll::Ready(InitialPoll::Cancelled);
        }

        Poll::Ready(match delivery.as_mut().poll(cx) {
            Poll::Ready(result) => InitialPoll::Completed(result),
            Poll::Pending => InitialPoll::Started,
        })
    })
    .await;

    match initial {
        InitialPoll::Cancelled => return None,
        InitialPoll::Completed(result) => return Some(result),
        InitialPoll::Started => {}
    }

    tokio::select! {
        biased;
        result = &mut delivery => Some(result),
        () = cancel.cancelled() => None,
    }
}

/// Error type for MCP operations
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// Server not found
    #[error("MCP server not found: {0}")]
    ServerNotFound(String),

    /// Connection failed
    #[error("Failed to connect to MCP server: {0}")]
    ConnectionFailed(String),

    /// Request failed
    #[error("MCP request failed: {0}")]
    RequestFailed(String),

    /// Tool not found
    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    /// Timeout
    #[error("MCP operation timed out")]
    Timeout,

    /// Request was cancelled by the client.
    #[error("MCP operation cancelled")]
    Cancelled,

    /// A dispatched request may have completed remotely, but its terminal
    /// outcome could not be observed.
    #[error("MCP remote outcome is indeterminate: {0}")]
    Indeterminate(String),

    /// Protocol error
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Compatibility envelope returned by a managed Computer API capability
/// probe. This is deliberately separate from the MCP tool catalog: a server
/// must prove the API contract before Maestro dispatches a mutating launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct McpApiCapabilities {
    pub api_version: String,
    pub minimum_client_version: String,
    pub features: Vec<String>,
    pub contract_digest: String,
}

/// Runtime notification surfaced from an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub enum McpRuntimeEvent {
    ToolsListChanged {
        server: String,
    },
    ResourcesListChanged {
        server: String,
    },
    PromptsListChanged {
        server: String,
    },
    Progress {
        server: String,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
    Log {
        server: String,
        level: String,
        logger: Option<String>,
        data: serde_json::Value,
    },
    /// A tool already admitted from this server came back with a different
    /// input schema, or failed an admission check, and was withdrawn from the
    /// model-facing tool set.
    ToolRevoked {
        server: String,
        tool: String,
        reason: String,
    },
}

impl McpRuntimeEvent {
    #[must_use]
    pub fn changes_tools(&self) -> bool {
        matches!(
            self,
            Self::ToolsListChanged { .. } | Self::ToolRevoked { .. }
        )
    }

    #[must_use]
    pub fn affects_badges(&self) -> bool {
        self.changes_tools()
    }
}

/// Connection backend type
#[allow(clippy::large_enum_variant)]
enum ConnectionBackend {
    /// Stdio subprocess
    Stdio {
        process: Child,
        stdin: tokio::process::ChildStdin,
        notification_rx: NotificationQueue,
        stdout_reader: Option<AbortOnDropHandle<()>>,
    },
    /// HTTP/SSE connection
    Http(HttpConnection),
}

/// Connection to a single MCP server
pub struct McpConnection {
    /// Server name
    name: String,
    /// Server configuration
    config: McpServerConfig,
    /// Connection backend
    backend: Option<ConnectionBackend>,
    /// Request ID counter (for stdio)
    next_id: AtomicU64,
    /// Pending requests (for stdio)
    pending: PendingResponses,
    /// Available tools
    tools: Vec<McpTool>,
    /// Available resources
    resources: Vec<McpResource>,
    /// Available prompts
    prompts: Vec<McpPrompt>,
    /// Whether initialized
    initialized: bool,

    /// Workspace used to re-read the trust decision from global config.
    /// Repository-controlled MCP configuration cannot set this value.
    workspace_dir: Option<PathBuf>,

    /// Whether a reconnect is currently in progress
    ///
    /// Used to avoid overlapping reconnect attempts.
    reconnecting: bool,

    /// Input-schema fingerprint recorded the first time each tool name was
    /// admitted from this server. A later `tools/list` that changes the
    /// schema of an already-seen name is a rug pull, not an update.
    tool_fingerprints: HashMap<String, McpToolFingerprint>,

    /// Tool names withdrawn by admission, mapped to the reason. Entries are
    /// cleared only by [`McpConnection::reapprove_tool`].
    revoked_tools: std::collections::BTreeMap<String, String>,
}

impl McpConnection {
    /// Create a new connection (not yet connected)
    #[must_use]
    pub fn new(config: McpServerConfig) -> Self {
        Self::new_with_workspace(config, None)
    }

    fn new_with_workspace(config: McpServerConfig, workspace_dir: Option<&Path>) -> Self {
        Self {
            name: config.name.clone(),
            config,
            backend: None,
            next_id: AtomicU64::new(1),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            initialized: false,
            workspace_dir: workspace_dir.map(Path::to_path_buf),
            reconnecting: false,
            tool_fingerprints: HashMap::new(),
            revoked_tools: std::collections::BTreeMap::new(),
        }
    }

    /// Get the server name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Connect to the MCP server
    pub async fn connect(&mut self) -> Result<(), McpError> {
        // The server name becomes part of the `mcp__<server>__<tool>` dispatch
        // name and is used as a map key, so reject the shapes that are unsafe
        // as an identifier before any process is spawned.
        if let Err(reason) = validate_mcp_name(&self.name) {
            return Err(McpError::ConnectionFailed(format!(
                "MCP server name rejected: {reason}"
            )));
        }
        self.ensure_workspace_trust().await?;
        match self.config.transport {
            McpTransport::Stdio => self.connect_stdio().await,
            McpTransport::Http | McpTransport::Sse => self.connect_http().await,
        }
    }

    /// Connect via HTTP/SSE transport
    async fn connect_http(&mut self) -> Result<(), McpError> {
        self.ensure_workspace_trust().await?;
        let mut http_conn =
            HttpConnection::new_with_workspace(self.config.clone(), self.workspace_dir.as_deref())?;
        http_conn.connect().await?;

        // HTTP/SSE tool catalogs are untrusted at initial connection just as
        // they are on later list-changed notifications. Admit the initial
        // catalog before exposing it or recording its schema baseline.
        let listed = http_conn.tools().to_vec();
        let _ = self.admit_tools(listed);
        self.resources = http_conn.resources().to_vec();
        self.prompts = http_conn.prompts().to_vec();
        self.initialized = true;
        self.backend = Some(ConnectionBackend::Http(http_conn));

        Ok(())
    }

    /// Connect via stdio transport
    async fn connect_stdio(&mut self) -> Result<(), McpError> {
        self.ensure_workspace_trust().await?;
        let command = self.config.command.as_ref().ok_or_else(|| {
            McpError::ConnectionFailed("No command specified for stdio transport".to_string())
        })?;

        // Expand environment variables in command and args
        let command = expand_env_vars_for_scope(command, self.config.scope);
        let args: Vec<String> = self
            .config
            .args
            .iter()
            .map(|a| expand_env_vars_for_scope(a, self.config.scope))
            .collect();

        // Build command
        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        // Set working directory
        if let Some(cwd) = &self.config.cwd {
            cmd.current_dir(expand_env_vars_for_scope(cwd, self.config.scope));
        }

        // Set environment variables (expand values)
        for (key, value) in &self.config.env {
            cmd.env(key, expand_env_vars_for_scope(value, self.config.scope));
        }

        // Don't inherit all env vars for security (only essential ones)
        cmd.env_clear();
        for key in [
            "PATH",
            "HOME",
            "USER",
            "SHELL",
            "TERM",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "TEMP",
            "TMP",
            "COMSPEC",
            "PATHEXT",
        ] {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        if std::env::var("HOME").is_err() {
            if let Some(home) = dirs::home_dir()
                .and_then(|path| path.to_str().map(std::string::ToString::to_string))
            {
                cmd.env("HOME", home);
            }
        }
        // Re-add configured env vars after clearing
        for (key, value) in &self.config.env {
            cmd.env(key, expand_env_vars_for_scope(value, self.config.scope));
        }

        // Spawn the process
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::ConnectionFailed(format!("Failed to spawn {command}: {e}")))?;

        // Take stdin/stdout
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::ConnectionFailed("Failed to get stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::ConnectionFailed("Failed to get stdout".to_string()))?;

        // Set up response reader
        let (notification_tx, notification_rx) = notification_channel();
        let pending = self.pending.clone();

        // Spawn stdout reader task
        let stdout_reader = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if let Ok(message) = serde_json::from_str::<McpIncomingMessage>(&line) {
                            match message {
                                McpIncomingMessage::Response(response) => {
                                    if let Some(id) = response.id {
                                        let mut pending = pending.lock().unwrap();
                                        if let Some(sender) = pending.remove(&id) {
                                            let _ = sender.send(response);
                                            continue;
                                        }
                                    }
                                }
                                McpIncomingMessage::Notification(notification) => {
                                    let _ = notification_tx.send(notification);
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }));

        self.backend = Some(ConnectionBackend::Stdio {
            process: child,
            stdin,
            notification_rx,
            stdout_reader: Some(stdout_reader),
        });

        // Initialize the connection
        self.initialize().await?;

        Ok(())
    }

    /// Initialize the MCP connection
    async fn initialize(&mut self) -> Result<(), McpError> {
        let request = McpRequest::initialize(self.next_id(), &ClientInfo::default());
        let response = self.send_request(request).await?;

        let _init_result: InitializeResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid initialize response: {e}")))?;

        // Send initialized notification
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        self.send_raw(&notification).await?;

        // List available tools
        self.refresh_tools().await?;
        // List resources (best effort)
        let _ = self.refresh_resources().await;
        // List prompts (best effort)
        let _ = self.refresh_prompts().await;

        self.initialized = true;
        Ok(())
    }

    /// Refresh the list of available tools.
    ///
    /// The listed tools are not trusted: they are run through
    /// [`Self::admit_tools`] before they become the model-facing tool set.
    pub async fn refresh_tools(&mut self) -> Result<(), McpError> {
        self.refresh_tools_reporting_revocations().await?;
        Ok(())
    }

    /// `refresh_tools`, returning the tool names this refresh withdrew.
    async fn refresh_tools_reporting_revocations(
        &mut self,
    ) -> Result<Vec<(String, String)>, McpError> {
        self.ensure_workspace_trust().await?;
        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            http.refresh_tools().await?;
            let listed = http.tools().to_vec();
            return Ok(self.admit_tools(listed));
        }

        let request = McpRequest::list_tools(self.next_id());
        let response = self.send_request(request).await?;

        let tools_result: ToolsListResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid tools/list response: {e}")))?;

        Ok(self.admit_tools(tools_result.tools))
    }

    /// Decide which listed tools become model-facing, recording the input
    /// schema fingerprint of each name the first time it is admitted.
    ///
    /// A tool is withdrawn when its name is unusable as an identifier, when
    /// its description or schema carries a prompt-injection marker, or when
    /// its input schema differs from the fingerprint recorded for that name.
    /// Withdrawal is sticky: the name stays out of the tool set until
    /// [`Self::reapprove_tool`] clears it, so a server cannot restore a
    /// swapped tool by listing the original schema again on the next poll.
    ///
    /// Returns the names withdrawn by this call, each with its reason.
    fn admit_tools(&mut self, listed: Vec<McpTool>) -> Vec<(String, String)> {
        let mut admitted = Vec::with_capacity(listed.len());
        let mut newly_revoked = Vec::new();

        for mut tool in listed {
            if self
                .config
                .disabled_tools
                .iter()
                .any(|disabled| disabled == &tool.name)
            {
                continue;
            }
            let reason = self.admission_reason(&tool);
            if let Some(reason) = reason {
                if self.revoked_tools.get(&tool.name) != Some(&reason) {
                    newly_revoked.push((tool.name.clone(), reason.clone()));
                }
                self.revoked_tools.insert(tool.name.clone(), reason);
                continue;
            }
            if self.revoked_tools.contains_key(&tool.name) {
                continue;
            }
            self.tool_fingerprints
                .entry(tool.name.clone())
                .or_insert_with(|| McpToolFingerprint::of(&tool));
            tool.description = tool
                .description
                .as_deref()
                .and_then(sanitize_tool_description);
            admitted.push(tool);
        }

        self.tools = admitted;
        newly_revoked
    }

    /// `Some(reason)` when a listed tool must not be admitted.
    fn admission_reason(&self, tool: &McpTool) -> Option<String> {
        if let Err(reason) = validate_mcp_name(&tool.name) {
            return Some(format!("invalid tool name: {reason}"));
        }
        if let Some(description) = tool.description.as_deref() {
            if contains_unsafe_instructions(description) {
                return Some("description contains injected instructions".to_string());
            }
        }
        if let Some(schema) = tool.input_schema.as_ref() {
            if contains_unsafe_schema_metadata(schema) {
                return Some("input schema contains injected instructions".to_string());
            }
        }
        if let Some(known) = self.tool_fingerprints.get(&tool.name) {
            let current = McpToolFingerprint::of(tool);
            if current.schema_sha256 != known.schema_sha256 {
                return Some(format!(
                    "input schema changed after approval (was {}, now {})",
                    &known.hex()[..16],
                    &current.hex()[..16]
                ));
            }
        }
        None
    }

    /// Tool names currently withdrawn from this server, with the reason.
    #[must_use]
    pub fn revoked_tools(&self) -> &std::collections::BTreeMap<String, String> {
        &self.revoked_tools
    }

    /// Fingerprint recorded for an admitted tool name, if any.
    #[must_use]
    pub fn tool_fingerprint(&self, name: &str) -> Option<&McpToolFingerprint> {
        self.tool_fingerprints.get(name)
    }

    /// Accept the server's current definition of a withdrawn tool.
    ///
    /// This is the re-approval step: the recorded fingerprint is dropped so
    /// the next `tools/list` re-admits the tool under its new schema. Call it
    /// only after a human has seen the new definition.
    pub fn reapprove_tool(&mut self, name: &str) {
        self.revoked_tools.remove(name);
        self.tool_fingerprints.remove(name);
    }

    /// Refresh the list of available resources
    pub async fn refresh_resources(&mut self) -> Result<(), McpError> {
        self.ensure_workspace_trust().await?;
        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            http.refresh_resources().await?;
            self.resources = http.resources().to_vec();
            return Ok(());
        }

        let request = McpRequest::list_resources(self.next_id());
        let response = self.send_request(request).await?;

        let resources_result: ResourcesListResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid resources/list response: {e}")))?;

        self.resources = resources_result.resources;
        Ok(())
    }

    /// Refresh the list of available prompts
    pub async fn refresh_prompts(&mut self) -> Result<(), McpError> {
        self.ensure_workspace_trust().await?;
        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            http.refresh_prompts().await?;
            self.prompts = http.prompts().to_vec();
            return Ok(());
        }

        let request = McpRequest::list_prompts(self.next_id());
        let response = self.send_request(request).await?;

        let prompts_result: PromptsListResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid prompts/list response: {e}")))?;

        self.prompts = prompts_result.prompts;
        Ok(())
    }

    /// Drain pending server notifications, refresh cached lists when needed, and surface runtime events.
    pub async fn poll_notifications(&mut self) -> Result<Vec<McpRuntimeEvent>, McpError> {
        self.ensure_workspace_trust().await?;
        if self.config.transport == McpTransport::Stdio && self.initialized {
            self.ensure_stdio_connected().await?;
        }

        let server = self.server_name().to_string();
        let mut events = Vec::new();

        // A chatty server may refill the queue while metadata refresh awaits.
        // Bound one poll as well as the transport queue itself.
        for _ in 0..MAX_POLL_NOTIFICATIONS {
            let Some(notification) = self.try_recv_notification() else {
                break;
            };
            if notification.is_tools_list_changed() {
                let revoked = self.refresh_tools_reporting_revocations().await?;
                events.push(McpRuntimeEvent::ToolsListChanged {
                    server: server.clone(),
                });
                for (tool, reason) in revoked {
                    events.push(McpRuntimeEvent::ToolRevoked {
                        server: server.clone(),
                        tool,
                        reason,
                    });
                }
            } else if notification.is_resources_list_changed() {
                self.refresh_resources().await?;
                events.push(McpRuntimeEvent::ResourcesListChanged {
                    server: server.clone(),
                });
            } else if notification.is_prompts_list_changed() {
                self.refresh_prompts().await?;
                events.push(McpRuntimeEvent::PromptsListChanged {
                    server: server.clone(),
                });
            } else if let Some(params) = notification.progress_params() {
                events.push(McpRuntimeEvent::Progress {
                    server: server.clone(),
                    progress: params.progress,
                    total: params.total,
                    message: params.message,
                });
            } else if let Some(params) = notification.log_message_params() {
                events.push(McpRuntimeEvent::Log {
                    server: server.clone(),
                    level: params.level,
                    logger: params.logger,
                    data: params.data,
                });
            }
        }

        Ok(events)
    }

    /// Get available tools
    pub fn tools(&self) -> &[McpTool] {
        if self.workspace_trusted_now() {
            &self.tools
        } else {
            &[]
        }
    }

    /// Fetch the connected HTTP server's API compatibility envelope.
    pub(crate) async fn fetch_api_capabilities(&mut self) -> Result<McpApiCapabilities, McpError> {
        self.ensure_workspace_trust().await?;
        match &mut self.backend {
            Some(ConnectionBackend::Http(http)) => http.fetch_api_capabilities().await,
            Some(ConnectionBackend::Stdio { .. }) | None => Err(McpError::ConnectionFailed(
                "Computer API capability negotiation requires an HTTP connection".to_string(),
            )),
        }
    }

    /// Get available resources
    pub fn resources(&self) -> &[McpResource] {
        if self.workspace_trusted_now() {
            &self.resources
        } else {
            &[]
        }
    }

    /// Get available prompts
    pub fn prompts(&self) -> &[McpPrompt] {
        if self.workspace_trusted_now() {
            &self.prompts
        } else {
            &[]
        }
    }

    /// Call a tool
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        self.ensure_workspace_trust().await?;
        // Ensure stdio transport is alive before using cached tools list.
        self.ensure_stdio_connected().await?;

        // Verify tool exists
        if !self.tools.iter().any(|t| t.name == tool_name) {
            return Err(McpError::ToolNotFound(tool_name.to_string()));
        }

        // Delegate to HTTP backend if using HTTP/SSE
        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            return http.call_tool(tool_name, arguments).await;
        }

        let request = McpRequest::call_tool(self.next_id(), tool_name, arguments);
        let response = self.send_request(request).await?;

        if let Some(error) = response.error {
            return Err(McpError::RequestFailed(error.message));
        }

        let mut result: McpToolResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid tool result: {e}")))?;
        cap_tool_result_bytes(&mut result);
        Ok(result)
    }

    /// Call a tool and notify the MCP server if the client cancels it.
    pub async fn call_tool_cancellable(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<McpToolResult, McpError> {
        self.ensure_workspace_trust().await?;
        self.ensure_stdio_connected_cancellable(cancel).await?;
        if !self.tools.iter().any(|tool| tool.name == tool_name) {
            return Err(McpError::ToolNotFound(tool_name.to_string()));
        }

        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            return http
                .call_tool_cancellable(tool_name, arguments, cancel)
                .await;
        }

        let request = McpRequest::call_tool(self.next_id(), tool_name, arguments);
        let response = self.send_request_cancellable(request, cancel).await?;
        if let Some(error) = response.error {
            return Err(McpError::RequestFailed(error.message));
        }
        let mut result: McpToolResult = response
            .result_as()
            .map_err(|error| McpError::Protocol(format!("Invalid tool result: {error}")))?;
        cap_tool_result_bytes(&mut result);
        Ok(result)
    }

    /// Read a resource by URI
    pub async fn read_resource(&mut self, uri: &str) -> Result<ResourceReadResult, McpError> {
        self.ensure_workspace_trust().await?;
        self.ensure_stdio_connected().await?;

        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            return http.read_resource(uri).await;
        }

        let request = McpRequest::read_resource(self.next_id(), uri);
        let response = self.send_request(request).await?;

        if let Some(error) = response.error {
            return Err(McpError::RequestFailed(error.message));
        }

        let result: ResourceReadResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid resource read result: {e}")))?;

        Ok(result)
    }

    /// Get a prompt by name
    pub async fn get_prompt(
        &mut self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<PromptGetResult, McpError> {
        self.ensure_workspace_trust().await?;
        self.ensure_stdio_connected().await?;

        if let Some(ConnectionBackend::Http(ref mut http)) = self.backend {
            return http.get_prompt(name, arguments).await;
        }

        let request = McpRequest::get_prompt(self.next_id(), name, arguments);
        let response = self.send_request(request).await?;

        if let Some(error) = response.error {
            return Err(McpError::RequestFailed(error.message));
        }

        let result: PromptGetResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid prompt get result: {e}")))?;

        Ok(result)
    }

    /// Ensure stdio transport is connected, with a single auto-reconnect if the process died.
    async fn ensure_stdio_connected(&mut self) -> Result<(), McpError> {
        self.ensure_workspace_trust().await?;
        if self.config.transport != McpTransport::Stdio {
            return Ok(());
        }

        // If not initialized or backend missing, connect fresh.
        if !self.initialized || !matches!(self.backend, Some(ConnectionBackend::Stdio { .. })) {
            return self.connect_stdio().await;
        }

        // Check child is still alive, reconnect once if not.
        let exited = if let Some(ConnectionBackend::Stdio { process, .. }) = &mut self.backend {
            matches!(process.try_wait(), Ok(Some(_)))
        } else {
            false
        };

        if exited {
            if self.reconnecting {
                return Err(McpError::ConnectionFailed(
                    "MCP stdio server exited while reconnecting".to_string(),
                ));
            }
            self.reconnecting = true;
            self.disconnect().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let result = self.connect_stdio().await;
            self.reconnecting = false;
            return result;
        }

        Ok(())
    }

    /// Re-read the workspace trust decision before every connection, spawn,
    /// reconnect, and network-facing metadata/tool operation. The repository
    /// can provide the path used for this lookup, but only global config can
    /// authorize it and revocation is observed without restarting the client.
    async fn ensure_workspace_trust(&mut self) -> Result<(), McpError> {
        if !self.workspace_trusted_now() {
            self.disconnect().await;
            return Err(McpError::ConnectionFailed(format!(
                "MCP server \"{}\" requires workspace trust approval; set projects.\"<workspace>\".trust_level = \"trusted\" in global config (~/.composer/config.toml) to enable it",
                self.name
            )));
        }
        Ok(())
    }

    fn workspace_trusted_now(&self) -> bool {
        !server_requires_workspace_approval(&self.config)
            || self.workspace_dir.as_deref().is_some_and(|workspace_dir| {
                crate::config::workspace_trusted_in_global_config(workspace_dir)
            })
    }

    async fn ensure_stdio_connected_cancellable(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<(), McpError> {
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }

        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.ensure_stdio_connected() => Some(result),
        };
        match result {
            Some(result) => result,
            None => {
                if self.config.transport == McpTransport::Stdio && !self.initialized {
                    self.disconnect().await;
                    self.pending.lock().unwrap().clear();
                    self.reconnecting = false;
                }
                Err(McpError::Cancelled)
            }
        }
    }

    /// Send a request and wait for response (stdio only)
    async fn send_request(&mut self, request: McpRequest) -> Result<McpResponse, McpError> {
        let id = request.id;

        // Set up response channel
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _pending_request = PendingRequestGuard::register(&self.pending, id, tx);

        // Send request
        if let Err(send_err) = self.send_raw(&request).await {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(&id);
            return Err(send_err);
        }

        // Wait for response with timeout
        let timeout = Duration::from_millis(self.config.timeout.unwrap_or(30_000));
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(McpError::Protocol("Response channel closed".to_string())),
            Err(_) => {
                // Remove from pending
                let mut pending = self.pending.lock().unwrap();
                pending.remove(&id);
                Err(McpError::Timeout)
            }
        }
    }

    /// Send a stdio request and propagate client cancellation to the server.
    async fn send_request_cancellable(
        &mut self,
        request: McpRequest,
        cancel: &CancellationToken,
    ) -> Result<McpResponse, McpError> {
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }

        let id = request.id;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _pending_request = PendingRequestGuard::register(&self.pending, id, tx);

        let send_result =
            match await_stdio_delivery_or_cancellation(self.send_raw(&request), cancel).await {
                Some(result) => result,
                None => {
                    self.pending.lock().unwrap().remove(&id);
                    // The write future may have already emitted a partial JSON
                    // frame. A cancellation notification cannot repair that
                    // stream, so close it and force a clean reconnect.
                    self.disconnect().await;
                    return Err(McpError::Cancelled);
                }
            };
        if let Err(send_err) = send_result {
            self.pending.lock().unwrap().remove(&id);
            return Err(send_err);
        }

        let timeout = Duration::from_millis(self.config.timeout.unwrap_or(30_000));
        tokio::select! {
            biased;
            response = tokio::time::timeout(timeout, rx) => match response {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_)) => Err(McpError::Protocol("Response channel closed".to_string())),
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    Err(McpError::Timeout)
                }
            },
            () = cancel.cancelled() => {
                self.pending.lock().unwrap().remove(&id);
                let notification =
                    McpNotification::cancelled(id, "Deixic Code turn cancelled");
                let delivery = tokio::time::timeout(
                    Duration::from_millis(500),
                    self.send_raw(&notification),
                )
                .await;
                match delivery {
                    Ok(Ok(())) => Err(McpError::Indeterminate(
                        "Cancellation notification was acknowledged, but the remote request outcome is unknown"
                            .to_string(),
                    )),
                    Ok(Err(error)) => {
                        self.disconnect().await;
                        Err(McpError::Indeterminate(format!(
                            "Failed to deliver cancellation notification: {error}"
                        )))
                    }
                    Err(_) => {
                        self.disconnect().await;
                        Err(McpError::Indeterminate(
                            "Timed out delivering cancellation notification".to_string(),
                        ))
                    }
                }
            }
        }
    }

    /// Send raw JSON to the server (stdio only)
    async fn send_raw(&mut self, value: &impl serde::Serialize) -> Result<(), McpError> {
        match &mut self.backend {
            Some(ConnectionBackend::Stdio { process, stdin, .. }) => {
                if let Ok(Some(status)) = process.try_wait() {
                    self.initialized = false;
                    return Err(McpError::ConnectionFailed(format!(
                        "MCP stdio server exited: {status}"
                    )));
                }

                let json = serde_json::to_string(value)?;
                if let Err(e) = stdin.write_all(json.as_bytes()).await {
                    self.initialized = false;
                    return Err(McpError::ConnectionFailed(format!(
                        "Failed to write to MCP stdio stdin: {e}"
                    )));
                }
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
                Ok(())
            }
            _ => Err(McpError::ConnectionFailed(
                "Not connected via stdio".to_string(),
            )),
        }
    }

    /// Get next request ID
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Try to receive a server notification (non-blocking)
    ///
    /// Returns any pending notifications from the server that weren't
    /// responses to specific requests (e.g., progress updates, log messages).
    pub fn try_recv_notification(&mut self) -> Option<McpNotification> {
        if !self.workspace_trusted_now() {
            return None;
        }
        match &mut self.backend {
            Some(ConnectionBackend::Stdio {
                notification_rx, ..
            }) => notification_rx.try_recv().ok(),
            Some(ConnectionBackend::Http(http)) => http.try_recv_notification(),
            None => None,
        }
    }

    /// Disconnect from the server
    pub async fn disconnect(&mut self) {
        match self.backend.take() {
            Some(ConnectionBackend::Stdio {
                mut process,
                stdout_reader,
                ..
            }) => {
                // Descendants can inherit stdout and keep it open after the
                // direct child exits. Stop our reader independently of EOF.
                if let Some(reader) = stdout_reader {
                    reader.abort();
                    let _ = reader.await;
                }
                let _ = process.kill().await;
            }
            Some(ConnectionBackend::Http(mut http)) => {
                http.disconnect().await;
            }
            None => {}
        }
        self.initialized = false;
        self.tools.clear();
        self.resources.clear();
        self.prompts.clear();
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        if !self.initialized {
            return false;
        }
        match &self.backend {
            Some(ConnectionBackend::Stdio { .. }) => true,
            Some(ConnectionBackend::Http(http)) => http.is_connected(),
            None => false,
        }
    }

    /// Get the server name for this connection
    pub fn server_name(&self) -> &str {
        match &self.backend {
            Some(ConnectionBackend::Http(http)) => http.name(),
            _ => &self.name,
        }
    }
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        // Try to kill the process synchronously
        if let Some(ConnectionBackend::Stdio { mut process, .. }) = self.backend.take() {
            let _ = process.start_kill();
        }
    }
}

/// The organization's MCP server policy, applied before any connection is
/// dialed. `None` means no administrator has expressed an opinion.
#[derive(Debug, Clone, Default)]
pub struct ManagedMcpPolicy {
    /// The policy document version, so a refusal can name the policy that
    /// produced it.
    pub version: u64,
    /// The decision table.
    pub policy: McpPolicy,
}

/// MCP Client managing multiple server connections
pub struct McpClient {
    /// Active connections
    connections: RwLock<HashMap<String, Arc<Mutex<McpConnection>>>>,
    /// The organization policy that admits or refuses a server before it is
    /// dialed. Absent until a managed setup document is resolved.
    managed_policy: RwLock<Option<ManagedMcpPolicy>>,
}

impl McpClient {
    /// Create a new MCP client
    #[must_use]
    pub fn new() -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            managed_policy: RwLock::new(None),
        }
    }

    /// Install the organization's MCP policy. Called once at session start
    /// after the managed setup document is resolved.
    pub async fn set_managed_policy(&self, policy: Option<ManagedMcpPolicy>) {
        *self.managed_policy.write().await = policy;
    }

    /// Apply the organization policy to one server configuration.
    ///
    /// The refusal names the policy version so an operator can tell which
    /// revision of the organization's configuration blocked the connection.
    async fn enforce_managed_policy(&self, config: &McpServerConfig) -> Result<(), McpError> {
        let guard = self.managed_policy.read().await;
        let Some(managed) = guard.as_ref() else {
            return Ok(());
        };
        let transport = match config.transport {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
        };
        match managed
            .policy
            .decide(&config.name, config.url.as_deref(), transport)
        {
            McpDecision::Allowed => Ok(()),
            McpDecision::RefusedNotAllowlisted => Err(McpError::ConnectionFailed(format!(
                "MCP server `{}` is not on your organization's allowlist                  (Deixic managed setup version {}). Ask an administrator to add it.",
                config.name, managed.version
            ))),
            McpDecision::RefusedDenylisted => Err(McpError::ConnectionFailed(format!(
                "MCP server `{}` is blocked by your organization's denylist                  (Deixic managed setup version {}).",
                config.name, managed.version
            ))),
        }
    }

    /// Connect to an MCP server
    pub async fn connect(&self, config: McpServerConfig) -> Result<(), McpError> {
        self.connect_with_workspace_trust(config, None).await
    }

    /// Connect to an MCP server using a workspace path whose trust decision is
    /// read from global configuration. Project/local configuration never
    /// supplies this decision itself; the connection re-reads it at every
    /// reconnect and network-facing operation so revocation takes effect.
    ///
    /// The organization's MCP policy is applied here, before the transport is
    /// opened, so a refused server is never dialed at all.
    pub async fn connect_with_workspace_trust(
        &self,
        config: McpServerConfig,
        workspace_dir: Option<&Path>,
    ) -> Result<(), McpError> {
        self.enforce_managed_policy(&config).await?;
        let name = config.name.clone();
        let mut connection = McpConnection::new_with_workspace(config, workspace_dir);
        connection.connect().await?;

        let mut connections = self.connections.write().await;
        connections.insert(name, Arc::new(Mutex::new(connection)));

        Ok(())
    }

    /// Disconnect from a server
    pub async fn disconnect(&self, name: &str) -> Result<(), McpError> {
        let mut connections = self.connections.write().await;
        if let Some(conn) = connections.remove(name) {
            let mut conn = conn.lock().await;
            conn.disconnect().await;
        }
        Ok(())
    }

    /// Drain pending server notifications across all active connections.
    pub async fn poll_notifications(&self) -> Result<Vec<McpRuntimeEvent>, McpError> {
        let connections = {
            let guard = self.connections.read().await;
            guard.values().cloned().collect::<Vec<_>>()
        };
        let mut events = Vec::new();

        for conn in connections {
            let mut conn = conn.lock().await;
            events.extend(conn.poll_notifications().await?);
        }

        Ok(events)
    }

    /// Disconnect from all servers
    pub async fn disconnect_all(&self) {
        let mut connections = self.connections.write().await;
        for (_, conn) in connections.drain() {
            let mut conn = conn.lock().await;
            conn.disconnect().await;
        }
    }

    /// Get all available tools from all connected servers
    pub async fn list_all_tools(&self) -> Vec<crate::ai::Tool> {
        let connections = self.connections.read().await;
        let mut tools = Vec::new();

        for (name, conn) in connections.iter() {
            let mut conn = conn.lock().await;
            if conn.ensure_workspace_trust().await.is_err() {
                continue;
            }
            for tool in conn.tools() {
                tools.push(tool.to_tool(name));
            }
        }

        tools
    }

    /// Get tool names grouped by server
    pub async fn list_tools_by_server(&self) -> Vec<(String, Vec<String>)> {
        let connections = self.connections.read().await;
        let mut results = Vec::new();

        for (name, conn) in connections.iter() {
            let mut conn = conn.lock().await;
            if conn.ensure_workspace_trust().await.is_err() {
                continue;
            }
            let tools = conn
                .tools()
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>();
            results.push((name.clone(), tools));
        }

        results
    }

    /// Fetch the API compatibility envelope for one connected server.
    pub(crate) async fn api_capabilities_for_server(
        &self,
        server_name: &str,
    ) -> Result<McpApiCapabilities, McpError> {
        let connections = self.connections.read().await;
        let conn = connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;
        let mut conn = conn.lock().await;
        conn.fetch_api_capabilities().await
    }

    /// Get tool annotations for all connected servers
    pub async fn list_tool_annotations(&self) -> HashMap<String, McpToolAnnotations> {
        let connections = self.connections.read().await;
        let mut annotations = HashMap::new();

        for (name, conn) in connections.iter() {
            let mut conn = conn.lock().await;
            if conn.ensure_workspace_trust().await.is_err() {
                continue;
            }
            for tool in conn.tools() {
                if let Some(meta) = tool.annotations.clone() {
                    let prefixed = tool.to_tool(name).name;
                    annotations.insert(prefixed, meta);
                }
            }
        }

        annotations
    }

    /// Get available resources from all connected servers
    pub async fn list_all_resources(&self) -> Vec<(String, Vec<String>)> {
        let connections = self.connections.read().await;
        let mut results = Vec::new();

        for (name, conn) in connections.iter() {
            let mut conn = conn.lock().await;
            if conn.ensure_workspace_trust().await.is_err() {
                continue;
            }
            let resources = conn
                .resources()
                .iter()
                .map(|r| r.uri.clone())
                .collect::<Vec<_>>();
            results.push((name.clone(), resources));
        }

        results
    }

    /// Get available prompts from all connected servers
    pub async fn list_all_prompts(&self) -> Vec<(String, Vec<String>)> {
        let connections = self.connections.read().await;
        let mut results = Vec::new();

        for (name, conn) in connections.iter() {
            let mut conn = conn.lock().await;
            if conn.ensure_workspace_trust().await.is_err() {
                continue;
            }
            let prompts = conn
                .prompts()
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>();
            results.push((name.clone(), prompts));
        }

        results
    }

    /// Get detailed prompt metadata from all connected servers
    pub async fn list_all_prompt_details(&self) -> Vec<(String, Vec<McpPrompt>)> {
        let connections = self.connections.read().await;
        let mut results = Vec::new();

        for (name, conn) in connections.iter() {
            let mut conn = conn.lock().await;
            if conn.ensure_workspace_trust().await.is_err() {
                continue;
            }
            results.push((name.clone(), conn.prompts().to_vec()));
        }

        results
    }

    /// Get a prompt from a connected server
    pub async fn get_prompt(
        &self,
        server_name: &str,
        name: &str,
        arguments: Option<HashMap<String, String>>,
    ) -> Result<PromptGetResult, McpError> {
        let connections = self.connections.read().await;
        let conn = connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;
        let mut conn = conn.lock().await;
        let args_value = arguments
            .map(|args| serde_json::to_value(args).unwrap_or_else(|_| serde_json::json!({})));
        conn.get_prompt(name, args_value).await
    }

    /// Call a tool (parses server name from prefixed tool name)
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        let connections = self.connections.read().await;
        let (_, tool_name, conn) =
            Self::resolve_prefixed_tool_with_connections(prefixed_name, &connections)?;
        drop(connections);
        let mut conn = conn.lock().await;
        conn.call_tool(&tool_name, arguments).await
    }

    /// Call a tool and return resolved server/tool metadata for the same parse.
    pub async fn call_tool_with_metadata(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<(String, String, McpToolResult), McpError> {
        let connections = self.connections.read().await;
        let (server_name, tool_name, conn) =
            Self::resolve_prefixed_tool_with_connections(prefixed_name, &connections)?;
        drop(connections);
        let mut conn = conn.lock().await;
        let result = conn.call_tool(&tool_name, arguments).await?;
        Ok((server_name, tool_name, result))
    }

    /// Call a tool with metadata and propagate cancellation to its server.
    pub async fn call_tool_with_metadata_cancellable(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<(String, String, McpToolResult), McpError> {
        let connections = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(McpError::Cancelled),
            connections = self.connections.read() => connections,
        };
        let (server_name, tool_name, conn) =
            Self::resolve_prefixed_tool_with_connections(prefixed_name, &connections)?;
        drop(connections);
        let mut conn = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(McpError::Cancelled),
            conn = conn.lock() => conn,
        };
        let result = conn
            .call_tool_cancellable(&tool_name, arguments, cancel)
            .await?;
        Ok((server_name, tool_name, result))
    }

    /// Parse a prefixed MCP tool name into (server, tool) using known connections
    pub async fn parse_prefixed_name(
        &self,
        prefixed_name: &str,
    ) -> Result<(String, String), McpError> {
        let connections = self.connections.read().await;
        Self::parse_prefixed_name_with_connections(prefixed_name, &connections)
    }

    fn parse_prefixed_name_with_connections(
        prefixed_name: &str,
        connections: &HashMap<String, Arc<Mutex<McpConnection>>>,
    ) -> Result<(String, String), McpError> {
        if let Some(rest) = prefixed_name.strip_prefix("mcp__") {
            let parts: Vec<&str> = rest.split("__").collect();
            if parts.len() < 2 {
                return Err(McpError::ToolNotFound(format!(
                    "Invalid MCP tool name format: {prefixed_name}"
                )));
            }
            for idx in (1..parts.len()).rev() {
                let candidate = parts[..idx].join("__");
                if connections.contains_key(&candidate) {
                    return Ok((candidate, parts[idx..].join("__")));
                }
            }
            return Ok((parts[0].to_string(), parts[1..].join("__")));
        }

        if let Some(rest) = prefixed_name.strip_prefix("mcp_") {
            let parts: Vec<&str> = rest.split('_').collect();
            if parts.len() < 2 {
                return Err(McpError::ToolNotFound(format!(
                    "Invalid MCP tool name format: {prefixed_name}"
                )));
            }
            for idx in (1..parts.len()).rev() {
                let candidate = parts[..idx].join("_");
                if connections.contains_key(&candidate) {
                    return Ok((candidate, parts[idx..].join("_")));
                }
            }
            return Ok((parts[0].to_string(), parts[1..].join("_")));
        }

        Err(McpError::ToolNotFound(format!(
            "Invalid MCP tool name format: {prefixed_name}"
        )))
    }

    fn resolve_prefixed_tool_with_connections(
        prefixed_name: &str,
        connections: &HashMap<String, Arc<Mutex<McpConnection>>>,
    ) -> Result<(String, String, Arc<Mutex<McpConnection>>), McpError> {
        let (server_name, tool_name) =
            Self::parse_prefixed_name_with_connections(prefixed_name, connections)?;
        let conn = connections
            .get(&server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.clone()))?;
        Ok((server_name, tool_name, Arc::clone(conn)))
    }

    /// Check if a tool name is an MCP tool
    #[must_use]
    pub fn is_mcp_tool(name: &str) -> bool {
        if name == "mcp_list_resources"
            || name == "mcp_read_resource"
            || name == "mcp_list_prompts"
            || name == "mcp_get_prompt"
        {
            return false;
        }
        name.starts_with("mcp__") || name.starts_with("mcp_")
    }

    /// Read a resource from a connected server
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Result<ResourceReadResult, McpError> {
        let connections = self.connections.read().await;
        let conn = connections
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;
        let mut conn = conn.lock().await;
        conn.read_resource(uri).await
    }

    /// Get connected server names
    pub async fn connected_servers(&self) -> Vec<String> {
        let connections = {
            let connections = self.connections.read().await;
            connections
                .iter()
                .map(|(name, connection)| (name.clone(), Arc::clone(connection)))
                .collect::<Vec<_>>()
        };
        let mut connected = Vec::new();
        for (name, connection) in connections {
            let mut connection = connection.lock().await;
            if connection.ensure_workspace_trust().await.is_ok() && connection.is_connected() {
                connected.push(name);
            }
        }
        connected
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "client/catalog_tests.rs"]
mod tests;
