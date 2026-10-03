//! HTTP and SSE Transport for MCP
//!
//! This module provides Streamable HTTP and legacy HTTP/SSE transports for MCP
//! servers.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Client, StatusCode, header};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::auth::{ManagedMcpAuth, requires_hosted_orb_auth};
use super::client::{McpApiCapabilities, McpError};
use super::config::{
    McpServerConfig, McpTransport, expand_env_vars_for_scope, server_requires_workspace_approval,
};
use super::notifications::{NotificationQueue, notification_channel};
use super::pending::{PendingRequestGuard, PendingResponses};
use super::protocol::{
    ClientInfo, InitializeResult, MCP_PROTOCOL_VERSION, McpIncomingMessage, McpNotification,
    McpPrompt, McpRequest, McpResource, McpResponse, McpTool, McpToolResult, PromptGetResult,
    PromptsListResult, ResourceReadResult, ResourcesListResult, ToolsListResult,
    cap_tool_result_bytes,
};

const MCP_ACCEPT: &str = "application/json, text/event-stream";
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

fn is_read_only_discovery(method: &str) -> bool {
    matches!(
        method,
        "tools/list" | "resources/list" | "resources/read" | "prompts/list" | "prompts/get"
    )
}

fn repository_request_requires_workspace_trust(config: &McpServerConfig) -> bool {
    matches!(
        config.scope,
        crate::mcp::McpConfigScope::Project | crate::mcp::McpConfigScope::Local
    ) && server_requires_workspace_approval(config)
}

fn repository_workspace_is_trusted(config: &McpServerConfig, workspace_dir: Option<&Path>) -> bool {
    if !repository_request_requires_workspace_trust(config) {
        return true;
    }

    // Only the runtime-supplied workspace path can carry a global trust
    // decision. An absent path therefore never authorizes a repository
    // request, even if a lower-level caller bypasses McpConnection.
    workspace_dir.is_some_and(crate::config::workspace_trusted_in_global_config)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpTransportMode {
    Streamable,
    LegacyMessage,
}

#[derive(Debug)]
struct HttpRequestError {
    status: Option<StatusCode>,
    error: McpError,
}

impl HttpRequestError {
    fn new(status: Option<StatusCode>, error: McpError) -> Self {
        Self { status, error }
    }

    fn from_error(error: McpError) -> Self {
        Self::new(None, error)
    }

    fn can_fallback_to_legacy(&self) -> bool {
        matches!(
            self.status.map(|status| status.as_u16()),
            Some(400 | 404 | 405)
        )
    }

    fn into_error(self) -> McpError {
        self.error
    }
}

async fn await_request_or_cancellation<F>(
    request_future: F,
    cancel: &CancellationToken,
) -> Option<F::Output>
where
    F: std::future::Future,
{
    tokio::pin!(request_future);
    let cancellation = cancel.cancelled();
    tokio::pin!(cancellation);

    enum InitialPoll<T> {
        Cancelled,
        Completed(T),
        Dispatched,
    }

    let initial = poll_fn(|cx| {
        if cancellation.as_mut().poll(cx).is_ready() {
            return Poll::Ready(InitialPoll::Cancelled);
        }

        Poll::Ready(match request_future.as_mut().poll(cx) {
            Poll::Ready(result) => InitialPoll::Completed(result),
            Poll::Pending => InitialPoll::Dispatched,
        })
    })
    .await;

    match initial {
        InitialPoll::Cancelled => return None,
        InitialPoll::Completed(result) => return Some(result),
        InitialPoll::Dispatched => {}
    }

    tokio::select! {
        biased;
        result = &mut request_future => Some(result),
        () = cancel.cancelled() => None,
    }
}

/// HTTP-based MCP connection
pub struct HttpConnection {
    /// Server name
    name: String,
    /// Server configuration
    config: McpServerConfig,
    /// HTTP client
    client: Client,
    /// Provider-owned runtime authentication. This never enters `config`.
    auth: ManagedMcpAuth,
    /// Base URL for the server
    base_url: String,
    /// Request ID counter
    next_id: AtomicU64,
    /// Available tools
    tools: Vec<McpTool>,
    namespace_instructions: Option<String>,
    /// Available resources
    resources: Vec<McpResource>,
    /// Available prompts
    prompts: Vec<McpPrompt>,
    /// Whether initialized
    initialized: bool,
    /// Streamable HTTP or legacy `/message` endpoint mode
    transport_mode: HttpTransportMode,
    /// Session assigned by a Streamable HTTP server
    session_id: Option<String>,
    /// Negotiated MCP protocol version for Streamable HTTP requests
    protocol_version: Option<String>,
    /// SSE notification receiver (for SSE transport)
    notification_rx: Option<NotificationQueue>,
    /// Notification sender used by Streamable HTTP response streams
    notification_tx: NotificationQueue,
    /// Pending SSE requests
    pending_sse: PendingResponses,
    /// SSE task handle
    sse_task: Option<tokio::task::JoinHandle<()>>,
    /// Workspace used to re-read the trust decision from global config.
    /// This is never derived from repository-controlled MCP configuration.
    workspace_dir: Option<PathBuf>,
}

impl HttpConnection {
    /// Create a new HTTP connection
    #[cfg(test)]
    pub fn new(config: McpServerConfig) -> Result<Self, McpError> {
        Self::new_with_workspace(config, None)
    }

    /// Create a connection with a workspace path used for global trust checks.
    pub(super) fn new_with_workspace(
        config: McpServerConfig,
        workspace_dir: Option<&Path>,
    ) -> Result<Self, McpError> {
        Self::new_with_auth_and_workspace(config, ManagedMcpAuth::new(), workspace_dir)
    }

    #[cfg(test)]
    fn new_with_auth(config: McpServerConfig, auth: ManagedMcpAuth) -> Result<Self, McpError> {
        Self::new_with_auth_and_workspace(config, auth, None)
    }

    fn new_with_auth_and_workspace(
        config: McpServerConfig,
        auth: ManagedMcpAuth,
        workspace_dir: Option<&Path>,
    ) -> Result<Self, McpError> {
        let base_url = config.url.clone().ok_or_else(|| {
            McpError::ConnectionFailed("URL required for HTTP/SSE transport".to_string())
        })?;

        // Build HTTP client with timeout
        let timeout = Duration::from_millis(config.timeout.unwrap_or(30_000));
        let client = Client::builder().timeout(timeout).build().map_err(|e| {
            McpError::ConnectionFailed(format!("Failed to create HTTP client: {e}"))
        })?;
        let (notification_tx, notification_rx) = notification_channel();
        let transport_mode = if config.transport == McpTransport::Http {
            HttpTransportMode::Streamable
        } else {
            HttpTransportMode::LegacyMessage
        };

        Ok(Self {
            name: config.name.clone(),
            config,
            client,
            auth,
            base_url,
            next_id: AtomicU64::new(1),
            tools: Vec::new(),
            namespace_instructions: None,
            resources: Vec::new(),
            prompts: Vec::new(),
            initialized: false,
            transport_mode,
            session_id: None,
            protocol_version: None,
            notification_rx: Some(notification_rx),
            notification_tx,
            pending_sse: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sse_task: None,
            workspace_dir: workspace_dir.map(Path::to_path_buf),
        })
    }

    /// Connect and initialize
    pub async fn connect(&mut self) -> Result<(), McpError> {
        self.prepare_repository_client().await?;
        self.ensure_repository_request_allowed()?;
        if self.notification_rx.is_none() {
            let (notification_tx, notification_rx) = notification_channel();
            self.notification_tx = notification_tx;
            self.notification_rx = Some(notification_rx);
        }

        let result = match self.config.transport {
            McpTransport::Http => self.connect_http().await,
            McpTransport::Sse => self.connect_sse().await,
            McpTransport::Stdio => Err(McpError::ConnectionFailed(
                "Use McpConnection for stdio".to_string(),
            )),
        };
        if result.is_err() && self.session_id.is_some() {
            self.disconnect().await;
        }
        result
    }

    /// Repository-controlled HTTP/SSE endpoints are untrusted until the
    /// workspace is explicitly trusted in global config. Resolve and pin an
    /// untrusted endpoint before connecting, and disable redirects so a
    /// public first hop cannot pivot to loopback, private, or link-local
    /// infrastructure. Explicitly trusted local development keeps access to
    /// loopback/private endpoints but still does not follow redirects.
    async fn prepare_repository_client(&mut self) -> Result<(), McpError> {
        if !matches!(
            self.config.scope,
            crate::mcp::McpConfigScope::Project | crate::mcp::McpConfigScope::Local
        ) {
            return Ok(());
        }

        let url = reqwest::Url::parse(&self.base_url).map_err(|error| {
            McpError::ConnectionFailed(format!("Invalid MCP endpoint URL: {error}"))
        })?;
        let timeout = Duration::from_millis(self.config.timeout.unwrap_or(30_000));

        let workspace_trusted = self.workspace_dir.as_deref().is_some_and(|workspace_dir| {
            crate::config::workspace_trusted_in_global_config(workspace_dir)
        });
        if workspace_trusted {
            self.client = Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| {
                    McpError::ConnectionFailed(format!(
                        "Failed to create repository MCP HTTP client: {error}"
                    ))
                })?;
            return Ok(());
        }

        let address = crate::tools::net_guard::resolve_public_endpoint(&url)
            .await
            .map_err(McpError::ConnectionFailed)?;
        self.client = crate::tools::net_guard::pinned_client(
            &url,
            address,
            timeout,
            crate::tools::net_guard::DEFAULT_USER_AGENT,
        )
        .map_err(McpError::ConnectionFailed)?;
        Ok(())
    }

    /// Re-check repository trust at the final request boundary. The outer
    /// McpConnection check protects connection setup, while this check closes
    /// the window between client preparation and a later initialize, retry,
    /// reconnect, or request send when global trust is revoked.
    fn ensure_repository_request_allowed(&self) -> Result<(), McpError> {
        if !repository_workspace_is_trusted(&self.config, self.workspace_dir.as_deref()) {
            return Err(McpError::ConnectionFailed(format!(
                "MCP server \"{}\" requires workspace trust approval; set projects.\"<workspace>\".trust_level = \"trusted\" in global config (~/.composer/config.toml) to enable it",
                self.name
            )));
        }
        Ok(())
    }

    /// Connect via Streamable HTTP, falling back to legacy `/message` when safe.
    async fn connect_http(&mut self) -> Result<(), McpError> {
        self.ensure_repository_request_allowed()?;
        // Managed hosted Orb authentication is intentionally supported only by
        // the Streamable HTTP transport. The legacy request path applies
        // config headers, which deliberately excludes provider-owned auth, so
        // falling back there would turn an authenticated request into an
        // unauthenticated one.
        let allow_legacy_fallback = !requires_hosted_orb_auth(&self.config);
        self.initialize(allow_legacy_fallback).await?;
        Ok(())
    }

    /// Connect via SSE (persistent streaming connection)
    async fn connect_sse(&mut self) -> Result<(), McpError> {
        // Start SSE event stream
        let (tx, rx) = notification_channel();
        self.notification_tx = tx.clone();
        self.notification_rx = Some(rx);

        let url = format!("{}/sse", self.base_url.trim_end_matches('/'));
        let pending = self.pending_sse.clone();
        let client = self.client.clone();
        let headers = self.config.headers.clone();
        let scope = self.config.scope;
        let trust_config = self.config.clone();
        let workspace_dir = self.workspace_dir.clone();
        let bearer = self.bearer_token().await?;

        // Spawn SSE reader task
        let task = tokio::spawn(async move {
            let mut request = client.get(&url);

            // Add custom headers
            for (key, value) in &headers {
                request = request.header(key, expand_env_vars_for_scope(value, scope));
            }
            if let Some(access_token) = bearer.as_ref() {
                request = request.bearer_auth(access_token.as_str());
            }

            if repository_request_requires_workspace_trust(&trust_config)
                && !repository_workspace_is_trusted(&trust_config, workspace_dir.as_deref())
            {
                eprintln!(
                    "[mcp/sse] workspace trust was revoked before opening the repository endpoint"
                );
                return;
            }

            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[mcp/sse] Connection failed: {e}");
                    return;
                }
            };

            let mut stream = response.bytes_stream().eventsource();

            while let Some(event) = stream.next().await {
                match event {
                    Ok(ev) => {
                        if let Ok(message) = serde_json::from_str::<McpIncomingMessage>(&ev.data) {
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
                                    let _ = tx.send(notification);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[mcp/sse] Stream error: {e}");
                        break;
                    }
                }
            }
        });

        self.sse_task = Some(task);

        // Initialize the connection
        self.initialize(false).await?;
        Ok(())
    }

    /// Initialize the MCP connection
    async fn initialize(&mut self, allow_legacy_fallback: bool) -> Result<(), McpError> {
        self.ensure_repository_request_allowed()?;
        let request = McpRequest::initialize(self.next_id(), &ClientInfo::default());
        let response = if self.config.transport == McpTransport::Http
            && self.transport_mode == HttpTransportMode::Streamable
        {
            match self.send_streamable_request(request.clone()).await {
                Ok(response) => response,
                Err(error) if allow_legacy_fallback && error.can_fallback_to_legacy() => {
                    self.transport_mode = HttpTransportMode::LegacyMessage;
                    self.session_id = None;
                    self.protocol_version = None;
                    self.ensure_repository_request_allowed()?;
                    self.send_legacy_http_request(request).await?
                }
                Err(error) => return Err(error.into_error()),
            }
        } else {
            self.send_request(request).await?
        };

        let init_result: InitializeResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid initialize response: {e}")))?;
        if self.transport_mode == HttpTransportMode::Streamable {
            self.protocol_version = Some(init_result.protocol_version);
        }

        self.namespace_instructions = init_result
            .instructions
            .map(|text| text.chars().take(4096).collect());

        // Send initialized notification
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        self.send_notification(&notification).await?;

        // List available tools
        self.refresh_tools().await?;
        // List resources (best effort)
        let _ = self.refresh_resources().await;
        // List prompts (best effort)
        let _ = self.refresh_prompts().await;

        self.initialized = true;
        Ok(())
    }

    /// Refresh the list of available tools
    pub async fn refresh_tools(&mut self) -> Result<(), McpError> {
        let request = McpRequest::list_tools(self.next_id());
        let response = self.send_request(request).await?;

        let tools_result: ToolsListResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid tools/list response: {e}")))?;

        self.tools = tools_result.tools;
        Ok(())
    }

    /// Fetch the managed Computer API compatibility envelope.
    ///
    /// This is intentionally a separate authenticated REST request rather
    /// than an MCP tool call. A same-name MCP server must not be allowed to
    /// mutate state until the API version, required feature, and contract
    /// digest have been admitted by the client.
    pub(super) async fn fetch_api_capabilities(&mut self) -> Result<McpApiCapabilities, McpError> {
        self.ensure_repository_request_allowed()?;
        let mut url = reqwest::Url::parse(&self.base_url).map_err(|error| {
            McpError::ConnectionFailed(format!("Invalid Computer MCP endpoint URL: {error}"))
        })?;
        url.set_path("/api/capabilities");
        url.set_query(None);
        url.set_fragment(None);

        let mut request = self
            .apply_config_headers(self.client.get(url))
            .header(header::ACCEPT, "application/json");
        if let Some(access_token) = self.bearer_token().await? {
            request = request.bearer_auth(access_token.as_str());
        }
        self.ensure_repository_request_allowed()?;
        let response = request.send().await.map_err(|error| {
            McpError::RequestFailed(format!("Computer capability request failed: {error}"))
        })?;
        let status = response.status();
        if requires_hosted_orb_auth(&self.config)
            && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        {
            self.auth.invalidate();
        }
        if !status.is_success() {
            return Err(McpError::RequestFailed(format!(
                "Computer capability HTTP error: {status}"
            )));
        }
        response.json().await.map_err(|error| {
            McpError::Protocol(format!("Invalid Computer capability response: {error}"))
        })
    }

    /// Refresh the list of available resources
    pub async fn refresh_resources(&mut self) -> Result<(), McpError> {
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
        let request = McpRequest::list_prompts(self.next_id());
        let response = self.send_request(request).await?;

        let prompts_result: PromptsListResult = response
            .result_as()
            .map_err(|e| McpError::Protocol(format!("Invalid prompts/list response: {e}")))?;

        self.prompts = prompts_result.prompts;
        Ok(())
    }

    /// Bounded untrusted initialize guidance for explicit namespace discovery.
    pub fn instructions(&self) -> Option<&str> {
        self.namespace_instructions.as_deref()
    }

    /// Get available tools
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Get available resources
    pub fn resources(&self) -> &[McpResource] {
        &self.resources
    }

    /// Get available prompts
    pub fn prompts(&self) -> &[McpPrompt] {
        &self.prompts
    }

    /// Get server name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Try to receive a server notification (non-blocking).
    pub fn try_recv_notification(&mut self) -> Option<McpNotification> {
        self.notification_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok())
    }

    /// Call a tool
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        if !self.initialized {
            self.connect().await?;
        }
        // Verify tool exists
        if !self.tools.iter().any(|t| t.name == tool_name) {
            return Err(McpError::ToolNotFound(tool_name.to_string()));
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

    /// Call a tool and notify the server when the client cancels the request.
    pub async fn call_tool_cancellable(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<McpToolResult, McpError> {
        if !self.initialized {
            self.connect().await?;
        }
        if !self.tools.iter().any(|tool| tool.name == tool_name) {
            return Err(McpError::ToolNotFound(tool_name.to_string()));
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

    /// Send a request and wait for response
    async fn send_request(&mut self, request: McpRequest) -> Result<McpResponse, McpError> {
        match self.config.transport {
            McpTransport::Http => match self.transport_mode {
                HttpTransportMode::Streamable => self
                    .send_streamable_request(request)
                    .await
                    .map_err(HttpRequestError::into_error),
                HttpTransportMode::LegacyMessage => self.send_legacy_http_request(request).await,
            },
            McpTransport::Sse => self.send_sse_request(request).await,
            McpTransport::Stdio => Err(McpError::ConnectionFailed(
                "HttpConnection does not support stdio transport".to_string(),
            )),
        }
    }

    async fn send_request_cancellable(
        &mut self,
        request: McpRequest,
        cancel: &CancellationToken,
    ) -> Result<McpResponse, McpError> {
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }

        let request_id = request.id;
        if let Some(result) =
            await_request_or_cancellation(self.send_request(request), cancel).await
        {
            return result;
        }

        // A cancelled SSE waiter may already have inserted its response
        // channel; remove it before telling the server to stop work.
        self.pending_sse.lock().unwrap().remove(&request_id);
        let delivery = tokio::time::timeout(
            Duration::from_millis(500),
            self.send_notification(&McpNotification::cancelled(
                request_id,
                "Deixic Code turn cancelled",
            )),
        )
        .await;
        match delivery {
            Ok(Ok(())) => Err(McpError::Indeterminate(
                "Cancellation notification was acknowledged, but the remote request outcome is unknown"
                    .to_string(),
            )),
            Ok(Err(error)) => Err(McpError::Indeterminate(format!(
                "Failed to deliver cancellation notification: {error}"
            ))),
            Err(_) => Err(McpError::Indeterminate(
                "Timed out delivering cancellation notification".to_string(),
            )),
        }
    }

    fn apply_config_headers(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (key, value) in &self.config.headers {
            if requires_hosted_orb_auth(&self.config) && key.eq_ignore_ascii_case("authorization") {
                continue;
            }
            request = request.header(key, expand_env_vars_for_scope(value, self.config.scope));
        }
        request
    }

    async fn apply_streamable_headers(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, McpError> {
        let mut request = self
            .apply_config_headers(request)
            .header(header::ACCEPT, MCP_ACCEPT)
            .header(
                MCP_PROTOCOL_VERSION_HEADER,
                self.protocol_version
                    .as_deref()
                    .unwrap_or(MCP_PROTOCOL_VERSION),
            );
        if let Some(session_id) = self.session_id.as_deref() {
            request = request.header(MCP_SESSION_ID_HEADER, session_id);
        }
        if let Some(access_token) = self.bearer_token().await? {
            request = request.bearer_auth(access_token.as_str());
        }
        Ok(request)
    }

    async fn bearer_token(&self) -> Result<Option<zeroize::Zeroizing<String>>, McpError> {
        if requires_hosted_orb_auth(&self.config) {
            self.auth.bearer_for(&self.config)
        } else {
            super::oauth_bearer_for(&self.config).await
        }
    }

    async fn apply_authenticated_config_headers(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, McpError> {
        let mut request = self.apply_config_headers(request);
        if let Some(access_token) = self.bearer_token().await? {
            request = request.bearer_auth(access_token.as_str());
        }
        Ok(request)
    }

    /// Send a Streamable HTTP request to the configured MCP endpoint.
    async fn send_streamable_request(
        &mut self,
        request: McpRequest,
    ) -> Result<McpResponse, HttpRequestError> {
        let had_session = self.session_id.is_some();
        match self.send_streamable_request_once(&request).await {
            Err(error)
                if had_session
                    && request.method != "initialize"
                    && error.status == Some(StatusCode::NOT_FOUND) =>
            {
                self.invalidate_session();
                if is_read_only_discovery(&request.method) {
                    self.ensure_repository_request_allowed()
                        .map_err(HttpRequestError::from_error)?;
                    Box::pin(self.initialize(false))
                        .await
                        .map_err(HttpRequestError::from_error)?;
                    self.send_streamable_request_once(&request).await
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }

    fn invalidate_session(&mut self) {
        self.session_id = None;
        self.protocol_version = None;
        self.initialized = false;
    }

    async fn send_streamable_request_once(
        &mut self,
        request: &McpRequest,
    ) -> Result<McpResponse, HttpRequestError> {
        let req = self.client.post(&self.base_url).json(request);
        let request_builder = self
            .apply_streamable_headers(req)
            .await
            .map_err(HttpRequestError::from_error)?
            .header(header::CONTENT_TYPE, "application/json");
        self.ensure_repository_request_allowed()
            .map_err(HttpRequestError::from_error)?;
        let response = request_builder.send().await.map_err(|error| {
            HttpRequestError::from_error(McpError::RequestFailed(format!(
                "HTTP request failed: {error}"
            )))
        })?;

        let status = response.status();
        if requires_hosted_orb_auth(&self.config)
            && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        {
            self.auth.invalidate();
        }
        if !status.is_success() {
            return Err(HttpRequestError::new(
                Some(status),
                McpError::RequestFailed(format!("HTTP error: {status}")),
            ));
        }

        if request.method == "initialize" {
            if let Some(value) = response.headers().get(MCP_SESSION_ID_HEADER) {
                let session_id = value.to_str().map_err(|error| {
                    HttpRequestError::from_error(McpError::Protocol(format!(
                        "Invalid MCP session id header: {error}"
                    )))
                })?;
                if session_id.is_empty()
                    || !session_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                {
                    return Err(HttpRequestError::from_error(McpError::Protocol(
                        "Invalid MCP session id header: expected visible ASCII".to_string(),
                    )));
                }
                self.session_id = Some(session_id.to_owned());
            }
        }

        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.contains("text/event-stream") {
            return self
                .parse_streamable_event_stream(response, request.id)
                .await;
        }

        response.json().await.map_err(|error| {
            HttpRequestError::from_error(McpError::Protocol(format!(
                "Failed to parse response: {error}"
            )))
        })
    }

    async fn parse_streamable_event_stream(
        &self,
        response: reqwest::Response,
        request_id: u64,
    ) -> Result<McpResponse, HttpRequestError> {
        let mut stream = response.bytes_stream().eventsource();
        while let Some(event) = stream.next().await {
            let event = event.map_err(|error| {
                HttpRequestError::from_error(McpError::Protocol(format!(
                    "Failed to read response stream: {error}"
                )))
            })?;
            let message =
                serde_json::from_str::<McpIncomingMessage>(&event.data).map_err(|error| {
                    HttpRequestError::from_error(McpError::Protocol(format!(
                        "Failed to parse response stream: {error}"
                    )))
                })?;
            match message {
                McpIncomingMessage::Response(response) if response.id == Some(request_id) => {
                    return Ok(response);
                }
                McpIncomingMessage::Notification(notification) => {
                    let _ = self.notification_tx.send(notification);
                }
                McpIncomingMessage::Response(_) => {}
            }
        }

        Err(HttpRequestError::from_error(McpError::Protocol(
            "Response stream ended before the matching response".to_string(),
        )))
    }

    /// Send request via the legacy `/message` endpoint.
    async fn send_legacy_http_request(&self, request: McpRequest) -> Result<McpResponse, McpError> {
        let url = format!("{}/message", self.base_url.trim_end_matches('/'));

        let mut req = self
            .client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&request);
        req = self.apply_authenticated_config_headers(req).await?;

        self.ensure_repository_request_allowed()?;
        let response = req
            .send()
            .await
            .map_err(|e| McpError::RequestFailed(format!("HTTP request failed: {e}")))?;

        if !response.status().is_success() {
            return Err(McpError::RequestFailed(format!(
                "HTTP error: {}",
                response.status()
            )));
        }

        let mcp_response: McpResponse = response
            .json()
            .await
            .map_err(|e| McpError::Protocol(format!("Failed to parse response: {e}")))?;

        Ok(mcp_response)
    }

    /// Send request via SSE channel
    async fn send_sse_request(&mut self, request: McpRequest) -> Result<McpResponse, McpError> {
        let id = request.id;

        // Set up response channel
        let (tx, rx) = oneshot::channel();
        let _pending_request = PendingRequestGuard::register(&self.pending_sse, id, tx);

        // Send via HTTP POST (SSE is for receiving)
        let url = format!("{}/message", self.base_url.trim_end_matches('/'));

        let mut req = self
            .client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&request);
        req = self.apply_authenticated_config_headers(req).await?;

        if let Err(error) = self.ensure_repository_request_allowed() {
            let mut pending = self.pending_sse.lock().unwrap();
            pending.remove(&id);
            return Err(error);
        }
        let response = match req.send().await {
            Ok(response) => response,
            Err(e) => {
                let mut pending = self.pending_sse.lock().unwrap();
                pending.remove(&id);
                return Err(McpError::RequestFailed(format!("SSE send failed: {e}")));
            }
        };

        if !response.status().is_success() {
            let mut pending = self.pending_sse.lock().unwrap();
            pending.remove(&id);
            return Err(McpError::RequestFailed(format!(
                "HTTP error: {}",
                response.status()
            )));
        }

        // Wait for response via SSE stream
        let timeout = Duration::from_millis(self.config.timeout.unwrap_or(30_000));
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                let mut pending = self.pending_sse.lock().unwrap();
                pending.remove(&id);
                Err(McpError::Protocol(
                    "SSE response channel closed".to_string(),
                ))
            }
            Err(_) => {
                // Remove from pending
                let mut pending = self.pending_sse.lock().unwrap();
                pending.remove(&id);
                Err(McpError::Timeout)
            }
        }
    }

    /// Send a notification (no response expected)
    async fn send_notification(&self, value: &impl serde::Serialize) -> Result<(), McpError> {
        if self.config.transport == McpTransport::Http
            && self.transport_mode == HttpTransportMode::Streamable
        {
            let req = self.client.post(&self.base_url).json(value);
            let response = self
                .apply_streamable_headers(req)
                .await?
                .header(header::CONTENT_TYPE, "application/json");
            self.ensure_repository_request_allowed()?;
            let response = response.send().await.map_err(|error| {
                McpError::RequestFailed(format!("Notification failed: {error}"))
            })?;
            if requires_hosted_orb_auth(&self.config)
                && matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                )
            {
                self.auth.invalidate();
            }
            if !response.status().is_success() {
                return Err(McpError::RequestFailed(format!(
                    "Notification HTTP error: {}",
                    response.status()
                )));
            }
            return Ok(());
        }

        let url = format!("{}/message", self.base_url.trim_end_matches('/'));

        let mut req = self
            .client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .json(value);
        req = self.apply_authenticated_config_headers(req).await?;

        self.ensure_repository_request_allowed()?;
        let response = req
            .send()
            .await
            .map_err(|e| McpError::RequestFailed(format!("Notification failed: {e}")))?;
        if !response.status().is_success() {
            return Err(McpError::RequestFailed(format!(
                "Notification HTTP error: {}",
                response.status()
            )));
        }

        Ok(())
    }

    async fn delete_streamable_session(&self, session_id: &str) -> Result<(), McpError> {
        let req = self.client.delete(&self.base_url);
        let request = self
            .apply_streamable_headers(req)
            .await?
            .header(MCP_SESSION_ID_HEADER, session_id);
        self.ensure_repository_request_allowed()?;
        let response = request.send().await.map_err(|error| {
            McpError::RequestFailed(format!("Session termination failed: {error}"))
        })?;
        if requires_hosted_orb_auth(&self.config)
            && matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
        {
            self.auth.invalidate();
        }
        if !response.status().is_success() && response.status() != StatusCode::METHOD_NOT_ALLOWED {
            return Err(McpError::RequestFailed(format!(
                "Session termination HTTP error: {}",
                response.status()
            )));
        }
        Ok(())
    }

    /// Get next request ID
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Disconnect from the server
    pub async fn disconnect(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
        let session_id = (self.transport_mode == HttpTransportMode::Streamable)
            .then(|| self.session_id.take())
            .flatten();
        if let Some(session_id) = session_id {
            let _ = self.delete_streamable_session(&session_id).await;
        }
        self.session_id = None;
        self.protocol_version = None;
        self.notification_rx = None;
        self.initialized = false;
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.initialized
    }
}

impl Drop for HttpConnection {
    fn drop(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::auth::ManagedMcpAuth;
    use crate::mcp::protocol::MCP_CANCELLED_METHOD;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // Placeholder bearer access token for an OAuth-provisioned server; no identity provider is contacted.
    const TEST_AUTHORIZATION: &str = "Bearer test-only-oauth-access-token";

    fn test_config(transport: McpTransport) -> McpServerConfig {
        McpServerConfig {
            name: "test".to_string(),
            transport,
            command: None,
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            url: Some("http://localhost:8080".to_string()),
            headers: HashMap::new(),
            headers_helper: None,
            auth_preset: None,
            connection_ref: None,
            credential_ref: None,
            managed_generation: None,
            supports_parallel_tool_calls: None,
            requires_project_approval: None,
            disabled_tools: Vec::new(),
            timeout: Some(5000),
            enabled: true,
            disabled: false,
            scope: crate::mcp::McpConfigScope::User,
        }
    }

    #[test]
    fn test_http_connection_new() {
        let config = test_config(McpTransport::Http);
        let conn = HttpConnection::new(config);
        assert!(conn.is_ok());
    }

    #[test]
    fn test_http_connection_requires_url() {
        let mut config = test_config(McpTransport::Http);
        config.url = None;
        let conn = HttpConnection::new(config);
        assert!(conn.is_err());
    }

    #[test]
    fn test_sse_connection_new() {
        let config = test_config(McpTransport::Sse);
        let conn = HttpConnection::new(config);
        assert!(conn.is_ok());
    }

    #[tokio::test]
    async fn test_http_connection_not_connected_initially() {
        let config = test_config(McpTransport::Http);
        let conn = HttpConnection::new(config).unwrap();
        assert!(!conn.is_connected());
    }

    #[tokio::test]
    async fn untrusted_repository_http_and_sse_reject_private_targets() {
        for (scope, transport, url) in [
            (
                crate::mcp::McpConfigScope::Project,
                McpTransport::Http,
                "http://127.0.0.1:9/mcp",
            ),
            (
                crate::mcp::McpConfigScope::Local,
                McpTransport::Sse,
                "http://192.168.1.1/sse",
            ),
            (
                crate::mcp::McpConfigScope::Project,
                McpTransport::Http,
                "http://169.254.169.254/latest/meta-data/",
            ),
        ] {
            let mut config = test_config(transport);
            config.scope = scope;
            config.url = Some(url.to_string());
            let mut connection = HttpConnection::new(config).unwrap();

            let error = connection
                .prepare_repository_client()
                .await
                .expect_err("untrusted repository endpoint must be blocked");
            assert!(matches!(
                error,
                McpError::ConnectionFailed(message)
                    if message.contains("blocked network target")
            ));
        }
    }

    #[tokio::test]
    async fn explicitly_trusted_repository_http_can_reach_loopback() {
        let _env_guard = crate::config::test_process_env_lock_async().await;
        let home = tempfile::tempdir().expect("temporary home");
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let previous_home = std::env::var_os("HOME");
        // SAFETY: the process-env lock serializes tests that mutate HOME.
        unsafe { std::env::set_var("HOME", home.path()) };
        crate::config::clear_global_config_cache();
        crate::config::set_workspace_trust_in_global_config(workspace.path(), true)
            .expect("grant workspace trust");

        let addr = start_error_server().await;
        let mut config = test_config(McpTransport::Http);
        config.scope = crate::mcp::McpConfigScope::Project;
        config.url = Some(format!("http://{addr}/mcp"));
        let mut connection =
            HttpConnection::new_with_workspace(config, Some(workspace.path())).unwrap();

        let error = connection
            .connect()
            .await
            .expect_err("test server intentionally returns HTTP 500");

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        crate::config::clear_global_config_cache();

        assert!(matches!(
            error,
            McpError::RequestFailed(message) if message.contains("HTTP error: 500")
        ));
    }

    #[test]
    fn test_next_id_increments() {
        let config = test_config(McpTransport::Http);
        let conn = HttpConnection::new(config).unwrap();
        assert_eq!(conn.next_id(), 1);
        assert_eq!(conn.next_id(), 2);
        assert_eq!(conn.next_id(), 3);
    }

    async fn start_error_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let response = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                let _ = socket.write_all(response).await;
                let _ = socket.shutdown().await;
            }
        });

        addr
    }

    async fn read_request_body(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                return String::new();
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP header terminator");
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8_lossy(&bytes[body_start..body_start + content_length]).into_owned()
    }

    struct TestHttpRequest {
        method: String,
        target: String,
        headers: HashMap<String, String>,
        body: String,
    }

    async fn read_http_request(socket: &mut TcpStream) -> TestHttpRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        let header_end = loop {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            assert!(read > 0, "request closed before headers were received");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
        };

        let header_text = String::from_utf8_lossy(&bytes[..header_end]);
        let mut lines = header_text.lines();
        let request_line = lines.next().expect("HTTP request line");
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().expect("HTTP method").to_string();
        let target = request_parts.next().expect("HTTP target").to_string();
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect::<HashMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            assert!(read > 0, "request closed before its body was received");
            bytes.extend_from_slice(&buffer[..read]);
        }

        TestHttpRequest {
            method,
            target,
            headers,
            body: String::from_utf8_lossy(&bytes[body_start..body_start + content_length])
                .into_owned(),
        }
    }

    async fn accept_http_request(listener: &TcpListener) -> (TcpStream, TestHttpRequest) {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut socket).await;
        (socket, request)
    }

    async fn write_http_response(
        socket: &mut TcpStream,
        status: &str,
        content_type: Option<&str>,
        session_id: Option<&str>,
        body: &str,
    ) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(content_type) = content_type {
            response.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        if let Some(session_id) = session_id {
            response.push_str(&format!("Mcp-Session-Id: {session_id}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(body);
        socket.write_all(response.as_bytes()).await.unwrap();
    }

    fn json_response(_method: &str, id: u64, result: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string()
    }

    fn sse_response(body: String) -> String {
        format!("data: {body}\n\n")
    }

    fn request_id(request: &TestHttpRequest) -> u64 {
        serde_json::from_str::<serde_json::Value>(&request.body)
            .expect("JSON-RPC request")
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .expect("JSON-RPC request id")
    }

    fn request_method(request: &TestHttpRequest) -> String {
        serde_json::from_str::<serde_json::Value>(&request.body)
            .expect("JSON-RPC request")
            .get("method")
            .and_then(serde_json::Value::as_str)
            .expect("JSON-RPC method")
            .to_string()
    }

    fn assert_streamable_headers(request: &TestHttpRequest, session_id: Option<&str>) {
        assert_streamable_headers_with_auth(request, session_id, TEST_AUTHORIZATION);
    }

    fn assert_streamable_headers_with_auth(
        request: &TestHttpRequest,
        session_id: Option<&str>,
        authorization: &str,
    ) {
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some(authorization)
        );
        assert!(request.headers.get("accept").is_some_and(
            |value| value.contains("application/json") && value.contains("text/event-stream")
        ));
        if request.method == "POST" {
            assert_eq!(
                request.headers.get("content-type").map(String::as_str),
                Some("application/json")
            );
        }
        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some("2024-11-05")
        );
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            session_id
        );
    }

    #[test]
    fn session_replay_is_limited_to_read_only_discovery_methods() {
        for method in [
            "tools/list",
            "resources/list",
            "resources/read",
            "prompts/list",
            "prompts/get",
        ] {
            assert!(
                is_read_only_discovery(method),
                "{method} should be replayable"
            );
        }
        for method in ["tools/call", "resources/subscribe", "prompts/complete"] {
            assert!(
                !is_read_only_discovery(method),
                "{method} must never be replayed automatically"
            );
        }
    }

    fn managed_test_config(url: String) -> McpServerConfig {
        McpServerConfig {
            name: crate::orb_connection::HOSTED_ORB_MCP_SERVER_NAME.to_owned(),
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some(url),
            headers: HashMap::new(),
            headers_helper: Some("externally managed by test authority".to_owned()),
            auth_preset: None,
            connection_ref: Some("orb-team".to_owned()),
            credential_ref: Some(
                "ref:orb/credential/00000000-0000-4000-8000-000000000001".to_owned(),
            ),
            managed_generation: Some(1),
            supports_parallel_tool_calls: None,
            requires_project_approval: Some(false),
            disabled_tools: Vec::new(),
            timeout: Some(5000),
            enabled: true,
            disabled: false,
            scope: crate::mcp::McpConfigScope::Managed,
        }
    }

    #[tokio::test]
    async fn managed_capability_probe_uses_authenticated_api_endpoint() {
        const MANAGED_AUTHORIZATION: &str = "Bearer managed-test-token";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.method, "GET");
            assert_eq!(request.target, "/api/capabilities");
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some(MANAGED_AUTHORIZATION)
            );
            assert!(
                request
                    .headers
                    .get("accept")
                    .is_some_and(|value| value.contains("application/json"))
            );
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                None,
                &serde_json::json!({
                    "api_version": "1.0.0",
                    "minimum_client_version": "0.1.0",
                    "features": ["hosted_maestro_delegation"],
                    "contract_digest": "sha256:test"
                })
                .to_string(),
            )
            .await;
        });

        let config = managed_test_config(format!("http://{addr}/mcp"));
        let auth =
            ManagedMcpAuth::with_loader_for_test(|| Ok(Some("managed-test-token".to_owned())));
        let mut connection = HttpConnection::new_with_auth(config, auth).unwrap();
        let capabilities = connection
            .fetch_api_capabilities()
            .await
            .expect("managed capability probe");
        assert_eq!(capabilities.api_version, "1.0.0");
        assert_eq!(capabilities.minimum_client_version, "0.1.0");
        assert_eq!(
            capabilities.features,
            vec!["hosted_maestro_delegation".to_string()]
        );
        assert_eq!(capabilities.contract_digest, "sha256:test");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn managed_streamable_http_never_falls_back_to_legacy_message_endpoint() {
        const MANAGED_AUTHORIZATION: &str = "Bearer managed-test-token";

        for status in ["400 Bad Request", "404 Not Found", "405 Method Not Allowed"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                assert_eq!(request.method, "POST");
                assert_eq!(request.target, "/mcp");
                assert_streamable_headers_with_auth(&request, None, MANAGED_AUTHORIZATION);
                assert_eq!(request_method(&request), "initialize");
                write_http_response(&mut socket, status, None, None, "").await;

                if let Ok(Ok((mut socket, _))) =
                    tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
                {
                    let request = read_http_request(&mut socket).await;
                    panic!(
                        "managed hosted Computer request unexpectedly fell back to {}",
                        request.target
                    );
                }
            });

            let config = managed_test_config(format!("http://{addr}/mcp"));
            let auth =
                ManagedMcpAuth::with_loader_for_test(|| Ok(Some("managed-test-token".to_owned())));
            let mut connection = HttpConnection::new_with_auth(config, auth).unwrap();
            let error = connection
                .connect()
                .await
                .expect_err("managed hosted Computer must reject unsupported streamable endpoints");
            assert!(
                matches!(&error, McpError::RequestFailed(message) if message.contains(status.split_once(' ').unwrap().0)),
                "unexpected connection error for {status}: {error:?}"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn managed_streamable_http_authenticates_every_request_including_delete() {
        const MANAGED_AUTHORIZATION: &str = "Bearer managed-test-token";

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.method, "POST");
            assert_eq!(request.target, "/mcp");
            assert_streamable_headers_with_auth(&request, None, MANAGED_AUTHORIZATION);
            assert_eq!(request_method(&request), "initialize");
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                Some("managed-session"),
                &json_response(
                    "initialize",
                    request_id(&request),
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                        "serverInfo": {"name": "managed-fixture", "version": "1"}
                    }),
                ),
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_streamable_headers_with_auth(
                &request,
                Some("managed-session"),
                MANAGED_AUTHORIZATION,
            );
            assert_eq!(request_method(&request), "notifications/initialized");
            write_http_response(&mut socket, "202 Accepted", None, None, "").await;

            for expected_method in ["tools/list", "resources/list", "prompts/list"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                assert_streamable_headers_with_auth(
                    &request,
                    Some("managed-session"),
                    MANAGED_AUTHORIZATION,
                );
                assert_eq!(request_method(&request), expected_method);
                let result = match expected_method {
                    "tools/list" => serde_json::json!({"tools": []}),
                    "resources/list" => serde_json::json!({"resources": []}),
                    "prompts/list" => serde_json::json!({"prompts": []}),
                    _ => unreachable!(),
                };
                write_http_response(
                    &mut socket,
                    "200 OK",
                    Some("application/json"),
                    None,
                    &json_response(expected_method, request_id(&request), result),
                )
                .await;
            }

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_streamable_headers_with_auth(
                &request,
                Some("managed-session"),
                MANAGED_AUTHORIZATION,
            );
            assert_eq!(request_method(&request), "tools/list");
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                None,
                &json_response(
                    "tools/list",
                    request_id(&request),
                    serde_json::json!({
                        "tools": [{
                            "name": "orb_launch_hosted_task",
                            "description": "Launch one hosted Computer task",
                            "inputSchema": {"type": "object"}
                        }]
                    }),
                ),
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_streamable_headers_with_auth(
                &request,
                Some("managed-session"),
                MANAGED_AUTHORIZATION,
            );
            assert_eq!(request_method(&request), "tools/call");
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            assert_eq!(body["params"]["name"], "orb_launch_hosted_task");
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                None,
                &json_response(
                    "tools/call",
                    request_id(&request),
                    serde_json::json!({
                        "content": [{"type": "text", "text": "hosted-task-receipt"}]
                    }),
                ),
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.method, "DELETE");
            assert_eq!(request.target, "/mcp");
            assert_streamable_headers_with_auth(
                &request,
                Some("managed-session"),
                MANAGED_AUTHORIZATION,
            );
            write_http_response(&mut socket, "200 OK", None, None, "").await;
        });

        let config = managed_test_config(format!("http://{addr}/mcp"));
        let auth =
            ManagedMcpAuth::with_loader_for_test(|| Ok(Some("managed-test-token".to_owned())));
        let mut connection = HttpConnection::new_with_auth(config, auth).unwrap();
        connection
            .connect()
            .await
            .expect("managed streamable connection");
        connection
            .refresh_tools()
            .await
            .expect("managed tools/list");
        connection
            .call_tool(
                "orb_launch_hosted_task",
                serde_json::json!({"prompt": "run hosted task"}),
            )
            .await
            .expect("single managed hosted Computer launch");
        connection.disconnect().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_transport_uses_streamable_endpoint_and_terminates_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.method, "POST");
            assert_eq!(request.target, "/mcp");
            assert_streamable_headers(&request, None);
            assert_eq!(request_method(&request), "initialize");
            write_http_response(
                &mut socket,
                "200 OK",
                Some("text/event-stream"),
                Some("session-1"),
                &sse_response(json_response(
                    "initialize",
                    request_id(&request),
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                        "serverInfo": {"name": "fixture", "version": "1"}
                    }),
                )),
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.target, "/mcp");
            assert_streamable_headers(&request, Some("session-1"));
            assert_eq!(request_method(&request), "notifications/initialized");
            write_http_response(&mut socket, "202 Accepted", None, None, "").await;

            for expected_method in ["tools/list", "resources/list", "prompts/list"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                assert_eq!(request.target, "/mcp");
                assert_streamable_headers(&request, Some("session-1"));
                assert_eq!(request_method(&request), expected_method);
                let result = match expected_method {
                    "tools/list" => serde_json::json!({"tools": []}),
                    "resources/list" => serde_json::json!({"resources": []}),
                    "prompts/list" => serde_json::json!({"prompts": []}),
                    _ => unreachable!(),
                };
                write_http_response(
                    &mut socket,
                    "200 OK",
                    Some("text/event-stream"),
                    None,
                    &sse_response(json_response(expected_method, request_id(&request), result)),
                )
                .await;
            }

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.target, "/mcp");
            assert_streamable_headers(&request, Some("session-1"));
            assert_eq!(request_method(&request), "tools/list");
            write_http_response(
                &mut socket,
                "200 OK",
                Some("text/event-stream"),
                None,
                &sse_response(json_response(
                    "tools/list",
                    request_id(&request),
                    serde_json::json!({"tools": []}),
                )),
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.method, "DELETE");
            assert_eq!(request.target, "/mcp");
            assert_streamable_headers(&request, Some("session-1"));
            write_http_response(&mut socket, "200 OK", None, None, "").await;
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}/mcp"));
        config
            .headers
            .insert("Authorization".to_string(), TEST_AUTHORIZATION.to_string());
        let mut connection = HttpConnection::new(config).unwrap();

        connection.connect().await.expect("streamable connection");
        connection
            .refresh_tools()
            .await
            .expect("streamable tools/list");
        connection.disconnect().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_transport_falls_back_to_legacy_message_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert_eq!(request.method, "POST");
            assert_eq!(request.target, "/legacy");
            assert_streamable_headers(&request, None);
            assert_eq!(request_method(&request), "initialize");
            write_http_response(&mut socket, "404 Not Found", None, None, "").await;

            for expected_method in [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "resources/list",
                "prompts/list",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                assert_eq!(request.target, "/legacy/message");
                assert_eq!(
                    request.headers.get("authorization").map(String::as_str),
                    Some(TEST_AUTHORIZATION)
                );
                assert_eq!(request_method(&request), expected_method);
                if expected_method == "notifications/initialized" {
                    write_http_response(&mut socket, "202 Accepted", None, None, "").await;
                } else {
                    let result = match expected_method {
                        "initialize" => serde_json::json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "legacy", "version": "1"}
                        }),
                        "tools/list" => serde_json::json!({"tools": []}),
                        "resources/list" => serde_json::json!({"resources": []}),
                        "prompts/list" => serde_json::json!({"prompts": []}),
                        _ => unreachable!(),
                    };
                    write_http_response(
                        &mut socket,
                        "200 OK",
                        Some("application/json"),
                        None,
                        &json_response(expected_method, request_id(&request), result),
                    )
                    .await;
                }
            }
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}/legacy"));
        config
            .headers
            .insert("Authorization".to_string(), TEST_AUTHORIZATION.to_string());
        let mut connection = HttpConnection::new(config).unwrap();
        connection.connect().await.expect("legacy message fallback");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_transport_reconnects_after_session_404() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, request) = accept_http_request(&listener).await;
            assert_eq!(request.target, "/mcp");
            assert_eq!(request_method(&request), "initialize");
            assert_streamable_headers(&request, None);
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                Some("session-1"),
                &json_response(
                    "initialize",
                    request_id(&request),
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fixture", "version": "1"}
                    }),
                ),
            )
            .await;

            for expected_method in [
                "notifications/initialized",
                "tools/list",
                "resources/list",
                "prompts/list",
            ] {
                let (mut socket, request) = accept_http_request(&listener).await;
                assert_eq!(request.target, "/mcp");
                assert_streamable_headers(&request, Some("session-1"));
                assert_eq!(request_method(&request), expected_method);
                if expected_method == "notifications/initialized" {
                    write_http_response(&mut socket, "202 Accepted", None, None, "").await;
                } else {
                    write_http_response(
                        &mut socket,
                        "200 OK",
                        Some("application/json"),
                        None,
                        &json_response(
                            expected_method,
                            request_id(&request),
                            match expected_method {
                                "tools/list" => serde_json::json!({"tools": []}),
                                "resources/list" => serde_json::json!({"resources": []}),
                                "prompts/list" => serde_json::json!({"prompts": []}),
                                _ => unreachable!(),
                            },
                        ),
                    )
                    .await;
                }
            }

            let (mut socket, request) = accept_http_request(&listener).await;
            assert_eq!(request.target, "/mcp");
            assert_eq!(request_method(&request), "tools/list");
            assert_streamable_headers(&request, Some("session-1"));
            write_http_response(&mut socket, "404 Not Found", None, None, "").await;

            let (mut socket, request) = accept_http_request(&listener).await;
            assert_eq!(request.target, "/mcp");
            assert_eq!(request_method(&request), "initialize");
            assert_streamable_headers(&request, None);
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                Some("session-2"),
                &json_response(
                    "initialize",
                    request_id(&request),
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fixture", "version": "2"}
                    }),
                ),
            )
            .await;

            for expected_method in [
                "notifications/initialized",
                "tools/list",
                "resources/list",
                "prompts/list",
            ] {
                let (mut socket, request) = accept_http_request(&listener).await;
                assert_eq!(request.target, "/mcp");
                assert_streamable_headers(&request, Some("session-2"));
                assert_eq!(request_method(&request), expected_method);
                if expected_method == "notifications/initialized" {
                    write_http_response(&mut socket, "202 Accepted", None, None, "").await;
                } else {
                    write_http_response(
                        &mut socket,
                        "200 OK",
                        Some("application/json"),
                        None,
                        &json_response(
                            expected_method,
                            request_id(&request),
                            match expected_method {
                                "tools/list" => serde_json::json!({"tools": []}),
                                "resources/list" => serde_json::json!({"resources": []}),
                                "prompts/list" => serde_json::json!({"prompts": []}),
                                _ => unreachable!(),
                            },
                        ),
                    )
                    .await;
                }
            }

            let (mut socket, request) = accept_http_request(&listener).await;
            assert_eq!(request.target, "/mcp");
            assert_eq!(request_method(&request), "tools/list");
            assert_streamable_headers(&request, Some("session-2"));
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                None,
                &json_response(
                    "tools/list",
                    request_id(&request),
                    serde_json::json!({"tools": []}),
                ),
            )
            .await;
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}/mcp"));
        config
            .headers
            .insert("Authorization".to_string(), TEST_AUTHORIZATION.to_string());
        let mut connection = HttpConnection::new(config).unwrap();
        connection.connect().await.expect("initial connection");
        connection
            .refresh_tools()
            .await
            .expect("request should retry on a fresh session");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_transport_does_not_retry_after_workspace_revocation_on_session_404() {
        let _env_guard = crate::config::test_process_env_lock_async().await;
        let home = tempfile::tempdir().expect("temporary home");
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let previous_home = std::env::var_os("HOME");
        // SAFETY: the process-env lock serializes tests that mutate HOME.
        unsafe { std::env::set_var("HOME", home.path()) };
        crate::config::clear_global_config_cache();
        crate::config::set_workspace_trust_in_global_config(workspace.path(), true)
            .expect("grant workspace trust");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let workspace_path = workspace.path().to_path_buf();
        let server = tokio::spawn(async move {
            let (mut socket, request) = accept_http_request(&listener).await;
            assert_eq!(request_method(&request), "tools/list");
            assert_streamable_headers(&request, Some("session-1"));

            // Revoke before the 404 reaches the client. The retry path must
            // re-read global trust and must not send another request/header.
            crate::config::set_workspace_trust_in_global_config(&workspace_path, false)
                .expect("revoke workspace trust");
            write_http_response(&mut socket, "404 Not Found", None, None, "").await;

            let extra_request =
                tokio::time::timeout(Duration::from_millis(150), listener.accept()).await;
            assert!(
                extra_request.is_err(),
                "revoked repository discovery must not initialize or retry"
            );
        });

        let mut config = test_config(McpTransport::Http);
        config.scope = crate::mcp::McpConfigScope::Project;
        config.url = Some(format!("http://{addr}/mcp"));
        config
            .headers
            .insert("Authorization".to_owned(), TEST_AUTHORIZATION.to_owned());
        let mut connection =
            HttpConnection::new_with_workspace(config, Some(workspace.path())).unwrap();
        connection
            .prepare_repository_client()
            .await
            .expect("trusted repository client setup");
        connection.session_id = Some("session-1".to_owned());
        connection.protocol_version = Some(MCP_PROTOCOL_VERSION.to_owned());
        connection.initialized = true;

        let result = connection.refresh_tools().await;

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        crate::config::clear_global_config_cache();

        let error = result.expect_err("revoked trust must deny the 404 recovery");
        assert!(matches!(
            error,
            McpError::ConnectionFailed(message)
                if message.contains("requires workspace trust approval")
        ));
        assert!(!connection.is_connected());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_transport_does_not_replay_mutating_call_after_session_404() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, request) = accept_http_request(&listener).await;
            assert_eq!(request_method(&request), "initialize");
            assert_streamable_headers(&request, None);
            write_http_response(
                &mut socket,
                "200 OK",
                Some("application/json"),
                Some("session-1"),
                &json_response(
                    "initialize",
                    request_id(&request),
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fixture", "version": "1"}
                    }),
                ),
            )
            .await;

            for expected_method in [
                "notifications/initialized",
                "tools/list",
                "resources/list",
                "prompts/list",
            ] {
                let (mut socket, request) = accept_http_request(&listener).await;
                assert_streamable_headers(&request, Some("session-1"));
                assert_eq!(request_method(&request), expected_method);
                if expected_method == "notifications/initialized" {
                    write_http_response(&mut socket, "202 Accepted", None, None, "").await;
                } else {
                    let result = match expected_method {
                        "tools/list" => serde_json::json!({
                            "tools": [{
                                "name": "mutate",
                                "description": "mutating fixture",
                                "inputSchema": {"type": "object"}
                            }]
                        }),
                        "resources/list" => serde_json::json!({"resources": []}),
                        "prompts/list" => serde_json::json!({"prompts": []}),
                        _ => unreachable!(),
                    };
                    write_http_response(
                        &mut socket,
                        "200 OK",
                        Some("application/json"),
                        None,
                        &json_response(expected_method, request_id(&request), result),
                    )
                    .await;
                }
            }

            let (mut socket, request) = accept_http_request(&listener).await;
            assert_streamable_headers(&request, Some("session-1"));
            assert_eq!(request_method(&request), "tools/call");
            write_http_response(&mut socket, "404 Not Found", None, None, "").await;

            let extra_request =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
            assert!(
                extra_request.is_err(),
                "a mutating tools/call must not be replayed after session expiry"
            );
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}/mcp"));
        config
            .headers
            .insert("Authorization".to_owned(), TEST_AUTHORIZATION.to_owned());
        let mut connection = HttpConnection::new(config).unwrap();
        connection.connect().await.expect("initial connection");
        let result = connection.call_tool("mutate", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpError::RequestFailed(message)) if message.contains("404")));
        assert!(!connection.is_connected());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn abandoned_sse_requests_release_pending_entries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = test_config(McpTransport::Sse);
        config.url = Some(format!("http://{}", listener.local_addr().unwrap()));
        let mut connection = HttpConnection::new(config).unwrap();
        for id in 0..16 {
            {
                let request = connection.send_sse_request(McpRequest::list_tools(id));
                tokio::pin!(request);
                let (mut socket, _) = tokio::select! {
                    result = &mut request => panic!("request finished before POST acceptance: {result:?}"),
                    accepted = accept_http_request(&listener) => accepted,
                };
                write_http_response(&mut socket, "202 Accepted", None, None, "").await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(5), &mut request)
                        .await
                        .is_err()
                );
            }
            assert!(connection.pending_sse.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn failed_sse_authentication_releases_pending_entries() {
        let mut connection = HttpConnection::new(test_config(McpTransport::Sse)).unwrap();
        connection.config.auth_preset = Some("oauth".into());
        connection.config.url = None;
        for id in 0..128 {
            let error = connection
                .send_sse_request(McpRequest::list_tools(id))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("missing its URL"));
        }
        assert_eq!(
            connection.pending_sse.lock().unwrap().len(),
            0,
            "authentication failures must not retain pending senders"
        );
    }

    #[tokio::test]
    async fn test_send_sse_request_clears_pending_on_http_error() {
        let addr = start_error_server().await;
        let mut config = test_config(McpTransport::Sse);
        config.url = Some(format!("http://{}", addr));
        config.timeout = Some(100);

        let mut conn = HttpConnection::new(config).unwrap();
        let request = McpRequest::list_tools(conn.next_id());

        let result = conn.send_sse_request(request).await;
        assert!(matches!(result, Err(McpError::RequestFailed(_))));

        let pending_len = conn.pending_sse.lock().unwrap().len();
        assert_eq!(pending_len, 0);
    }

    #[tokio::test]
    async fn acknowledged_http_cancellation_remains_indeterminate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (call_seen_tx, call_seen_rx) = tokio::sync::oneshot::channel();
        let (notification_tx, notification_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut call_socket, _) = listener.accept().await.unwrap();
            let call_body = read_request_body(&mut call_socket).await;
            let call: serde_json::Value = serde_json::from_str(&call_body).unwrap();
            assert_eq!(call["method"], "tools/call");
            let _ = call_seen_tx.send(());

            // Keep the original request socket open while cancellation is
            // delivered over a second HTTP request.
            let (mut notification_socket, _) = listener.accept().await.unwrap();
            let notification_body = read_request_body(&mut notification_socket).await;
            let notification: serde_json::Value = serde_json::from_str(&notification_body).unwrap();
            assert_eq!(notification["method"], MCP_CANCELLED_METHOD);
            let _ = notification_tx.send(notification);
            let response =
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = notification_socket.write_all(response).await;
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}"));
        let mut conn = HttpConnection::new(config).unwrap();
        let request_id = conn.next_id();
        let request = McpRequest::call_tool(request_id, "mutate", serde_json::json!({}));
        let cancel = CancellationToken::new();
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                call_seen_rx.await.expect("server saw tool request");
                cancel.cancel();
            }
        });

        let result = conn.send_request_cancellable(request, &cancel).await;
        assert!(
            matches!(result, Err(McpError::Indeterminate(ref message)) if message.contains("outcome is unknown")),
            "acknowledgement must not imply that the remote mutation was cancelled: {result:?}"
        );
        let notification = notification_rx.await.expect("server saw cancellation");
        assert_eq!(notification["params"]["requestId"], request_id);
    }

    #[tokio::test]
    async fn completed_request_wins_after_dispatch_when_cancellation_is_already_ready() {
        let cancel = CancellationToken::new();
        let cancel_from_request = cancel.clone();
        let mut first_poll = true;
        let request = poll_fn(move |cx| {
            if first_poll {
                first_poll = false;
                cancel_from_request.cancel();
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(42)
            }
        });

        let result = await_request_or_cancellation(request, &cancel).await;

        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn pre_cancelled_http_request_never_reaches_the_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = 0;
            while let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                requests += 1;
                let _ = read_request_body(&mut socket).await;
                let response =
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(response).await;
            }
            requests
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}"));
        let mut conn = HttpConnection::new(config).unwrap();
        let request = McpRequest::call_tool(conn.next_id(), "mutate", serde_json::json!({}));
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = conn.send_request_cancellable(request, &cancel).await;

        assert!(matches!(result, Err(McpError::Cancelled)));
        assert_eq!(
            server.await.unwrap(),
            0,
            "a pre-cancelled mutating request must not be polled or sent"
        );
    }

    #[tokio::test]
    async fn hanging_http_cancellation_notification_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (call_seen_tx, call_seen_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut call_socket, _) = listener.accept().await.unwrap();
            let call_body = read_request_body(&mut call_socket).await;
            let call: serde_json::Value = serde_json::from_str(&call_body).unwrap();
            assert_eq!(call["method"], "tools/call");
            let _ = call_seen_tx.send(());

            let (mut notification_socket, _) = listener.accept().await.unwrap();
            let notification_body = read_request_body(&mut notification_socket).await;
            let notification: serde_json::Value = serde_json::from_str(&notification_body).unwrap();
            assert_eq!(notification["method"], MCP_CANCELLED_METHOD);
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}"));
        config.timeout = Some(30_000);
        let mut conn = HttpConnection::new(config).unwrap();
        let request = McpRequest::call_tool(conn.next_id(), "mutate", serde_json::json!({}));
        let cancel = CancellationToken::new();
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                call_seen_rx.await.expect("server saw tool request");
                cancel.cancel();
            }
        });

        let started = tokio::time::Instant::now();
        let result = conn.send_request_cancellable(request, &cancel).await;

        assert!(
            matches!(result, Err(McpError::Indeterminate(ref message))
                if message.contains("cancellation notification")),
            "notification timeout must remain visible: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation delivery must not inherit the 30-second request timeout"
        );
    }

    #[tokio::test]
    async fn rejected_http_cancellation_is_not_reported_as_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (call_seen_tx, call_seen_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut call_socket, _) = listener.accept().await.unwrap();
            let call_body = read_request_body(&mut call_socket).await;
            let call: serde_json::Value = serde_json::from_str(&call_body).unwrap();
            assert_eq!(call["method"], "tools/call");
            let _ = call_seen_tx.send(());

            let (mut notification_socket, _) = listener.accept().await.unwrap();
            let notification_body = read_request_body(&mut notification_socket).await;
            let notification: serde_json::Value = serde_json::from_str(&notification_body).unwrap();
            assert_eq!(notification["method"], MCP_CANCELLED_METHOD);
            let response = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = notification_socket.write_all(response).await;
        });

        let mut config = test_config(McpTransport::Http);
        config.url = Some(format!("http://{addr}"));
        let mut conn = HttpConnection::new(config).unwrap();
        let request = McpRequest::call_tool(conn.next_id(), "mutate", serde_json::json!({}));
        let cancel = CancellationToken::new();
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                call_seen_rx.await.expect("server saw tool request");
                cancel.cancel();
            }
        });

        let result = conn.send_request_cancellable(request, &cancel).await;

        assert!(
            matches!(result, Err(McpError::Indeterminate(ref message)) if message.contains("503")),
            "server rejection must remain visible instead of becoming Cancelled: {result:?}"
        );
    }
}
