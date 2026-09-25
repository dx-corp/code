use crate::agent::ExecutionReceipt;
use maestro_runtime_contracts::tool_wire;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{
    CODEX_SUBAGENT_TOOL_PREFIX, CODEX_SUBAGENT_WORK_GRAPH_SCHEMA, ClientCapabilities, ClientInfo,
    ConnectionRole, ConnectionState, FromAgentMessage, HeadlessErrorType, ResponseToolsSummary,
    ServerCapabilities, ServerRequestResolutionStatus, ServerRequestType, ToAgentMessage,
    TokenUsage, UtilityCommandShellMode, UtilityCommandTerminalMode, WorkspaceCapabilitySetApplied,
};

/// # State Synchronization
///
/// The `AgentState` uses an event-sourcing pattern where state is derived from
/// incoming messages rather than queried. The `handle_message()` method processes
/// each `FromAgentMessage` and updates internal state accordingly.
///
/// Benefits of this approach:
/// - **No polling** - State updates are event-driven
/// - **Consistency** - State always reflects the latest message
/// - **Simplicity** - No need for separate state query protocol
///
/// # Usage Pattern
///
/// ```rust,ignore
/// use maestro_local_host::headless::{AgentState, FromAgentMessage};
///
/// let mut state = AgentState::default();
///
/// // Process a message
/// let msg = FromAgentMessage::Ready {
///     model: "claude-3-opus".to_string(),
///     provider: "anthropic".to_string(),
/// };
///
/// if let Some(event) = state.handle_message(msg) {
///     // React to the event
///     println!("Agent is ready!");
/// }
///
/// assert!(state.is_ready);
/// assert_eq!(state.model.as_deref(), Some("claude-3-opus"));
/// ```
///
/// # Thread Safety
///
/// `AgentState` is `Clone` but not thread-safe (`!Sync`). Each transport should
/// maintain its own state instance. For shared state across threads, wrap in
/// `Arc<Mutex<AgentState>>`.
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    /// Transient pending invocations, never restored after resident process death.
    pub pending_managed_authorizations: Vec<String>,
    /// Model information
    pub protocol_version: Option<String>,
    pub client_protocol_version: Option<String>,
    pub controller_binding_version: Option<String>,
    pub controller_binding_sha256: Option<String>,
    pub client_info: Option<ClientInfo>,
    pub capabilities: Option<ClientCapabilities>,
    pub server_capabilities: Option<ServerCapabilities>,
    pub opt_out_notifications: Option<Vec<String>>,
    pub connection_role: Option<ConnectionRole>,
    pub connection_count: usize,
    pub subscriber_count: usize,
    pub controller_subscription_id: Option<String>,
    pub controller_connection_id: Option<String>,
    pub connections: Vec<ConnectionState>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Session information
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// Current response being streamed
    pub current_response: Option<StreamingResponse>,
    /// Pending tool calls requiring approval
    pub pending_approvals: Vec<PendingApproval>,
    /// Pending client-side tool execution requests
    pub pending_client_tools: Vec<PendingApproval>,
    /// Capability binding required to complete a governed client tool.
    pub governed_client_tool_bindings: HashMap<String, GovernedClientToolBinding>,
    /// Pending structured user input requests
    pub pending_user_inputs: Vec<PendingApproval>,
    /// Pending tool retry requests
    pub pending_tool_retries: Vec<PendingApproval>,
    /// Active tool executions
    pub active_tools: HashMap<String, ActiveTool>,
    /// Active utility-plane commands
    pub active_utility_commands: HashMap<String, ActiveUtilityCommand>,
    /// Active utility-plane file watches
    pub active_file_watches: HashMap<String, ActiveFileWatch>,
    /// Tracks tool metadata until a tool run completes, even when approval is not required.
    pub tracked_tools: HashMap<String, PendingApproval>,
    /// Durable Codex app-server subagent edge metadata for restore/drain continuity.
    pub codex_subagent_edges: Vec<CodexSubagentContinuityEdge>,
    /// Last error message
    pub last_error: Option<String>,
    /// Last structured error type
    pub last_error_type: Option<HeadlessErrorType>,
    /// Exact provider failure classification, when the last error came from a provider terminal.
    pub provider_error_kind: Option<maestro_ai::ProviderStreamErrorKind>,
    /// Last status message
    pub last_status: Option<String>,
    /// Last response duration
    pub last_response_duration_ms: Option<u64>,
    /// Last time-to-first-token telemetry
    pub last_ttft_ms: Option<u64>,
    /// Whether the agent is ready
    pub is_ready: bool,
    /// Whether currently processing a response
    pub is_responding: bool,
}

pub(crate) const HEADLESS_OUTPUT_LIMIT: usize = 32_768;

fn append_headless_output(existing: &mut String, chunk: &str) {
    existing.push_str(chunk);
    if existing.len() <= HEADLESS_OUTPUT_LIMIT {
        return;
    }
    let mut drain_until = existing.len() - HEADLESS_OUTPUT_LIMIT;
    while drain_until < existing.len() && !existing.is_char_boundary(drain_until) {
        drain_until += 1;
    }
    existing.drain(..drain_until);
}

/// A response currently being streamed
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamingResponse {
    pub response_id: String,
    pub text: String,
    pub thinking: String,
    pub usage: Option<TokenUsage>,
}

impl StreamingResponse {
    #[must_use]
    pub fn new(response_id: String) -> Self {
        Self {
            response_id,
            text: String::new(),
            thinking: String::new(),
            usage: None,
        }
    }

    pub fn append(&mut self, content: &str, is_thinking: bool) {
        if is_thinking {
            self.thinking.push_str(content);
        } else {
            self.text.push_str(content);
        }
    }
}

/// A tool call pending approval
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingApproval {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub tool: String,
    pub args: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernedClientToolBinding {
    pub tool_execution_id: String,
    pub client_instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_binding_id: Option<String>,
    pub grant_id: String,
    pub grant_version: u64,
    pub grant_hash: String,
    #[serde(default)]
    pub turn_digest: String,
    pub definition_digest: String,
    pub args_digest: String,
    pub owner_lease_epoch: u64,
    pub idempotency_key: String,
}

/// Durable edge metadata for Codex app-server subagent tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CodexSubagentContinuityEdge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_tool_execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_tool_execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub operation: String,
    pub status: String,
}

/// A tool currently executing
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTool {
    pub call_id: String,
    pub tool: String,
    pub output: String,
    pub started: std::time::Instant,
}

/// A utility command currently executing
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveUtilityCommand {
    pub command_id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub shell_mode: UtilityCommandShellMode,
    pub terminal_mode: UtilityCommandTerminalMode,
    pub pid: Option<u32>,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    pub owner_connection_id: Option<String>,
    pub output: String,
}

/// A file watch currently active on the runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveFileWatch {
    pub watch_id: String,
    pub root_dir: String,
    pub include_patterns: Option<Vec<String>>,
    pub exclude_patterns: Option<Vec<String>>,
    pub debounce_ms: u32,
    pub owner_connection_id: Option<String>,
}

pub(crate) fn codex_subagent_operation(tool: &str) -> Option<&'static str> {
    let suffix = tool
        .strip_prefix(CODEX_SUBAGENT_TOOL_PREFIX)
        .unwrap_or(tool);
    match suffix {
        "spawnAgent" | "spawn_agent" => Some("spawn_agent"),
        "sendInput" | "send_input" => Some("send_input"),
        "resumeAgent" | "resume_agent" => Some("resume_agent"),
        "wait" | "waitAgent" | "wait_agent" => Some("wait_agent"),
        "closeAgent" | "close_agent" => Some("close_agent"),
        _ => None,
    }
}

pub(crate) fn active_codex_subagent_status(operation: &str) -> &'static str {
    match operation {
        "send_input" => "waiting_for_input_ack",
        "wait_agent" => "wait_pending",
        "close_agent" => "waiting_for_close",
        "resume_agent" => "restoring",
        _ => "waiting_for_restore",
    }
}

fn terminal_codex_subagent_status(operation: &str, success: bool) -> &'static str {
    if !success {
        return "failed";
    }
    match operation {
        "spawn_agent" => "spawned",
        "send_input" => "acknowledged",
        "resume_agent" => "resumed",
        "close_agent" => "closed",
        _ => "completed",
    }
}

pub(crate) fn codex_subagent_status_is_terminal(status: &str) -> bool {
    let normalized = status
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch == '-' || ch == ' ' { '_' } else { ch })
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "closed"
            | "explicitly_closed"
            | "succeeded"
            | "success"
            | "completed"
            | "complete"
            | "done"
            | "acknowledged"
            | "failed"
            | "failure"
            | "error"
            | "cancelled"
            | "canceled"
    )
}

pub(crate) fn json_string_from_object(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

pub(crate) fn json_string_array_from_object(
    object: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Vec<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_array))
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn has_codex_work_graph(args: Option<&serde_json::Value>) -> bool {
    let Some(args) = args.and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(graph) = args
        .get("codexWorkGraph")
        .or_else(|| args.get("codex_work_graph"))
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    json_string_from_object(graph, &["schemaVersion", "schema_version"]).as_deref()
        == Some(CODEX_SUBAGENT_WORK_GRAPH_SCHEMA)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexSubagentChildRun {
    pub child_run_id: Option<String>,
    pub thread_id: Option<String>,
    pub status: Option<String>,
}

pub(crate) fn codex_subagent_child_runs(
    args: Option<&serde_json::Value>,
) -> Vec<CodexSubagentChildRun> {
    let Some(args) = args.and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let child_run_ids = json_string_array_from_object(args, &["childRunIds", "child_run_ids"]);
    let thread_ids =
        json_string_array_from_object(args, &["receiverThreadIds", "receiver_thread_ids"]);
    let graph = args
        .get("codexWorkGraph")
        .or_else(|| args.get("codex_work_graph"));
    let mut graph_runs = Vec::new();
    if let Some(graph) = graph.and_then(serde_json::Value::as_object) {
        if let Some(child_runs) = graph
            .get("childRuns")
            .or_else(|| graph.get("child_runs"))
            .and_then(serde_json::Value::as_array)
        {
            for child_run in child_runs {
                let Some(child_run) = child_run.as_object() else {
                    continue;
                };
                graph_runs.push(CodexSubagentChildRun {
                    child_run_id: json_string_from_object(
                        child_run,
                        &["childRunId", "child_run_id"],
                    ),
                    thread_id: json_string_from_object(child_run, &["threadId", "thread_id"]),
                    status: json_string_from_object(
                        child_run,
                        &["status", "targetStatus", "target_status"],
                    ),
                });
            }
        }
    }
    let count = child_run_ids
        .len()
        .max(thread_ids.len())
        .max(graph_runs.len())
        .max(1);
    let mut runs = Vec::new();
    for index in 0..count {
        let thread_id = thread_ids
            .get(index)
            .cloned()
            .or_else(|| graph_runs.get(index).and_then(|run| run.thread_id.clone()));
        let status = graph_runs
            .get(index)
            .and_then(|run| run.status.clone())
            .or_else(|| codex_agent_state_status(args, thread_id.as_deref()));
        let child_run_id = child_run_ids.get(index).cloned().or_else(|| {
            graph_runs
                .get(index)
                .and_then(|run| run.child_run_id.clone())
        });
        if child_run_id.is_some() || thread_id.is_some() {
            runs.push(CodexSubagentChildRun {
                child_run_id,
                thread_id,
                status,
            });
        }
    }
    runs
}

fn codex_agent_state_status(
    args: &serde_json::Map<String, serde_json::Value>,
    thread_id: Option<&str>,
) -> Option<String> {
    let thread_id = thread_id?;
    let agent_states = args
        .get("agentsStates")
        .or_else(|| args.get("agents_states"))?
        .as_object()?;
    let agent_state = agent_states.get(thread_id)?.as_object()?;
    json_string_from_object(agent_state, &["status"])
}

fn has_codex_subagent_args(args: &serde_json::Value) -> bool {
    let Some(object) = args.as_object() else {
        return false;
    };
    has_codex_work_graph(Some(args))
        || !json_string_array_from_object(object, &["childRunIds", "child_run_ids"]).is_empty()
        || !json_string_array_from_object(object, &["receiverThreadIds", "receiver_thread_ids"])
            .is_empty()
}

fn merge_codex_subagent_args(
    base: Option<&serde_json::Value>,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut merged = base
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, value) in patch {
        merged.insert(key.clone(), value.clone());
    }
    serde_json::Value::Object(merged)
}

fn codex_subagent_tool_end_args(
    source_args: Option<&serde_json::Value>,
    details: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let Some(details_object) = details.and_then(serde_json::Value::as_object) else {
        return source_args.cloned();
    };
    if let Some(args) = details_object
        .get("args")
        .and_then(serde_json::Value::as_object)
    {
        return Some(merge_codex_subagent_args(source_args, args));
    }
    let details = serde_json::Value::Object(details_object.clone());
    if has_codex_subagent_args(&details) {
        return Some(merge_codex_subagent_args(source_args, details_object));
    }
    source_args.cloned()
}

pub(crate) fn codex_subagent_edge_key(edge: &CodexSubagentContinuityEdge) -> String {
    [
        edge.spawn_tool_call_id.as_deref().unwrap_or_default(),
        edge.wait_tool_call_id.as_deref().unwrap_or_default(),
        edge.child_run_id.as_deref().unwrap_or_default(),
        edge.thread_id.as_deref().unwrap_or_default(),
        edge.operation.as_str(),
    ]
    .join("\0")
}

impl AgentState {
    fn upsert_codex_subagent_edges(
        &mut self,
        call_id: &str,
        tool_execution_id: Option<&str>,
        tool: &str,
        args: Option<&serde_json::Value>,
        status: &str,
    ) {
        let Some(operation) = codex_subagent_operation(tool) else {
            return;
        };
        if !tool.starts_with(CODEX_SUBAGENT_TOOL_PREFIX) && !has_codex_work_graph(args) {
            return;
        }
        let child_runs = codex_subagent_child_runs(args);
        let mut edges = Vec::new();
        if child_runs.is_empty() {
            edges.push(CodexSubagentContinuityEdge {
                spawn_tool_call_id: (operation == "spawn_agent").then(|| call_id.to_string()),
                spawn_tool_execution_id: if operation == "spawn_agent" {
                    tool_execution_id.map(str::to_string)
                } else {
                    None
                },
                wait_tool_call_id: (operation != "spawn_agent").then(|| call_id.to_string()),
                wait_tool_execution_id: if operation != "spawn_agent" {
                    tool_execution_id.map(str::to_string)
                } else {
                    None
                },
                child_run_id: None,
                thread_id: None,
                operation: operation.to_string(),
                status: status.to_string(),
            });
        } else {
            for child_run in child_runs {
                edges.push(CodexSubagentContinuityEdge {
                    spawn_tool_call_id: (operation == "spawn_agent").then(|| call_id.to_string()),
                    spawn_tool_execution_id: if operation == "spawn_agent" {
                        tool_execution_id.map(str::to_string)
                    } else {
                        None
                    },
                    wait_tool_call_id: (operation != "spawn_agent").then(|| call_id.to_string()),
                    wait_tool_execution_id: if operation != "spawn_agent" {
                        tool_execution_id.map(str::to_string)
                    } else {
                        None
                    },
                    child_run_id: child_run.child_run_id,
                    thread_id: child_run.thread_id,
                    operation: operation.to_string(),
                    status: child_run.status.unwrap_or_else(|| status.to_string()),
                });
            }
        }

        let mut existing = self
            .codex_subagent_edges
            .iter()
            .cloned()
            .map(|edge| (codex_subagent_edge_key(&edge), edge))
            .collect::<HashMap<_, _>>();
        if edges
            .iter()
            .any(|edge| edge.child_run_id.is_some() || edge.thread_id.is_some())
        {
            existing.retain(|_, edge| {
                let same_tool_call = edge.spawn_tool_call_id.as_deref() == Some(call_id)
                    || edge.wait_tool_call_id.as_deref() == Some(call_id);
                !(same_tool_call
                    && edge.operation == operation
                    && edge.child_run_id.is_none()
                    && edge.thread_id.is_none())
            });
        }
        for edge in &edges {
            existing.insert(codex_subagent_edge_key(edge), edge.clone());
        }
        if matches!(status, "closed" | "completed")
            && matches!(operation, "close_agent" | "wait_agent")
        {
            for edge in existing.values_mut() {
                if edge.operation == operation || codex_subagent_status_is_terminal(&edge.status) {
                    continue;
                }
                let same_child = edge.child_run_id.as_ref().is_some_and(|child_run_id| {
                    edges
                        .iter()
                        .any(|closed_edge| closed_edge.child_run_id.as_ref() == Some(child_run_id))
                }) || edge.thread_id.as_ref().is_some_and(|thread_id| {
                    edges
                        .iter()
                        .any(|closed_edge| closed_edge.thread_id.as_ref() == Some(thread_id))
                });
                if same_child {
                    edge.status = "completed".to_string();
                }
            }
        }
        let mut sorted_edges = existing.into_values().collect::<Vec<_>>();
        sorted_edges.sort_by(|left, right| {
            codex_subagent_edge_key(left).cmp(&codex_subagent_edge_key(right))
        });
        self.codex_subagent_edges = sorted_edges;
    }

    fn mark_codex_subagent_edges_failed_for_call(&mut self, call_id: &str) {
        if call_id.is_empty() {
            return;
        }
        let mut changed = false;
        for edge in &mut self.codex_subagent_edges {
            let matches = edge.spawn_tool_call_id.as_deref() == Some(call_id)
                || edge.wait_tool_call_id.as_deref() == Some(call_id);
            if matches && !codex_subagent_status_is_terminal(&edge.status) {
                edge.status = "failed".to_string();
                changed = true;
            }
        }
        if changed {
            self.codex_subagent_edges.sort_by(|left, right| {
                codex_subagent_edge_key(left).cmp(&codex_subagent_edge_key(right))
            });
        }
    }

    fn fail_codex_subagent_edge(&mut self, call_id: &str) {
        if let Some(source) = self.tracked_tools.get(call_id).cloned() {
            if let Some(operation) = codex_subagent_operation(&source.tool) {
                self.upsert_codex_subagent_edges(
                    call_id,
                    source.tool_execution_id.as_deref(),
                    &source.tool,
                    Some(&source.args),
                    terminal_codex_subagent_status(operation, false),
                );
            }
        }
        self.mark_codex_subagent_edges_failed_for_call(call_id);
    }

    /// Clear volatile runtime activity that can become stale across transport gaps.
    pub fn clear_transient_progress(&mut self) {
        self.current_response = None;
        self.active_tools.clear();
        self.active_utility_commands.clear();
        self.active_file_watches.clear();
        self.is_responding = false;
    }

    /// Clear request state that should not survive an explicit disconnect.
    pub fn clear_pending_request_state(&mut self) {
        self.pending_approvals.clear();
        self.pending_client_tools.clear();
        self.pending_user_inputs.clear();
        self.pending_tool_retries.clear();
        self.pending_managed_authorizations.clear();
        self.tracked_tools.clear();
    }

    /// Handle an outbound message and update optimistic local state.
    pub fn handle_sent_message(&mut self, msg: &ToAgentMessage) {
        match msg {
            ToAgentMessage::Hello {
                protocol_version,
                client_info,
                capabilities,
                role,
                opt_out_notifications,
                controller_binding: _,
            } => {
                self.client_protocol_version = protocol_version.clone();
                self.client_info = client_info.clone();
                self.capabilities = capabilities.clone();
                self.opt_out_notifications = opt_out_notifications.clone();
                self.connection_role = Some(role.unwrap_or(ConnectionRole::Controller));
                self.connection_count = 1;
                self.controller_connection_id = match self.connection_role {
                    Some(ConnectionRole::Controller) => Some("local".to_string()),
                    _ => None,
                };
                self.connections = vec![ConnectionState {
                    connection_id: "local".to_string(),
                    role: self.connection_role.unwrap_or(ConnectionRole::Controller),
                    client_protocol_version: protocol_version.clone(),
                    client_info: client_info.clone(),
                    capabilities: capabilities.clone(),
                    opt_out_notifications: opt_out_notifications.clone(),
                    subscription_count: 1,
                    attached_subscription_count: 1,
                    controller_lease_granted: matches!(
                        self.connection_role,
                        Some(ConnectionRole::Controller)
                    ),
                    lease_expires_at: None,
                }];
            }
            ToAgentMessage::Init { .. }
            | ToAgentMessage::GovernedInit { .. }
            | ToAgentMessage::ConfigurePromptExperiment { .. }
            | ToAgentMessage::ApplyWorkspaceCapabilitySet { .. } => {}
            ToAgentMessage::RestoreConversation { .. } => {}
            ToAgentMessage::Prompt { .. } | ToAgentMessage::GovernedPrompt { .. } => {
                self.current_response = None;
                self.last_error = None;
                self.last_error_type = None;
                self.provider_error_kind = None;
                self.last_status = None;
                self.is_responding = true;
            }
            ToAgentMessage::Steer { .. } | ToAgentMessage::GovernedSteer { .. } => {
                self.last_error = None;
                self.last_error_type = None;
                self.provider_error_kind = None;
                self.last_status = None;
                self.is_responding = true;
            }
            ToAgentMessage::Interrupt | ToAgentMessage::Cancel => {
                self.current_response = None;
                self.pending_approvals.clear();
                self.pending_client_tools.clear();
                self.pending_user_inputs.clear();
                self.pending_tool_retries.clear();
                self.pending_managed_authorizations.clear();
                self.active_tools.clear();
                self.active_utility_commands.clear();
                self.active_file_watches.clear();
                self.tracked_tools.clear();
                self.is_responding = false;
            }
            // Retain the pending request until the native consumer acknowledges it.
            ToAgentMessage::ManagedAuthorizationResult { .. } => {}
            ToAgentMessage::ToolResponse {
                call_id, approved, ..
            } => {
                if !approved {
                    self.fail_codex_subagent_edge(call_id);
                }
                let _ = self.remove_pending_approval(call_id);
                if !approved {
                    self.tracked_tools.remove(call_id);
                }
            }
            ToAgentMessage::ClientToolResult { call_id, .. }
            | ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
                call_id,
                ..
            }) => {
                self.pending_client_tools.retain(|p| p.call_id != *call_id);
                self.pending_user_inputs.retain(|p| p.call_id != *call_id);
            }
            ToAgentMessage::ServerRequestResponse {
                request_id,
                request_type,
                approved,
                ..
            } => match request_type {
                ServerRequestType::Approval => {
                    let call_id = self
                        .pending_approvals
                        .iter()
                        .find(|p| pending_request_matches(p, request_id))
                        .map(|p| p.call_id.clone())
                        .unwrap_or_else(|| request_id.clone());
                    if approved != &Some(true) {
                        self.fail_codex_subagent_edge(&call_id);
                    }
                    self.pending_approvals
                        .retain(|p| !pending_request_matches(p, request_id));
                    if approved != &Some(true) {
                        self.tracked_tools.remove(&call_id);
                        self.tracked_tools.remove(request_id);
                    }
                }
                ServerRequestType::ClientTool => {
                    self.pending_client_tools
                        .retain(|p| !pending_request_matches(p, request_id));
                }
                ServerRequestType::UserInput => {
                    self.pending_user_inputs
                        .retain(|p| !pending_request_matches(p, request_id));
                }
                ServerRequestType::ToolRetry => {
                    self.pending_tool_retries
                        .retain(|p| !pending_request_matches(p, request_id));
                }
            },
            ToAgentMessage::UtilityCommandStart { .. } => {}
            ToAgentMessage::UtilityCommandTerminate { .. } => {}
            ToAgentMessage::UtilityCommandStdin { .. } => {}
            ToAgentMessage::UtilityCommandResize { .. } => {}
            ToAgentMessage::UtilityFileSearch { .. } => {}
            ToAgentMessage::UtilityFileRead { .. } => {}
            ToAgentMessage::UtilityFileWatchStart { .. } => {}
            ToAgentMessage::UtilityFileWatchStop { .. } => {}
            ToAgentMessage::Shutdown => {
                self.current_response = None;
                self.pending_approvals.clear();
                self.pending_client_tools.clear();
                self.pending_user_inputs.clear();
                self.pending_tool_retries.clear();
                self.pending_managed_authorizations.clear();
                self.active_tools.clear();
                self.active_utility_commands.clear();
                self.active_file_watches.clear();
                self.tracked_tools.clear();
                self.is_ready = false;
                self.is_responding = false;
            }
        }
    }

    /// Handle an incoming message and update state
    pub fn handle_message(&mut self, msg: FromAgentMessage) -> Option<AgentEvent> {
        match msg {
            // A provider conversation snapshot is durable private runtime state,
            // not a customer-visible protocol event.
            FromAgentMessage::ConversationSnapshot { .. } => None,
            FromAgentMessage::ManagedAuthorizationRequest { request_id } => {
                if !self.pending_managed_authorizations.contains(&request_id) {
                    self.pending_managed_authorizations.push(request_id);
                }
                None
            }
            FromAgentMessage::ResponseAccepted(tool_wire::ResponseAccepted { request_id }) => {
                self.pending_managed_authorizations
                    .retain(|id| id != &request_id);
                None
            }
            FromAgentMessage::ProcessBudgetCheckpoint { .. } => None,
            FromAgentMessage::AssistantTextObserved { .. } => None,
            FromAgentMessage::ManagedGatewayReceipt {
                request_id,
                record_id,
                lineage_id,
                record_status,
                ..
            } => Some(AgentEvent::ManagedGatewayReceipt {
                request_id,
                record_id,
                lineage_id,
                record_status,
            }),
            FromAgentMessage::WorkspaceCapabilitySetApplied { receipt } => {
                Some(AgentEvent::WorkspaceCapabilitySetApplied { receipt })
            }
            FromAgentMessage::HelloOk {
                protocol_version,
                controller_binding_version,
                controller_binding_sha256,
                connection_id: _connection_id,
                client_protocol_version,
                client_info,
                capabilities,
                server_capabilities,
                opt_out_notifications,
                role,
                controller_connection_id,
                lease_expires_at: _lease_expires_at,
            } => {
                self.protocol_version = Some(protocol_version);
                self.controller_binding_version = controller_binding_version;
                self.controller_binding_sha256 = controller_binding_sha256;
                self.client_protocol_version = client_protocol_version;
                self.client_info = client_info;
                self.capabilities = capabilities;
                self.server_capabilities = server_capabilities;
                self.opt_out_notifications = opt_out_notifications;
                self.connection_role = role;
                self.controller_connection_id = controller_connection_id;
                None
            }
            FromAgentMessage::Ready {
                protocol_version,
                model,
                provider,
                session_id,
            } => {
                self.protocol_version = protocol_version.clone();
                self.model = Some(model.clone());
                self.provider = Some(provider.clone());
                self.session_id = session_id.clone();
                self.is_ready = true;
                Some(AgentEvent::Ready {
                    protocol_version,
                    model,
                    provider,
                    session_id,
                })
            }

            FromAgentMessage::SessionInfo {
                session_id,
                cwd,
                git_branch,
            } => {
                self.session_id = session_id.clone();
                self.cwd = Some(cwd.clone());
                self.git_branch = git_branch.clone();
                Some(AgentEvent::SessionInfo {
                    session_id,
                    cwd,
                    git_branch,
                })
            }
            FromAgentMessage::ConnectionInfo {
                connection_id: _connection_id,
                client_protocol_version,
                client_info,
                capabilities,
                opt_out_notifications,
                role,
                connection_count,
                controller_connection_id,
                lease_expires_at: _lease_expires_at,
                connections,
            } => {
                self.client_protocol_version = client_protocol_version;
                self.client_info = client_info;
                self.capabilities = capabilities;
                self.opt_out_notifications = opt_out_notifications;
                self.connection_role = role;
                self.connection_count = connection_count.unwrap_or_default();
                self.controller_connection_id = controller_connection_id;
                self.connections = connections.unwrap_or_default();
                None
            }
            FromAgentMessage::RawAgentEvent { event_type, event } => {
                Some(AgentEvent::RawAgentEvent { event_type, event })
            }
            FromAgentMessage::UtilityCommandStarted {
                command_id,
                command,
                cwd,
                shell_mode,
                terminal_mode,
                pid,
                columns,
                rows,
                owner_connection_id,
            } => {
                self.active_utility_commands.insert(
                    command_id.clone(),
                    ActiveUtilityCommand {
                        command_id,
                        command,
                        cwd,
                        shell_mode,
                        terminal_mode,
                        pid,
                        columns,
                        rows,
                        owner_connection_id,
                        output: String::new(),
                    },
                );
                None
            }
            FromAgentMessage::UtilityCommandResized {
                command_id,
                columns,
                rows,
            } => {
                if let Some(command) = self.active_utility_commands.get_mut(&command_id) {
                    command.columns = Some(columns);
                    command.rows = Some(rows);
                }
                None
            }
            FromAgentMessage::UtilityCommandOutput {
                command_id,
                content,
                ..
            } => {
                if let Some(command) = self.active_utility_commands.get_mut(&command_id) {
                    append_headless_output(&mut command.output, &content);
                }
                None
            }
            FromAgentMessage::UtilityCommandExited { command_id, .. } => {
                self.active_utility_commands.remove(&command_id);
                None
            }
            FromAgentMessage::UtilityFileSearchResults { .. } => None,
            FromAgentMessage::UtilityFileReadResult { .. } => None,
            FromAgentMessage::UtilityFileWatchStarted {
                watch_id,
                root_dir,
                include_patterns,
                exclude_patterns,
                debounce_ms,
                owner_connection_id,
            } => {
                self.active_file_watches.insert(
                    watch_id.clone(),
                    ActiveFileWatch {
                        watch_id,
                        root_dir,
                        include_patterns,
                        exclude_patterns,
                        debounce_ms,
                        owner_connection_id,
                    },
                );
                None
            }
            FromAgentMessage::UtilityFileWatchEvent { .. } => None,
            FromAgentMessage::UtilityFileWatchStopped { watch_id, .. } => {
                self.active_file_watches.remove(&watch_id);
                None
            }

            FromAgentMessage::ResponseStart { response_id } => {
                self.current_response = Some(StreamingResponse::new(response_id.clone()));
                self.is_responding = true;
                Some(AgentEvent::ResponseStart { response_id })
            }

            FromAgentMessage::ResponseChunk {
                response_id,
                content,
                is_thinking,
            } => {
                if let Some(ref mut response) = self.current_response {
                    if response.response_id == response_id {
                        response.append(&content, is_thinking);
                    }
                }
                Some(AgentEvent::ResponseChunk {
                    response_id,
                    content,
                    is_thinking,
                })
            }

            FromAgentMessage::ResponseEnd {
                response_id,
                usage,
                tools_summary,
                duration_ms,
                ttft_ms,
            } => {
                if let Some(ref mut response) = self.current_response {
                    if response.response_id == response_id {
                        response.usage = usage.clone();
                    }
                }
                self.last_response_duration_ms = duration_ms;
                self.last_ttft_ms = ttft_ms;
                let response = self.current_response.take();
                Some(AgentEvent::ResponseEnd {
                    response_id,
                    usage,
                    tools_summary,
                    duration_ms,
                    ttft_ms,
                    full_text: response.map(|r| r.text),
                })
            }

            FromAgentMessage::TurnCompleted {
                response_id,
                coding_completion,
                coding_child_records,
            } => {
                self.is_responding = false;
                self.current_response = None;
                self.pending_managed_authorizations.clear();
                Some(AgentEvent::TurnCompleted {
                    response_id,
                    coding_completion,
                    coding_child_records,
                })
            }

            FromAgentMessage::TurnInterrupted {
                response_id,
                reason,
            } => {
                self.is_responding = false;
                self.current_response = None;
                self.pending_managed_authorizations.clear();
                Some(AgentEvent::TurnInterrupted {
                    response_id,
                    reason,
                })
            }

            FromAgentMessage::CodexSessionState {
                state,
                thread_id,
                profile,
            } => Some(AgentEvent::CodexSessionState {
                state,
                thread_id,
                profile,
            }),

            FromAgentMessage::CodexTurnState {
                state,
                thread_id,
                turn_id,
            } => Some(AgentEvent::CodexTurnState {
                state,
                thread_id,
                turn_id,
            }),

            FromAgentMessage::CodexUsageState { source, usage } => {
                Some(AgentEvent::CodexUsageState { source, usage })
            }

            FromAgentMessage::CodexCompatibility {
                protocol_version,
                resume,
                steering,
            } => Some(AgentEvent::CodexCompatibility {
                protocol_version,
                resume,
                steering,
            }),

            FromAgentMessage::ToolCall {
                call_id,
                tool_execution_id,
                tool,
                args,
                requires_approval,
            } => {
                self.tracked_tools.insert(
                    call_id.clone(),
                    PendingApproval {
                        call_id: call_id.clone(),
                        tool_execution_id: tool_execution_id.clone(),
                        request_id: None,
                        tool: tool.clone(),
                        args: args.clone(),
                        started_at_ms: None,
                    },
                );
                if let Some(operation) = codex_subagent_operation(&tool) {
                    self.upsert_codex_subagent_edges(
                        &call_id,
                        tool_execution_id.as_deref(),
                        &tool,
                        Some(&args),
                        active_codex_subagent_status(operation),
                    );
                }
                if requires_approval {
                    self.pending_approvals.push(PendingApproval {
                        call_id: call_id.clone(),
                        tool_execution_id: tool_execution_id.clone(),
                        request_id: None,
                        tool: tool.clone(),
                        args: args.clone(),
                        started_at_ms: None,
                    });
                    Some(AgentEvent::ApprovalRequired {
                        call_id,
                        tool,
                        args,
                    })
                } else {
                    Some(AgentEvent::ToolCall {
                        call_id,
                        tool,
                        args,
                    })
                }
            }

            FromAgentMessage::ToolStart { call_id } => {
                let tool = self
                    .tracked_tools
                    .get(&call_id)
                    .map_or_else(|| "unknown".to_string(), |p| p.tool.clone());

                self.active_tools.insert(
                    call_id.clone(),
                    ActiveTool {
                        call_id: call_id.clone(),
                        tool: tool.clone(),
                        output: String::new(),
                        started: std::time::Instant::now(),
                    },
                );
                Some(AgentEvent::ToolStart { call_id, tool })
            }

            FromAgentMessage::ToolOutput { call_id, content } => {
                if let Some(tool) = self.active_tools.get_mut(&call_id) {
                    tool.output.push_str(&content);
                }
                Some(AgentEvent::ToolOutput { call_id, content })
            }

            FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
                call_id,
                tool_execution_id,
                success,
                tool,
                details,
                receipt,
                ..
            }) => {
                let active_tool = self.active_tools.remove(&call_id);
                let tracked_tool = self.tracked_tools.remove(&call_id);
                let tool_execution_id = tool_execution_id.or_else(|| {
                    tracked_tool
                        .as_ref()
                        .and_then(|source| source.tool_execution_id.clone())
                });
                let source_tool = tool
                    .as_deref()
                    .or_else(|| tracked_tool.as_ref().map(|source| source.tool.as_str()))
                    .or_else(|| active_tool.as_ref().map(|source| source.tool.as_str()));
                if let Some(tool) = source_tool {
                    if let Some(operation) = codex_subagent_operation(tool) {
                        let args = codex_subagent_tool_end_args(
                            tracked_tool.as_ref().map(|source| &source.args),
                            details.as_ref(),
                        );
                        self.upsert_codex_subagent_edges(
                            &call_id,
                            tool_execution_id.as_deref(),
                            tool,
                            args.as_ref(),
                            terminal_codex_subagent_status(operation, success),
                        );
                    }
                }
                self.pending_approvals.retain(|p| p.call_id != call_id);
                self.pending_client_tools.retain(|p| p.call_id != call_id);
                self.governed_client_tool_bindings.remove(&call_id);
                self.pending_user_inputs.retain(|p| p.call_id != call_id);
                self.pending_tool_retries.retain(|p| p.call_id != call_id);
                Some(AgentEvent::ToolEnd {
                    call_id,
                    tool_execution_id,
                    success,
                    duration: active_tool.map(|t| t.started.elapsed()),
                    receipt,
                })
            }

            FromAgentMessage::ClientToolRequest {
                call_id,
                tool_execution_id,
                tool,
                args,
            } => {
                self.tracked_tools.insert(
                    call_id.clone(),
                    PendingApproval {
                        call_id: call_id.clone(),
                        tool_execution_id: tool_execution_id.clone(),
                        request_id: None,
                        tool: tool.clone(),
                        args: args.clone(),
                        started_at_ms: None,
                    },
                );
                if let Some(operation) = codex_subagent_operation(&tool) {
                    self.upsert_codex_subagent_edges(
                        &call_id,
                        tool_execution_id.as_deref(),
                        &tool,
                        Some(&args),
                        active_codex_subagent_status(operation),
                    );
                }
                if tool == "ask_user" {
                    self.pending_user_inputs.retain(|p| p.call_id != call_id);
                    self.pending_user_inputs.push(PendingApproval {
                        call_id,
                        tool_execution_id,
                        request_id: None,
                        tool,
                        args,
                        started_at_ms: None,
                    });
                } else {
                    self.pending_client_tools.retain(|p| p.call_id != call_id);
                    self.pending_client_tools.push(PendingApproval {
                        call_id,
                        tool_execution_id,
                        request_id: None,
                        tool,
                        args,
                        started_at_ms: None,
                    });
                }
                None
            }

            FromAgentMessage::GovernedClientToolRequest(tool_wire::GovernedClientToolRequest {
                call_id,
                tool_execution_id,
                tool,
                args,
                client_instance_id,
                connection_binding_id,
                grant_id,
                grant_version,
                grant_hash,
                turn_digest,
                definition_digest,
                args_digest,
                owner_lease_epoch,
                idempotency_key,
                ..
            }) => {
                self.governed_client_tool_bindings.insert(
                    call_id.clone(),
                    GovernedClientToolBinding {
                        tool_execution_id: tool_execution_id.clone(),
                        client_instance_id,
                        connection_binding_id,
                        grant_id,
                        grant_version,
                        grant_hash,
                        turn_digest,
                        definition_digest,
                        args_digest,
                        owner_lease_epoch,
                        idempotency_key,
                    },
                );
                self.tracked_tools.insert(
                    call_id.clone(),
                    PendingApproval {
                        call_id: call_id.clone(),
                        tool_execution_id: Some(tool_execution_id.clone()),
                        request_id: None,
                        tool: tool.clone(),
                        args: args.clone(),
                        started_at_ms: None,
                    },
                );
                if let Some(operation) = codex_subagent_operation(&tool) {
                    self.upsert_codex_subagent_edges(
                        &call_id,
                        Some(tool_execution_id.as_str()),
                        &tool,
                        Some(&args),
                        active_codex_subagent_status(operation),
                    );
                }
                if tool == "ask_user" {
                    self.pending_user_inputs.retain(|p| p.call_id != call_id);
                    self.pending_user_inputs.push(PendingApproval {
                        call_id,
                        tool_execution_id: Some(tool_execution_id),
                        request_id: None,
                        tool,
                        args,
                        started_at_ms: None,
                    });
                } else {
                    self.pending_client_tools.retain(|p| p.call_id != call_id);
                    self.pending_client_tools.push(PendingApproval {
                        call_id,
                        tool_execution_id: Some(tool_execution_id),
                        request_id: None,
                        tool,
                        args,
                        started_at_ms: None,
                    });
                }
                None
            }

            FromAgentMessage::ServerRequest {
                request_id,
                call_id,
                request_type,
                tool_execution_id,
                tool,
                args,
                started_at_ms,
                ..
            } => {
                let tracked_tool_execution_id = self
                    .tracked_tools
                    .get(&call_id)
                    .and_then(|source| source.tool_execution_id.clone());
                let effective_tool_execution_id = tool_execution_id
                    .clone()
                    .or(tracked_tool_execution_id.clone());
                let has_tracked_tool = self.tracked_tools.contains_key(&call_id);
                let should_track_request = request_type != ServerRequestType::ToolRetry
                    || !has_tracked_tool
                    || tracked_tool_execution_id.as_deref()
                        != effective_tool_execution_id.as_deref();
                if should_track_request {
                    self.tracked_tools.insert(
                        call_id.clone(),
                        PendingApproval {
                            call_id: call_id.clone(),
                            tool_execution_id: effective_tool_execution_id.clone(),
                            request_id: None,
                            tool: tool.clone(),
                            args: args.clone(),
                            started_at_ms,
                        },
                    );
                }
                if let Some(operation) = codex_subagent_operation(&tool) {
                    self.upsert_codex_subagent_edges(
                        &call_id,
                        effective_tool_execution_id.as_deref(),
                        &tool,
                        Some(&args),
                        active_codex_subagent_status(operation),
                    );
                }
                let request_id = if request_id == call_id {
                    None
                } else {
                    Some(request_id)
                };
                match request_type {
                    ServerRequestType::Approval => {
                        self.pending_approvals.retain(|p| p.call_id != call_id);
                        self.pending_approvals.push(PendingApproval {
                            call_id,
                            tool_execution_id: effective_tool_execution_id,
                            request_id,
                            tool,
                            args,
                            started_at_ms,
                        });
                    }
                    ServerRequestType::ClientTool => {
                        self.pending_client_tools.retain(|p| p.call_id != call_id);
                        self.pending_client_tools.push(PendingApproval {
                            call_id,
                            tool_execution_id: effective_tool_execution_id,
                            request_id,
                            tool,
                            args,
                            started_at_ms,
                        });
                    }
                    ServerRequestType::UserInput => {
                        self.pending_user_inputs.retain(|p| p.call_id != call_id);
                        self.pending_user_inputs.push(PendingApproval {
                            call_id,
                            tool_execution_id: effective_tool_execution_id,
                            request_id,
                            tool,
                            args,
                            started_at_ms,
                        });
                    }
                    ServerRequestType::ToolRetry => {
                        self.pending_tool_retries.retain(|p| p.call_id != call_id);
                        self.pending_tool_retries.push(PendingApproval {
                            call_id,
                            tool_execution_id: effective_tool_execution_id,
                            request_id,
                            tool,
                            args,
                            started_at_ms,
                        });
                    }
                }
                None
            }

            FromAgentMessage::ServerRequestResolved {
                request_id,
                call_id,
                request_type,
                resolution,
                ..
            } => {
                match request_type {
                    ServerRequestType::Approval => {
                        self.pending_approvals
                            .retain(|p| !pending_request_matches(p, &request_id));
                        if resolution != ServerRequestResolutionStatus::Approved {
                            self.fail_codex_subagent_edge(&call_id);
                            self.tracked_tools.remove(&call_id);
                        }
                    }
                    ServerRequestType::ClientTool => {
                        self.pending_client_tools
                            .retain(|p| !pending_request_matches(p, &request_id));
                        if resolution == ServerRequestResolutionStatus::Cancelled {
                            self.fail_codex_subagent_edge(&call_id);
                            self.tracked_tools.remove(&call_id);
                        }
                    }
                    ServerRequestType::UserInput => {
                        self.pending_user_inputs
                            .retain(|p| !pending_request_matches(p, &request_id));
                        if resolution != ServerRequestResolutionStatus::Answered {
                            self.fail_codex_subagent_edge(&call_id);
                            self.tracked_tools.remove(&call_id);
                        }
                    }
                    ServerRequestType::ToolRetry => {
                        self.pending_tool_retries
                            .retain(|p| !pending_request_matches(p, &request_id));
                    }
                }
                None
            }

            FromAgentMessage::Error {
                request_id,
                message,
                fatal,
                terminal,
                error_type,
            } => {
                self.last_error = Some(message.clone());
                self.last_error_type = error_type;
                self.provider_error_kind = None;
                if fatal || terminal {
                    self.pending_managed_authorizations.clear();
                    self.is_responding = false;
                    self.current_response = None;
                }
                Some(AgentEvent::Error {
                    request_id,
                    message,
                    fatal,
                    terminal,
                    error_type,
                })
            }

            FromAgentMessage::ProviderError { kind, message } => {
                self.last_error = Some(message.clone());
                self.last_error_type = Some(HeadlessErrorType::Protocol);
                self.provider_error_kind = Some(kind);
                self.is_responding = false;
                self.current_response = None;
                self.pending_managed_authorizations.clear();
                Some(AgentEvent::ProviderError { kind, message })
            }

            FromAgentMessage::Status { message } => {
                self.last_status = Some(message.clone());
                Some(AgentEvent::Status { message })
            }
            FromAgentMessage::DelegationEvent { event } => {
                Some(AgentEvent::DelegationEvent { event })
            }
            FromAgentMessage::Compaction {
                summary,
                first_kept_entry_index,
                tokens_before,
                auto,
                custom_instructions,
                continuation,
                timestamp,
            } => Some(AgentEvent::Compaction {
                summary,
                first_kept_entry_index,
                tokens_before,
                auto,
                custom_instructions,
                continuation,
                timestamp,
            }),
        }
    }

    /// Remove a pending approval (after user decision)
    pub fn remove_pending_approval(&mut self, call_id: &str) -> Option<PendingApproval> {
        let idx = self
            .pending_approvals
            .iter()
            .position(|p| p.call_id == call_id)?;
        Some(self.pending_approvals.remove(idx))
    }
}

fn pending_request_matches(pending: &PendingApproval, request_id: &str) -> bool {
    pending.request_id.as_deref().unwrap_or(&pending.call_id) == request_id
}

/// High-level events for the TUI to react to
#[derive(Debug, Clone)]
pub enum AgentEvent {
    RawAgentEvent {
        event_type: String,
        event: serde_json::Value,
    },
    ManagedGatewayReceipt {
        request_id: String,
        record_id: String,
        lineage_id: String,
        record_status: String,
    },
    WorkspaceCapabilitySetApplied {
        receipt: WorkspaceCapabilitySetApplied,
    },
    DelegationEvent {
        event: maestro_runtime::DelegationEvent,
    },
    Ready {
        protocol_version: Option<String>,
        model: String,
        provider: String,
        session_id: Option<String>,
    },
    SessionInfo {
        session_id: Option<String>,
        cwd: String,
        git_branch: Option<String>,
    },
    ResponseStart {
        response_id: String,
    },
    ResponseChunk {
        response_id: String,
        content: String,
        is_thinking: bool,
    },
    ResponseEnd {
        response_id: String,
        usage: Option<TokenUsage>,
        tools_summary: Option<ResponseToolsSummary>,
        duration_ms: Option<u64>,
        ttft_ms: Option<u64>,
        full_text: Option<String>,
    },
    TurnCompleted {
        response_id: String,
        coding_completion: Option<maestro_runtime::coding_acceptance::CodingCompletionSubmission>,
        coding_child_records: Vec<maestro_runtime::coding_acceptance::CodingAcceptanceChildRecord>,
    },
    TurnInterrupted {
        response_id: String,
        reason: String,
    },
    CodexSessionState {
        state: String,
        thread_id: String,
        profile: String,
    },
    CodexTurnState {
        state: String,
        thread_id: String,
        turn_id: Option<String>,
    },
    CodexUsageState {
        source: String,
        usage: Option<TokenUsage>,
    },
    CodexCompatibility {
        protocol_version: String,
        resume: bool,
        steering: bool,
    },
    ToolCall {
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    ApprovalRequired {
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    ToolStart {
        call_id: String,
        tool: String,
    },
    ToolOutput {
        call_id: String,
        content: String,
    },
    ToolEnd {
        call_id: String,
        tool_execution_id: Option<String>,
        success: bool,
        duration: Option<std::time::Duration>,
        receipt: Option<ExecutionReceipt>,
    },
    Error {
        request_id: Option<String>,
        message: String,
        fatal: bool,
        terminal: bool,
        error_type: Option<HeadlessErrorType>,
    },
    ProviderError {
        kind: maestro_ai::ProviderStreamErrorKind,
        message: String,
    },
    Status {
        message: String,
    },
    Compaction {
        summary: String,
        first_kept_entry_index: usize,
        tokens_before: u64,
        auto: bool,
        custom_instructions: Option<String>,
        continuation: Option<crate::agent::compaction::ContinuationRecord>,
        timestamp: String,
    },
}
