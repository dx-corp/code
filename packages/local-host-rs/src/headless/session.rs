//! Session persistence for the headless protocol.
//!
//! Provides save/load/resume functionality for agent sessions using JSONL
//! (JSON Lines) format. This enables session replay, debugging, and conversation
//! history without requiring a database.
//!
//! # JSONL Format
//!
//! JSONL (JSON Lines) stores one JSON object per line:
//!
//! ```text
//! {"direction":"sent","timestamp":1234567890,"message":{"type":"prompt","content":"Hello"}}
//! {"direction":"received","timestamp":1234567891,"message":{"type":"ready","model":"claude-3-opus","provider":"anthropic"}}
//! {"direction":"received","timestamp":1234567892,"message":{"type":"response_chunk","response_id":"abc","content":"Hi","is_thinking":false}}
//! ```
//!
//! ## Why JSONL?
//!
//! - **Streaming writes** - Append new entries without rewriting entire file
//! - **Partial reads** - Process entries incrementally without loading full file
//! - **Crash recovery** - Previous entries remain valid even if write is interrupted
//! - **Line-based tools** - Compatible with `grep`, `sed`, `wc -l`, etc.
//! - **Human-readable** - Debug sessions with standard text tools
//!
//! ## File Structure
//!
//! Each session creates two files:
//!
//! - `{session_id}.jsonl` - JSONL file with all messages
//! - `{session_id}.meta.json` - JSON file with session metadata
//!
//! The metadata file is updated periodically and contains aggregated statistics
//! like token usage, message count, and session title.
//!
//! # Session Recording
//!
//! The `SessionRecorder` appends entries as they occur:
//!
//! ```rust,ignore
//! use maestro_local_host::headless::session::SessionRecorder;
//! use maestro_local_host::headless::ToAgentMessage;
//!
//! let mut recorder = SessionRecorder::new("/tmp/sessions")?;
//!
//! recorder.record_sent(&ToAgentMessage::Prompt {
//!     content: "Hello!".to_string(),
//!     attachments: None,
//!     managed_inference_authorization: None,
//! })?;
//!
//! recorder.flush()?; // Ensure writes are persisted
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Session Replay
//!
//! The `SessionReader` loads all entries from a session file:
//!
//! ```rust,ignore
//! use maestro_local_host::headless::session::SessionReader;
//!
//! let reader = SessionReader::load("/tmp/sessions", "session-id")?;
//!
//! println!("Session: {}", reader.metadata().title.as_deref().unwrap_or("Untitled"));
//! println!("Messages: {}", reader.entries().len());
//!
//! for prompt in reader.prompts() {
//!     println!("User: {}", prompt);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Buffered Writes
//!
//! `SessionRecorder` uses a `BufWriter` to batch writes, reducing filesystem
//! overhead. The buffer is automatically flushed:
//!
//! - When `flush()` is called explicitly
//! - When the recorder is dropped (via `Drop` implementation)
//! - When the internal buffer fills (typically 8KB)
//!
//! For reliability, call `flush()` after important events to ensure data is
//! persisted to disk.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::agent::process_budget::ProcessBudgetState;
use crate::fs_atomic::create_dir_all_synced;

use super::messages::{
    ActiveFileWatch, ActiveTool, AgentState, CodexSubagentContinuityEdge, FromAgentMessage,
    GovernedClientToolBinding, InitConfig, PendingApproval, ServerRequestType, StreamingResponse,
    ToAgentMessage, TokenUsage,
};
#[cfg(test)]
use super::workspace_capabilities::WorkspaceCapabilitySetApplied;
use super::workspace_capabilities::{
    ApplyWorkspaceCapabilitySet, accept_workspace_capability_receipt,
};

fn workspace_capability_replay_cursor(request: &ApplyWorkspaceCapabilitySet) -> String {
    format!(
        "{}:{}",
        request.activation_generation, request.capability_set_digest
    )
}

fn record_sent_workspace_capability(
    pending: &mut HashMap<String, ApplyWorkspaceCapabilitySet>,
    message: &ToAgentMessage,
) {
    if let ToAgentMessage::ApplyWorkspaceCapabilitySet { request } = message {
        pending.insert(workspace_capability_replay_cursor(request), request.clone());
    }
}

fn init_config_from_message(message: &ToAgentMessage) -> Option<InitConfig> {
    match message {
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
        _ => None,
    }
}

/// A recorded session entry (either a sent or received message).
///
/// Represents a single message in the session history, tagged with direction
/// (sent to agent or received from agent) and timestamp.
///
/// # Serialization Format
///
/// Uses serde's `tag` attribute to add a `direction` discriminator:
///
/// ```json
/// {"direction":"sent","timestamp":1234567890,"message":{"type":"prompt","content":"Hello"}}
/// {"direction":"received","timestamp":1234567891,"message":{"type":"ready","model":"claude-3-opus","provider":"anthropic"}}
/// ```
///
/// # Timestamp Format
///
/// Timestamps are Unix milliseconds (milliseconds since 1970-01-01 00:00:00 UTC).
/// This provides millisecond precision for accurate timing analysis while remaining
/// compact and sortable as a numeric value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case")]
pub enum SessionEntry {
    /// Message sent to the agent
    Sent {
        timestamp: u64,
        message: ToAgentMessage,
    },
    /// Message received from the agent
    Received {
        timestamp: u64,
        message: FromAgentMessage,
    },
    /// Periodic checkpoint of reconstructed runtime state.
    Checkpoint {
        timestamp: u64,
        state: Box<AgentStateCheckpoint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_init: Option<InitConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        semantic_conversation: Option<SemanticConversationCheckpoint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_workspace_capability_set: Option<Box<ApplyWorkspaceCapabilitySet>>,
        /// Latest native process budget observation. This is evidence for
        /// recovery diagnostics only; it is never restored as authority.
        #[serde(default)]
        last_process_budget: Option<ProcessBudgetState>,
    },
}

impl SessionEntry {
    /// Create a sent entry with current timestamp
    #[must_use]
    pub fn sent(message: ToAgentMessage) -> Self {
        SessionEntry::Sent {
            timestamp: current_timestamp(),
            message,
        }
    }

    /// Create a received entry with current timestamp
    #[must_use]
    pub fn received(message: FromAgentMessage) -> Self {
        SessionEntry::Received {
            timestamp: current_timestamp(),
            message,
        }
    }

    /// Get the timestamp of this entry
    #[must_use]
    pub fn timestamp(&self) -> u64 {
        match self {
            SessionEntry::Sent { timestamp, .. } => *timestamp,
            SessionEntry::Received { timestamp, .. } => *timestamp,
            SessionEntry::Checkpoint { timestamp, .. } => *timestamp,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveToolCheckpoint {
    pub call_id: String,
    pub tool: String,
    pub output: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticConversationCheckpoint {
    protocol_version: String,
    messages: Vec<maestro_ai::Message>,
}

/// Atomically replaced latest replay state. The JSONL remains the complete
/// ordinary-event journal for readers; this sidecar is only the restore anchor
/// so day-scale resumes do not deserialize historical checkpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplaySidecar {
    tail_offset: u64,
    state: AgentStateCheckpoint,
    last_init: Option<InitConfig>,
    semantic_conversation: Option<SemanticConversationCheckpoint>,
    #[serde(default)]
    last_workspace_capability_set: Option<ApplyWorkspaceCapabilitySet>,
    #[serde(default)]
    semantic_conversation_attempt: Option<u32>,
    #[serde(default)]
    semantic_processed_queue_ids: Vec<u64>,
    /// Latest native process budget observation. This is evidence for
    /// recovery diagnostics only; it is never restored as authority.
    #[serde(default)]
    last_process_budget: Option<ProcessBudgetState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveUtilityCommandCheckpoint {
    pub command_id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub shell_mode: super::messages::UtilityCommandShellMode,
    pub terminal_mode: super::messages::UtilityCommandTerminalMode,
    pub pid: Option<u32>,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    pub owner_connection_id: Option<String>,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveFileWatchCheckpoint {
    pub watch_id: String,
    pub root_dir: String,
    pub include_patterns: Option<Vec<String>>,
    pub exclude_patterns: Option<Vec<String>>,
    pub debounce_ms: u32,
    pub owner_connection_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentStateCheckpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_binding_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_binding_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<super::messages::ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<super::messages::ClientCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_capabilities: Option<super::messages::ServerCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_out_notifications: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_role: Option<super::messages::ConnectionRole>,
    #[serde(default)]
    pub connection_count: usize,
    #[serde(default)]
    pub subscriber_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_subscription_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_connection_id: Option<String>,
    #[serde(default)]
    pub connections: Vec<super::messages::ConnectionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_response: Option<StreamingResponse>,
    #[serde(default)]
    pub pending_approvals: Vec<PendingApproval>,
    #[serde(default)]
    pub pending_client_tools: Vec<PendingApproval>,
    #[serde(default)]
    pub governed_client_tool_bindings: HashMap<String, GovernedClientToolBinding>,
    #[serde(default)]
    pub pending_user_inputs: Vec<PendingApproval>,
    #[serde(default)]
    pub pending_tool_retries: Vec<PendingApproval>,
    #[serde(default)]
    pub tracked_tools: Vec<PendingApproval>,
    #[serde(default)]
    pub active_tools: Vec<ActiveToolCheckpoint>,
    #[serde(default)]
    pub codex_subagent_edges: Vec<CodexSubagentContinuityEdge>,
    #[serde(default)]
    pub active_utility_commands: Vec<ActiveUtilityCommandCheckpoint>,
    #[serde(default)]
    pub active_file_watches: Vec<ActiveFileWatchCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_type: Option<super::messages::HeadlessErrorType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_error_kind: Option<maestro_ai::ProviderStreamErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_response_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ttft_ms: Option<u64>,
    #[serde(default)]
    pub is_ready: bool,
    #[serde(default)]
    pub is_responding: bool,
}

impl AgentStateCheckpoint {
    #[must_use]
    pub fn from_state(state: &AgentState) -> Self {
        Self {
            protocol_version: state.protocol_version.clone(),
            client_protocol_version: state.client_protocol_version.clone(),
            controller_binding_version: state.controller_binding_version.clone(),
            controller_binding_sha256: state.controller_binding_sha256.clone(),
            client_info: state.client_info.clone(),
            capabilities: state.capabilities.clone(),
            server_capabilities: state.server_capabilities.clone(),
            opt_out_notifications: state.opt_out_notifications.clone(),
            connection_role: state.connection_role,
            connection_count: state.connection_count,
            subscriber_count: state.subscriber_count,
            controller_subscription_id: state.controller_subscription_id.clone(),
            controller_connection_id: state.controller_connection_id.clone(),
            connections: state.connections.clone(),
            model: state.model.clone(),
            provider: state.provider.clone(),
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
            git_branch: state.git_branch.clone(),
            current_response: state.current_response.clone(),
            pending_approvals: state.pending_approvals.clone(),
            pending_client_tools: state.pending_client_tools.clone(),
            governed_client_tool_bindings: state.governed_client_tool_bindings.clone(),
            pending_user_inputs: state.pending_user_inputs.clone(),
            pending_tool_retries: state.pending_tool_retries.clone(),
            tracked_tools: state.tracked_tools.values().cloned().collect(),
            active_tools: state
                .active_tools
                .values()
                .map(|tool| ActiveToolCheckpoint {
                    call_id: tool.call_id.clone(),
                    tool: tool.tool.clone(),
                    output: tool.output.clone(),
                    elapsed_ms: tool.started.elapsed().as_millis() as u64,
                })
                .collect(),
            codex_subagent_edges: state.codex_subagent_edges.clone(),
            active_utility_commands: state
                .active_utility_commands
                .values()
                .map(|command| ActiveUtilityCommandCheckpoint {
                    command_id: command.command_id.clone(),
                    command: command.command.clone(),
                    cwd: command.cwd.clone(),
                    shell_mode: command.shell_mode,
                    terminal_mode: command.terminal_mode,
                    pid: command.pid,
                    columns: command.columns,
                    rows: command.rows,
                    owner_connection_id: command.owner_connection_id.clone(),
                    output: command.output.clone(),
                })
                .collect(),
            active_file_watches: state
                .active_file_watches
                .values()
                .map(|watch| ActiveFileWatchCheckpoint {
                    watch_id: watch.watch_id.clone(),
                    root_dir: watch.root_dir.clone(),
                    include_patterns: watch.include_patterns.clone(),
                    exclude_patterns: watch.exclude_patterns.clone(),
                    debounce_ms: watch.debounce_ms,
                    owner_connection_id: watch.owner_connection_id.clone(),
                })
                .collect(),
            last_error: state.last_error.clone(),
            last_error_type: state.last_error_type,
            provider_error_kind: state.provider_error_kind,
            last_status: state.last_status.clone(),
            last_response_duration_ms: state.last_response_duration_ms,
            last_ttft_ms: state.last_ttft_ms,
            is_ready: state.is_ready,
            is_responding: state.is_responding,
        }
    }

    #[must_use]
    pub fn into_state(self) -> AgentState {
        AgentState {
            pending_managed_authorizations: Vec::new(),
            protocol_version: self.protocol_version,
            client_protocol_version: self.client_protocol_version,
            controller_binding_version: self.controller_binding_version,
            controller_binding_sha256: self.controller_binding_sha256,
            client_info: self.client_info,
            capabilities: self.capabilities,
            server_capabilities: self.server_capabilities,
            opt_out_notifications: self.opt_out_notifications,
            connection_role: self.connection_role,
            connection_count: self.connection_count,
            subscriber_count: self.subscriber_count,
            controller_subscription_id: self.controller_subscription_id,
            controller_connection_id: self.controller_connection_id,
            connections: self.connections,
            model: self.model,
            provider: self.provider,
            session_id: self.session_id,
            cwd: self.cwd,
            git_branch: self.git_branch,
            current_response: self.current_response,
            pending_approvals: self.pending_approvals,
            pending_client_tools: self.pending_client_tools,
            governed_client_tool_bindings: self.governed_client_tool_bindings,
            pending_user_inputs: self.pending_user_inputs,
            pending_tool_retries: self.pending_tool_retries,
            tracked_tools: self
                .tracked_tools
                .into_iter()
                .map(|tool| (tool.call_id.clone(), tool))
                .collect::<HashMap<_, _>>(),
            active_tools: self
                .active_tools
                .into_iter()
                .map(|tool| {
                    let started = std::time::Instant::now()
                        .checked_sub(Duration::from_millis(tool.elapsed_ms))
                        .unwrap_or_else(std::time::Instant::now);
                    (
                        tool.call_id.clone(),
                        ActiveTool {
                            call_id: tool.call_id,
                            tool: tool.tool,
                            output: tool.output,
                            started,
                        },
                    )
                })
                .collect(),
            codex_subagent_edges: self.codex_subagent_edges,
            active_utility_commands: self
                .active_utility_commands
                .into_iter()
                .map(|command| {
                    (
                        command.command_id.clone(),
                        super::messages::ActiveUtilityCommand {
                            command_id: command.command_id,
                            command: command.command,
                            cwd: command.cwd,
                            shell_mode: command.shell_mode,
                            terminal_mode: command.terminal_mode,
                            pid: command.pid,
                            columns: command.columns,
                            rows: command.rows,
                            owner_connection_id: command.owner_connection_id,
                            output: command.output,
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
            active_file_watches: self
                .active_file_watches
                .into_iter()
                .map(|watch| {
                    (
                        watch.watch_id.clone(),
                        ActiveFileWatch {
                            watch_id: watch.watch_id,
                            root_dir: watch.root_dir,
                            include_patterns: watch.include_patterns,
                            exclude_patterns: watch.exclude_patterns,
                            debounce_ms: watch.debounce_ms,
                            owner_connection_id: watch.owner_connection_id,
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
            last_error: self.last_error,
            last_error_type: self.last_error_type,
            provider_error_kind: self.provider_error_kind,
            last_status: self.last_status,
            last_response_duration_ms: self.last_response_duration_ms,
            last_ttft_ms: self.last_ttft_ms,
            is_ready: self.is_ready,
            is_responding: self.is_responding,
        }
    }
}

/// Session metadata stored in a separate file.
///
/// Contains aggregated statistics and metadata about a session, stored as a
/// separate JSON file alongside the JSONL message log.
///
/// # Purpose
///
/// The metadata file enables:
/// - **Fast session listing** - Read metadata without parsing JSONL
/// - **Session search** - Find sessions by title, model, or date
/// - **Usage tracking** - Aggregate token counts and costs
/// - **Session preview** - Display title and stats without full load
///
/// # Update Strategy
///
/// Metadata is updated incrementally as messages are recorded and flushed
/// to disk when:
/// - `SessionRecorder::flush()` is called
/// - The recorder is dropped
///
/// This ensures metadata stays synchronized with the message log even if
/// the process crashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Session ID
    pub id: String,
    /// When the session was created
    pub created_at: u64,
    /// When the session was last updated
    pub updated_at: u64,
    /// Session title (first user message, truncated)
    pub title: Option<String>,
    /// Model used in this session
    pub model: Option<String>,
    /// Provider used in this session
    pub provider: Option<String>,
    /// Headless protocol version reported by the agent
    #[serde(default)]
    pub protocol_version: Option<String>,
    /// Session ID reported by the agent itself
    #[serde(default)]
    pub agent_session_id: Option<String>,
    /// Working directory
    pub cwd: Option<String>,
    /// Git branch (if any)
    pub git_branch: Option<String>,
    /// Total input tokens used
    pub total_input_tokens: u64,
    /// Total output tokens used
    pub total_output_tokens: u64,
    /// Number of messages in session
    pub message_count: usize,
}

impl SessionMetadata {
    /// Create new session metadata
    pub fn new(id: impl Into<String>) -> Self {
        let now = current_timestamp();
        Self {
            id: id.into(),
            created_at: now,
            updated_at: now,
            title: None,
            model: None,
            provider: None,
            protocol_version: None,
            agent_session_id: None,
            cwd: None,
            git_branch: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            message_count: 0,
        }
    }

    /// Update title from first user prompt
    pub fn set_title_from_prompt(&mut self, content: &str) {
        if self.title.is_none() {
            let title = content.lines().next().unwrap_or(content);
            let chars: Vec<char> = title.chars().collect();
            let title = if chars.len() > 80 {
                format!("{}...", chars[..77].iter().collect::<String>())
            } else {
                title.to_string()
            };
            self.title = Some(title);
        }
    }

    /// Update token usage
    pub fn add_usage(&mut self, usage: &TokenUsage) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
    }
}

/// Session recorder - appends entries to a JSONL file.
///
/// Provides append-only recording of session messages to a JSONL file, with
/// automatic metadata tracking and buffered writes for performance.
///
/// # Lifecycle
///
/// 1. Create with `new()` or `resume()` an existing session
/// 2. Record messages with `record_sent()` and `record_received()`
/// 3. Flush periodically with `flush()` to persist to disk
/// 4. Automatic cleanup on drop (flushes remaining data)
///
/// # Buffering
///
/// Uses a `BufWriter` internally to batch writes. This significantly improves
/// performance for high-frequency message streams by reducing syscall overhead.
///
/// Call `flush()` explicitly after important events to ensure data is persisted,
/// especially before operations that might crash or terminate the process.
///
/// # File Safety
///
/// - Opens files in append mode (`OpenOptions::append(true)`)
/// - Creates parent directories automatically
/// - Flushes on drop to prevent data loss
/// - Metadata is written atomically (overwrites entire file)
///
/// # Examples
///
/// ```rust,ignore
/// use maestro_local_host::headless::session::SessionRecorder;
/// use maestro_local_host::headless::ToAgentMessage;
///
/// let mut recorder = SessionRecorder::new("/tmp/sessions")?;
/// println!("Session ID: {}", recorder.id());
///
/// recorder.record_sent(&ToAgentMessage::Prompt {
///     content: "Hello".to_string(),
///     attachments: None,
///     managed_inference_authorization: None,
/// })?;
///
/// recorder.flush()?; // Ensure persistence
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct SessionRecorder {
    /// Session ID
    id: String,
    /// Path to the session JSONL file
    path: PathBuf,
    /// Buffered writer for appending entries
    writer: BufWriter<File>,
    /// Session metadata
    metadata: SessionMetadata,
    /// Path to metadata file
    metadata_path: PathBuf,
    /// Atomically overwritten latest replay/semantic anchor.
    replay_path: PathBuf,
    /// Reconstructed state including optimistic outbound actions.
    replay_state: AgentState,
    /// Last init message seen in the stream.
    last_init: Option<InitConfig>,
    /// Latest private, sanitized provider conversation checkpoint.
    semantic_conversation: Option<SemanticConversationCheckpoint>,
    /// Last request paired with a matching applied receipt.
    last_workspace_capability_set: Option<ApplyWorkspaceCapabilitySet>,
    /// Capability requests sent since their matching receipt boundary.
    pending_workspace_capability_sets: HashMap<String, ApplyWorkspaceCapabilitySet>,
    /// Attempt identity associated with the latest semantic checkpoint.
    semantic_conversation_attempt: Option<u32>,
    /// Explicit prompt queue ids known to be covered by semantic checkpoints.
    semantic_processed_queue_ids: HashSet<u64>,
    /// Latest native process budget observation, retained as evidence only.
    last_process_budget: Option<ProcessBudgetState>,
    /// Number of entries written since the last checkpoint.
    entries_since_checkpoint: usize,
}

const CHECKPOINT_INTERVAL: usize = 25;

fn without_ephemeral_managed_authorization(message: &ToAgentMessage) -> ToAgentMessage {
    let mut durable = message.clone();
    match &mut durable {
        ToAgentMessage::Prompt {
            managed_inference_authorization,
            ..
        }
        | ToAgentMessage::GovernedPrompt {
            managed_inference_authorization,
            ..
        }
        | ToAgentMessage::Steer {
            managed_inference_authorization,
            ..
        }
        | ToAgentMessage::GovernedSteer {
            managed_inference_authorization,
            ..
        } => *managed_inference_authorization = None,
        _ => {}
    }
    durable
}

/// Load `<id>.meta.json`, tolerating a corrupt or torn file left by a crash
/// mid-write.
///
/// A missing file yields fresh default metadata (normal for a brand-new
/// session). A file that exists but fails to parse is forensic evidence of
/// real damage (the pre-atomic-write code path here used a direct
/// `fs::write`, so a `kill -9` mid-write could truncate it); rather than
/// silently discarding that evidence or hard-failing the caller, the file is
/// rotated aside (`<id>.meta.json.corrupt.<millis>`) and fresh default
/// metadata is returned so `resume`/`list_sessions` still see the session
/// instead of treating it as gone. Only I/O errors (permissions, etc.) are
/// propagated as hard errors.
///
/// The returned flag is `true` when the metadata was reconstructed after
/// corruption, so callers can rebuild the lost fields from the still-intact
/// JSONL log via [`rebuild_metadata`].
fn load_metadata_tolerant(
    metadata_path: &Path,
    id: &str,
) -> std::io::Result<(SessionMetadata, bool)> {
    if !metadata_path.exists() {
        return Ok((SessionMetadata::new(id), false));
    }
    // Read bytes, not `read_to_string`: a file torn mid-write by a crash
    // (the whole reason this function tolerates a parse failure) can just
    // as easily be torn in the middle of a multi-byte UTF-8 character as
    // in the middle of a JSON token. `read_to_string` would then fail with
    // `InvalidData` from the `?` above before the tolerant JSON-parsing
    // branch below ever runs, hard-failing the caller instead of rotating
    // the file aside and reconstructing defaults like every other kind of
    // corruption here.
    let content = fs::read(metadata_path)?;
    match serde_json::from_slice(&content) {
        Ok(metadata) => Ok((metadata, false)),
        Err(err) => {
            eprintln!(
                "Corrupt session metadata at {}: {err}. Rotating aside and reconstructing defaults.",
                metadata_path.display()
            );
            crate::fs_atomic::rotate_corrupt_aside(metadata_path);
            Ok((SessionMetadata::new(id), true))
        }
    }
}

/// Reconstruct metadata by folding the JSONL log entries, mirroring the
/// incremental updates `record_sent`/`record_received` apply. Used when the
/// persisted metadata file was corrupt and had to be rotated aside, so the
/// historical title, model, token totals, and message count are not
/// permanently reset to defaults.
fn rebuild_metadata(id: &str, entries: &[SessionEntry]) -> SessionMetadata {
    let mut metadata = SessionMetadata::new(id);
    if let Some(first) = entries.first() {
        metadata.created_at = first.timestamp();
    }
    for entry in entries {
        match entry {
            SessionEntry::Sent { message, .. } => {
                if let ToAgentMessage::Prompt { content, .. } = message {
                    metadata.set_title_from_prompt(content);
                }
                metadata.message_count += 1;
            }
            SessionEntry::Received { message, .. } => {
                match message {
                    FromAgentMessage::Ready {
                        protocol_version,
                        model,
                        provider,
                        session_id,
                    } => {
                        metadata.model = Some(model.clone());
                        metadata.provider = Some(provider.clone());
                        metadata.protocol_version = protocol_version.clone();
                        if session_id.is_some() {
                            metadata.agent_session_id = session_id.clone();
                        }
                    }
                    FromAgentMessage::SessionInfo {
                        session_id,
                        cwd,
                        git_branch,
                    } => {
                        if session_id.is_some() {
                            metadata.agent_session_id = session_id.clone();
                        }
                        metadata.cwd = Some(cwd.clone());
                        metadata.git_branch = git_branch.clone();
                    }
                    FromAgentMessage::ResponseEnd {
                        usage: Some(usage), ..
                    } => {
                        metadata.add_usage(usage);
                    }
                    _ => {}
                }
                metadata.message_count += 1;
            }
            SessionEntry::Checkpoint { .. } => {}
        }
        metadata.updated_at = entry.timestamp();
    }
    metadata
}

impl SessionRecorder {
    /// Create a new session recorder
    pub fn new(sessions_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        Self::with_id(sessions_dir, &id)
    }

    /// Create a session recorder with a specific ID
    pub fn with_id(sessions_dir: impl AsRef<Path>, id: &str) -> std::io::Result<Self> {
        let sessions_dir = sessions_dir.as_ref();
        create_dir_all_synced(sessions_dir)?;

        let path = sessions_dir.join(format!("{id}.jsonl"));
        let metadata_path = sessions_dir.join(format!("{id}.meta.json"));
        let replay_path = sessions_dir.join(format!("{id}.replay.json"));

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let writer = BufWriter::new(file);

        let metadata = SessionMetadata::new(id);

        Ok(Self {
            id: id.to_string(),
            path,
            writer,
            metadata,
            metadata_path,
            replay_path,
            replay_state: AgentState::default(),
            last_init: None,
            semantic_conversation: None,
            last_workspace_capability_set: None,
            pending_workspace_capability_sets: HashMap::new(),
            semantic_conversation_attempt: None,
            semantic_processed_queue_ids: HashSet::new(),
            last_process_budget: None,
            entries_since_checkpoint: 0,
        })
    }

    /// Resume an existing session
    pub fn resume(sessions_dir: impl AsRef<Path>, id: &str) -> std::io::Result<Self> {
        let sessions_dir = sessions_dir.as_ref();
        create_dir_all_synced(sessions_dir)?;
        let path = sessions_dir.join(format!("{id}.jsonl"));
        let metadata_path = sessions_dir.join(format!("{id}.meta.json"));
        let replay_path = sessions_dir.join(format!("{id}.replay.json"));

        // Load existing metadata
        let (mut metadata, reconstructed) = load_metadata_tolerant(&metadata_path, id)?;

        let loaded_sidecar = load_replay_sidecar(&replay_path);
        let semantic_conversation_attempt = loaded_sidecar
            .as_ref()
            .and_then(|(sidecar, _)| sidecar.semantic_conversation_attempt);
        let semantic_processed_queue_ids = loaded_sidecar
            .as_ref()
            .map(|(sidecar, _)| {
                sidecar
                    .semantic_processed_queue_ids
                    .iter()
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        // A pre-budget sidecar can point past an older budget receipt. The
        // sidecar remains authoritative for semantic history; replay_from_
        // sidecar_tail scans the journal prefix only for the missing evidence
        // before applying the sidecar tail.
        let sidecar_replay = loaded_sidecar.and_then(|(sidecar, has_process_budget_field)| {
            replay_from_sidecar_tail(&path, sidecar, has_process_budget_field).ok()
        });
        let reader = if sidecar_replay.is_none() || reconstructed {
            SessionReader::load(sessions_dir, id).ok()
        } else {
            None
        };
        if reconstructed {
            // The corrupt metadata was rotated aside; rebuild what it held
            // (title, model, usage, counts) from the still-intact JSONL log
            // so the next flush doesn't permanently reset them to defaults.
            if let Some(reader) = &reader {
                metadata = rebuild_metadata(id, reader.entries());
            }
        }
        let replay = sidecar_replay.or_else(|| reader.map(|reader| reader.replay()));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let writer = BufWriter::new(file);

        Ok(Self {
            id: id.to_string(),
            path,
            writer,
            metadata,
            metadata_path,
            replay_path,
            replay_state: replay
                .as_ref()
                .map_or_else(AgentState::default, |replay| replay.state.clone()),
            last_init: replay.as_ref().and_then(|replay| replay.last_init.clone()),
            last_workspace_capability_set: replay
                .as_ref()
                .and_then(|replay| replay.last_workspace_capability_set.clone()),
            last_process_budget: replay
                .as_ref()
                .and_then(|replay| replay.last_process_budget.clone()),
            pending_workspace_capability_sets: HashMap::new(),
            semantic_conversation: replay.and_then(|replay| {
                replay
                    .semantic_conversation
                    .map(|messages| SemanticConversationCheckpoint {
                        protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                            .to_string(),
                        messages,
                    })
            }),
            semantic_conversation_attempt,
            semantic_processed_queue_ids,
            entries_since_checkpoint: 0,
        })
    }

    /// Get the session ID
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the path to the session JSONL file
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the session metadata
    #[must_use]
    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    /// Get the current replay state tracked by the recorder.
    #[must_use]
    pub fn replay_state(&self) -> &AgentState {
        &self.replay_state
    }

    /// Get the last init config tracked by the recorder.
    #[must_use]
    pub fn last_init(&self) -> Option<&InitConfig> {
        self.last_init.as_ref()
    }

    /// Return the current restore-ready session replay snapshot.
    #[must_use]
    pub fn replay(&self) -> SessionReplay {
        SessionReplay {
            state: self.replay_state.clone(),
            last_init: self.last_init.clone(),
            semantic_conversation: self
                .semantic_conversation
                .as_ref()
                .filter(|checkpoint| {
                    checkpoint.protocol_version
                        == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                })
                .map(|checkpoint| checkpoint.messages.clone()),
            last_workspace_capability_set: self.last_workspace_capability_set.clone(),
            last_process_budget: self.last_process_budget.clone(),
        }
    }

    /// Return the attempt identity stored with the latest semantic checkpoint.
    #[must_use]
    pub(crate) fn semantic_conversation_attempt(&self) -> Option<u32> {
        self.semantic_conversation_attempt
    }

    #[must_use]
    pub(crate) fn semantic_processed_queue_ids(&self) -> &HashSet<u64> {
        &self.semantic_processed_queue_ids
    }

    /// Replace the reconstructed replay state with a snapshot and persist it.
    pub fn apply_snapshot(
        &mut self,
        state: AgentState,
        last_init: Option<InitConfig>,
    ) -> std::io::Result<()> {
        self.replay_state = state;
        self.last_init = last_init;
        self.metadata.model = self.replay_state.model.clone();
        self.metadata.provider = self.replay_state.provider.clone();
        self.metadata.protocol_version = self.replay_state.protocol_version.clone();
        if self.replay_state.session_id.is_some() {
            self.metadata.agent_session_id = self.replay_state.session_id.clone();
        }
        self.metadata.cwd = self.replay_state.cwd.clone();
        self.metadata.git_branch = self.replay_state.git_branch.clone();
        self.metadata.updated_at = current_timestamp();
        self.maybe_write_checkpoint(true)
    }

    /// Record the newest private provider conversation for the next durable
    /// checkpoint. The checkpoint payload is sanitized before it reaches disk.
    pub fn record_semantic_conversation(
        &mut self,
        messages: Vec<maestro_ai::Message>,
    ) -> std::io::Result<()> {
        self.semantic_conversation = Some(SemanticConversationCheckpoint {
            protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_string(),
            messages: portable_redacted_semantic_conversation(&sanitize_semantic_conversation(
                &messages,
            ))?,
        });
        Ok(())
    }

    /// Force a durable checkpoint, including any semantic conversation snapshot.
    pub fn flush_checkpoint(&mut self) -> std::io::Result<()> {
        self.maybe_write_checkpoint(true)?;
        self.flush()
    }

    /// Record a sent message
    pub fn record_sent(&mut self, message: &ToAgentMessage) -> std::io::Result<()> {
        // A renewal result is an ephemeral credential, not replayable state.
        // Requests and Gateway receipts retain content-free correlation.
        if matches!(message, ToAgentMessage::ManagedAuthorizationResult { .. }) {
            return Ok(());
        }
        let entry = SessionEntry::sent(without_ephemeral_managed_authorization(message));
        self.write_entry(&entry)?;
        self.replay_state.handle_sent_message(message);
        record_sent_workspace_capability(&mut self.pending_workspace_capability_sets, message);
        if let Some(config) = init_config_from_message(message) {
            self.last_init = Some(config);
        }
        self.entries_since_checkpoint += 1;
        self.maybe_write_checkpoint(false)?;

        // Update metadata
        if let ToAgentMessage::Prompt { content, .. } = message {
            self.metadata.set_title_from_prompt(content);
        }
        self.metadata.message_count += 1;
        self.metadata.updated_at = current_timestamp();

        Ok(())
    }

    /// Record a received message
    pub fn record_received(&mut self, message: &FromAgentMessage) -> std::io::Result<()> {
        let portable_message = portable_redacted_message(message)?;
        if let FromAgentMessage::ConversationSnapshot {
            protocol_version,
            messages,
            processed_queue_ids,
        } = &portable_message
        {
            return self.record_conversation_snapshot(
                protocol_version,
                messages,
                None,
                processed_queue_ids,
            );
        }
        let entry = SessionEntry::received(portable_message);
        self.write_entry(&entry)?;
        let workspace_capability_accepted =
            if let FromAgentMessage::WorkspaceCapabilitySetApplied { receipt } = message {
                accept_workspace_capability_receipt(
                    &mut self.pending_workspace_capability_sets,
                    &mut self.last_workspace_capability_set,
                    receipt,
                )
            } else {
                false
            };
        if let FromAgentMessage::ProcessBudgetCheckpoint { budget } = message {
            // Keep the native checkpoint as evidence for resume diagnostics;
            // this never installs it into the live agent or grants authority.
            self.last_process_budget = Some(budget.clone());
        }
        if !matches!(
            message,
            FromAgentMessage::WorkspaceCapabilitySetApplied { .. }
        ) || workspace_capability_accepted
        {
            let _ = self.replay_state.handle_message(message.clone());
        }
        self.entries_since_checkpoint += 1;
        self.maybe_write_checkpoint(
            matches!(
                message,
                FromAgentMessage::ProcessBudgetCheckpoint { .. }
                    | FromAgentMessage::ResponseEnd { .. }
                    | FromAgentMessage::Error { .. }
                    | FromAgentMessage::Compaction { .. }
                    | FromAgentMessage::ConversationSnapshot { .. }
            ) || workspace_capability_accepted,
        )?;

        // Update metadata
        match message {
            FromAgentMessage::Ready {
                protocol_version,
                model,
                provider,
                session_id,
            } => {
                self.metadata.model = Some(model.clone());
                self.metadata.provider = Some(provider.clone());
                self.metadata.protocol_version = protocol_version.clone();
                if session_id.is_some() {
                    self.metadata.agent_session_id = session_id.clone();
                }
            }
            FromAgentMessage::SessionInfo {
                session_id,
                cwd,
                git_branch,
            } => {
                if session_id.is_some() {
                    self.metadata.agent_session_id = session_id.clone();
                }
                self.metadata.cwd = Some(cwd.clone());
                self.metadata.git_branch = git_branch.clone();
            }
            FromAgentMessage::ResponseEnd {
                usage: Some(usage), ..
            } => {
                self.metadata.add_usage(usage);
            }
            _ => {}
        }
        self.metadata.message_count += 1;
        self.metadata.updated_at = current_timestamp();

        Ok(())
    }

    /// Record a received message whose semantic snapshot has already been
    /// vaulted by the owning session.
    ///
    /// Portable transcript export intentionally masks credential references.
    /// Delegated child checkpoints are different: they must retain the live
    /// opaque references so an in-process resume can resolve them from the
    /// child credential scope. Callers must vault the message before using
    /// this method; non-snapshot messages continue through the portable path.
    pub fn record_received_preserving_credential_references(
        &mut self,
        message: &FromAgentMessage,
    ) -> std::io::Result<()> {
        self.record_received_preserving_credential_references_with_snapshot_attempt(message, None)
    }

    /// Record a received message while atomically associating semantic snapshots
    /// with the child attempt that produced them.
    pub(crate) fn record_received_preserving_credential_references_with_snapshot_attempt(
        &mut self,
        message: &FromAgentMessage,
        snapshot_attempt: Option<u32>,
    ) -> std::io::Result<()> {
        self.record_received_preserving_credential_references_with_snapshot_metadata(
            message,
            snapshot_attempt,
            &[],
        )
    }

    pub(crate) fn record_received_preserving_credential_references_with_snapshot_metadata(
        &mut self,
        message: &FromAgentMessage,
        snapshot_attempt: Option<u32>,
        processed_queue_ids: &[u64],
    ) -> std::io::Result<()> {
        match message {
            FromAgentMessage::ConversationSnapshot {
                protocol_version,
                messages,
                ..
            } => self.record_conversation_snapshot(
                protocol_version,
                messages,
                snapshot_attempt,
                processed_queue_ids,
            ),
            _ => self.record_received(message),
        }
    }

    fn record_conversation_snapshot(
        &mut self,
        protocol_version: &str,
        messages: &[maestro_ai::Message],
        snapshot_attempt: Option<u32>,
        processed_queue_ids: &[u64],
    ) -> std::io::Result<()> {
        self.semantic_conversation_attempt = snapshot_attempt;
        if protocol_version == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL {
            self.semantic_processed_queue_ids
                .extend(processed_queue_ids.iter().copied());
            self.semantic_conversation = Some(SemanticConversationCheckpoint {
                protocol_version: protocol_version.to_string(),
                messages: messages.to_vec(),
            });
        } else {
            self.semantic_processed_queue_ids.clear();
            // A present-but-unsupported version is not a facade-only
            // snapshot. Clear stale provider history rather than replaying
            // a transcript with unknown semantics after restart.
            self.semantic_conversation = Some(SemanticConversationCheckpoint {
                protocol_version: protocol_version.to_string(),
                messages: Vec::new(),
            });
        }
        // Semantic snapshots are private checkpoint material, never an
        // ordinary received event or transcript/SSE payload.
        self.entries_since_checkpoint += 1;
        self.maybe_write_checkpoint(true)
    }

    fn maybe_write_checkpoint(&mut self, force: bool) -> std::io::Result<()> {
        if !force && self.entries_since_checkpoint < CHECKPOINT_INTERVAL {
            return Ok(());
        }

        // Publish the new replay state before writing the semantic-free JSONL
        // checkpoint. If this process dies after either write, the sidecar
        // still contains the newest semantic boundary and its tail offset
        // never skips it.
        self.write_replay_sidecar()?;
        let checkpoint = SessionEntry::Checkpoint {
            timestamp: current_timestamp(),
            state: Box::new(portable_redacted_checkpoint(&self.replay_state)),
            last_init: self.last_init.clone(),
            // Semantic history lives only in the atomically replaced sidecar;
            // embedding it in every append-only checkpoint is quadratic.
            semantic_conversation: None,
            last_workspace_capability_set: self.last_workspace_capability_set.clone().map(Box::new),
            last_process_budget: self.last_process_budget.clone(),
        };
        self.write_entry(&checkpoint)?;
        self.entries_since_checkpoint = 0;
        Ok(())
    }

    fn write_replay_sidecar(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        let mut semantic_processed_queue_ids = self
            .semantic_processed_queue_ids
            .iter()
            .copied()
            .collect::<Vec<_>>();
        semantic_processed_queue_ids.sort_unstable();
        let sidecar = ReplaySidecar {
            tail_offset: self.writer.get_ref().metadata()?.len(),
            state: portable_redacted_checkpoint(&self.replay_state),
            last_init: self.last_init.clone(),
            semantic_conversation: self.semantic_conversation.clone(),
            last_workspace_capability_set: self.last_workspace_capability_set.clone(),
            semantic_conversation_attempt: self.semantic_conversation_attempt,
            semantic_processed_queue_ids,
            last_process_budget: self.last_process_budget.clone(),
        };
        let json = serde_json::to_string(&sidecar)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        crate::fs_atomic::write_atomic(&self.replay_path, json)
    }

    /// Write an entry to the JSONL file
    fn write_entry(&mut self, entry: &SessionEntry) -> std::io::Result<()> {
        let json = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(self.writer, "{json}")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Flush and save metadata
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        // Make the JSONL log itself durable *before* persisting metadata
        // that describes it (message counts, usage totals, timestamps).
        // `BufWriter::flush` only transfers bytes to the OS; `save_metadata`
        // below now goes through `write_atomic`, which does fsync the
        // metadata file. Without this, a power loss right after `flush()`
        // returns could keep that durable metadata while losing the very
        // log entries it describes, leaving metadata durably ahead of the
        // actual session history on the next load.
        self.writer.get_ref().sync_all()?;
        self.save_metadata()?;
        Ok(())
    }

    /// Save metadata to file
    fn save_metadata(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.metadata)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::fs_atomic::write_atomic(&self.metadata_path, json)?;
        Ok(())
    }
}

fn portable_redacted_message(message: &FromAgentMessage) -> std::io::Result<FromAgentMessage> {
    let mut redacted = message.clone();
    let args = match &mut redacted {
        FromAgentMessage::ClientToolRequest { args, .. }
        | FromAgentMessage::ServerRequest {
            request_type: ServerRequestType::ClientTool,
            args,
            ..
        } => Some(args),
        _ => None,
    };
    if let Some(args) = args {
        *args = crate::agent::credential_store::redact_credentials_in_json(args);
    }
    if let FromAgentMessage::ConversationSnapshot { messages, .. } = &mut redacted {
        *messages =
            portable_redacted_semantic_conversation(&sanitize_semantic_conversation(messages))?;
    }
    Ok(redacted)
}

fn sanitize_semantic_conversation(messages: &[maestro_ai::Message]) -> Vec<maestro_ai::Message> {
    use maestro_ai::{ContentBlock, Message, MessageContent};

    messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::Text(_) => Some(message.clone()),
            MessageContent::Blocks(blocks) => {
                let blocks: Vec<ContentBlock> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { .. } | ContentBlock::ToolUse { .. } => {
                            Some(block.clone())
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            ..
                        } => Some(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: "[tool result omitted from checkpoint]".to_owned(),
                            is_error: *is_error,
                        }),
                        ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => None,
                    })
                    .collect();
                (!blocks.is_empty()).then_some(Message {
                    role: message.role,
                    content: MessageContent::Blocks(blocks),
                })
            }
        })
        .collect()
}

fn portable_redacted_semantic_conversation(
    messages: &[maestro_ai::Message],
) -> std::io::Result<Vec<maestro_ai::Message>> {
    let serialized = serde_json::to_value(messages)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    serde_json::from_value(redact_semantic_checkpoint_value(
        crate::agent::credential_store::redact_credentials_in_json(&serialized),
    ))
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn redact_semantic_checkpoint_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(redact_semantic_checkpoint_value)
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    let sensitive = matches!(
                        key.to_ascii_lowercase().as_str(),
                        "api_key" | "apikey" | "authorization" | "password" | "secret" | "token"
                    );
                    (
                        key,
                        if sensitive {
                            serde_json::Value::String("[REDACTED]".to_string())
                        } else {
                            redact_semantic_checkpoint_value(value)
                        },
                    )
                })
                .collect(),
        ),
        value => value,
    }
}

fn portable_redacted_checkpoint(state: &AgentState) -> AgentStateCheckpoint {
    let mut checkpoint = AgentStateCheckpoint::from_state(state);
    for pending in checkpoint
        .pending_client_tools
        .iter_mut()
        .chain(checkpoint.tracked_tools.iter_mut())
    {
        pending.args = crate::agent::credential_store::redact_credentials_in_json(&pending.args);
    }
    checkpoint
}

impl Drop for SessionRecorder {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Session reader - loads entries from a JSONL file.
///
/// Loads a complete session from disk, including all messages and metadata.
/// Provides convenient methods for filtering and analyzing session history.
///
/// # Memory Considerations
///
/// `SessionReader` loads the entire session into memory. For very long sessions
/// (thousands of messages), this may consume significant memory. Consider
/// implementing streaming/pagination for production use with large sessions.
///
/// # Error Handling
///
/// Parse errors for individual entries are logged to stderr but don't prevent
/// loading the rest of the session. This provides resilience against corrupted
/// or incompatible entries in old session files.
///
/// # Examples
///
/// ```rust,ignore
/// use maestro_local_host::headless::session::SessionReader;
///
/// let reader = SessionReader::load("/tmp/sessions", "session-id")?;
///
/// println!("Session: {}", reader.metadata().title.as_deref().unwrap_or("Untitled"));
/// println!("Total messages: {}", reader.entries().len());
/// println!("User prompts: {}", reader.prompts().len());
///
/// // Analyze conversation
/// for (i, prompt) in reader.prompts().iter().enumerate() {
///     println!("{}. {}", i + 1, prompt);
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct SessionReader {
    /// Session ID
    id: String,
    /// Loaded entries
    entries: Vec<SessionEntry>,
    /// Session metadata
    metadata: SessionMetadata,
}

/// Reconstructed headless session state derived from the recorded JSONL log.
#[derive(Debug, Clone)]
pub struct SessionReplay {
    /// Current reconstructed agent state after replaying recorded events.
    pub state: AgentState,
    /// Most recent init configuration sent to the headless agent, if any.
    pub last_init: Option<InitConfig>,
    /// Latest private provider conversation, if the session wrote a semantic
    /// checkpoint using the supported snapshot protocol.
    pub semantic_conversation: Option<Vec<maestro_ai::Message>>,
    /// Last workspace capability set proven accepted by a matching receipt.
    pub last_workspace_capability_set: Option<ApplyWorkspaceCapabilitySet>,
    /// Latest native process budget observation, retained as evidence only.
    /// Replay never installs it into the active agent or grants authority.
    pub last_process_budget: Option<ProcessBudgetState>,
}

fn persist_rebuilt_metadata(path: &Path, metadata: &SessionMetadata) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(metadata)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    crate::fs_atomic::write_atomic(path, json)
}

fn load_replay_sidecar(path: &Path) -> Option<(ReplaySidecar, bool)> {
    let json = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let has_process_budget_field = value.get("last_process_budget").is_some();
    let sidecar = serde_json::from_value(value).ok()?;
    Some((sidecar, has_process_budget_field))
}

fn replay_from_sidecar_tail(
    path: &Path,
    sidecar: ReplaySidecar,
    has_process_budget_field: bool,
) -> std::io::Result<SessionReplay> {
    let mut state = sidecar.state.into_state();
    let mut last_init = sidecar.last_init;
    let mut last_workspace_capability_set = sidecar.last_workspace_capability_set;
    let mut last_process_budget = sidecar.last_process_budget;
    let mut pending_workspace_capability_sets = HashMap::new();
    let mut semantic_conversation = sidecar.semantic_conversation.and_then(|checkpoint| {
        (checkpoint.protocol_version == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL)
            .then_some(checkpoint.messages)
    });
    if !path.exists() {
        return Ok(SessionReplay {
            state,
            last_init,
            semantic_conversation,
            last_workspace_capability_set,
            last_process_budget,
        });
    }
    let mut file = File::open(path)?;
    let journal_len = file.metadata()?.len();
    if sidecar.tail_offset > journal_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replay sidecar offset exceeds session journal",
        ));
    }
    if !has_process_budget_field {
        // Legacy sidecars may have advanced their tail beyond a budget
        // receipt. Recover only the omitted evidence from the prefix so the
        // sidecar's semantic conversation and other restore anchors remain
        // intact.
        last_process_budget = recover_process_budget_prefix(path, sidecar.tail_offset)?;
    }
    file.seek(SeekFrom::Start(sidecar.tail_offset))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(&line) {
            apply_replay_entry(
                &entry,
                &mut state,
                &mut last_init,
                &mut semantic_conversation,
                &mut last_workspace_capability_set,
                &mut last_process_budget,
                &mut pending_workspace_capability_sets,
            );
        }
    }
    Ok(SessionReplay {
        state,
        last_init,
        semantic_conversation,
        last_workspace_capability_set,
        last_process_budget,
    })
}

fn recover_process_budget_prefix(
    path: &Path,
    end_offset: u64,
) -> std::io::Result<Option<ProcessBudgetState>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut offset = 0_u64;
    let mut last_process_budget = None;
    while offset < end_offset {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        let next_offset = offset.saturating_add(bytes_read as u64);
        if next_offset > end_offset {
            break;
        }
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(&line) {
            apply_process_budget_evidence(&entry, &mut last_process_budget);
        }
        offset = next_offset;
    }
    Ok(last_process_budget)
}

fn apply_process_budget_evidence(
    entry: &SessionEntry,
    last_process_budget: &mut Option<ProcessBudgetState>,
) {
    match entry {
        SessionEntry::Received {
            message: FromAgentMessage::ProcessBudgetCheckpoint { budget },
            ..
        } => *last_process_budget = Some(budget.clone()),
        SessionEntry::Checkpoint {
            last_process_budget: Some(budget),
            ..
        } => *last_process_budget = Some(budget.clone()),
        _ => {}
    }
}

fn apply_replay_entry(
    entry: &SessionEntry,
    state: &mut AgentState,
    last_init: &mut Option<InitConfig>,
    semantic_conversation: &mut Option<Vec<maestro_ai::Message>>,
    last_workspace_capability_set: &mut Option<ApplyWorkspaceCapabilitySet>,
    last_process_budget: &mut Option<ProcessBudgetState>,
    pending_workspace_capability_sets: &mut HashMap<String, ApplyWorkspaceCapabilitySet>,
) {
    apply_process_budget_evidence(entry, last_process_budget);
    match entry {
        SessionEntry::Sent { message, .. } => {
            state.handle_sent_message(message);
            record_sent_workspace_capability(pending_workspace_capability_sets, message);
            if let Some(config) = init_config_from_message(message) {
                *last_init = Some(config);
            }
        }
        SessionEntry::Received { message, .. } => {
            let accepted_capability_receipt =
                if let FromAgentMessage::WorkspaceCapabilitySetApplied { receipt } = message {
                    accept_workspace_capability_receipt(
                        pending_workspace_capability_sets,
                        last_workspace_capability_set,
                        receipt,
                    )
                } else {
                    true
                };
            if let FromAgentMessage::ConversationSnapshot {
                protocol_version,
                messages,
                ..
            } = message
            {
                *semantic_conversation = (protocol_version
                    == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL)
                    .then_some(messages.clone());
            }
            if accepted_capability_receipt {
                let _ = state.handle_message(message.clone());
            }
        }
        SessionEntry::Checkpoint {
            state: checkpoint,
            last_init: checkpoint_init,
            semantic_conversation: checkpoint_conversation,
            last_workspace_capability_set: checkpoint_capability_set,
            ..
        } => {
            *state = checkpoint.as_ref().clone().into_state();
            *last_init = checkpoint_init.clone();
            *last_workspace_capability_set = checkpoint_capability_set.as_deref().cloned();
            pending_workspace_capability_sets.clear();
            if let Some(checkpoint) = checkpoint_conversation {
                *semantic_conversation = (checkpoint.protocol_version
                    == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL)
                    .then_some(checkpoint.messages.clone());
            }
        }
    }
}

impl SessionReader {
    /// Load a session from disk
    pub fn load(sessions_dir: impl AsRef<Path>, id: &str) -> std::io::Result<Self> {
        let sessions_dir = sessions_dir.as_ref();
        let path = sessions_dir.join(format!("{id}.jsonl"));
        let metadata_path = sessions_dir.join(format!("{id}.meta.json"));

        // Load metadata
        let (metadata, reconstructed) = load_metadata_tolerant(&metadata_path, id)?;

        // Load entries
        let mut entries = Vec::new();
        if path.exists() {
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<SessionEntry>(&line) {
                    Ok(entry) => entries.push(entry),
                    Err(e) => {
                        eprintln!("Warning: Failed to parse session entry: {e}");
                    }
                }
            }
        }

        // `load_metadata_tolerant` already rotated the corrupt file aside
        // above but, unlike `SessionRecorder::resume`, this is a read-only
        // path with no later `flush()` to persist a replacement -- so
        // without rebuilding and writing one back here, calling this
        // function directly (rather than through `resume`) would both
        // discard the title/usage/counts recoverable from these
        // just-loaded entries (falling back to bare defaults) and leave
        // the session with no `.meta.json` at all, making it invisible to
        // a later `list_sessions` scan.
        let metadata = if reconstructed {
            let rebuilt = rebuild_metadata(id, &entries);
            persist_rebuilt_metadata(&metadata_path, &rebuilt)?;
            rebuilt
        } else {
            metadata
        };

        Ok(Self {
            id: id.to_string(),
            entries,
            metadata,
        })
    }

    /// Get the session ID
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the session metadata
    #[must_use]
    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    /// Get all entries
    #[must_use]
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Get only sent messages
    #[must_use]
    pub fn sent_messages(&self) -> Vec<&ToAgentMessage> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Sent { message, .. } => Some(message),
                _ => None,
            })
            .collect()
    }

    /// Get only received messages
    #[must_use]
    pub fn received_messages(&self) -> Vec<&FromAgentMessage> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Received { message, .. } => Some(message),
                _ => None,
            })
            .collect()
    }

    /// Get user prompts only
    #[must_use]
    pub fn prompts(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Sent {
                    message: ToAgentMessage::Prompt { content, .. },
                    ..
                } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Return the last init configuration sent to the headless agent, if any.
    #[must_use]
    pub fn last_init(&self) -> Option<InitConfig> {
        self.replay().last_init
    }

    /// Replay the recorded agent messages into an `AgentState`.
    #[must_use]
    pub fn replay_state(&self) -> AgentState {
        self.replay().state
    }

    /// Build a resumable snapshot from the recorded session log.
    #[must_use]
    pub fn replay(&self) -> SessionReplay {
        let mut state = AgentState::default();
        let mut last_init = None;
        let mut semantic_conversation = None;
        let mut last_workspace_capability_set = None;
        // Budget receipts are evidence independent of the state checkpoint.
        // Scan the complete journal so a legacy checkpoint that omits the
        // optional field cannot hide an earlier receipt behind start_index.
        let mut last_process_budget = None;
        for entry in &self.entries {
            apply_process_budget_evidence(entry, &mut last_process_budget);
        }
        let mut pending_workspace_capability_sets = HashMap::new();
        let mut start_index = 0;

        for (index, entry) in self.entries.iter().enumerate() {
            if let SessionEntry::Checkpoint {
                state: checkpoint,
                last_init: checkpoint_init,
                semantic_conversation: checkpoint_conversation,
                last_workspace_capability_set: checkpoint_capability_set,
                ..
            } = entry
            {
                state = checkpoint.as_ref().clone().into_state();
                last_init = checkpoint_init.clone();
                last_workspace_capability_set = checkpoint_capability_set.as_deref().cloned();
                pending_workspace_capability_sets.clear();
                match checkpoint_conversation {
                    Some(checkpoint)
                        if checkpoint.protocol_version
                            == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL =>
                    {
                        semantic_conversation = Some(checkpoint.messages.clone());
                    }
                    Some(_) => semantic_conversation = None,
                    // A facade-only checkpoint does not supersede a durable
                    // semantic checkpoint recorded earlier in the same log.
                    None => {}
                }
                start_index = index + 1;
            }
        }

        for entry in &self.entries[start_index..] {
            match entry {
                SessionEntry::Sent { message, .. } => {
                    state.handle_sent_message(message);
                    record_sent_workspace_capability(
                        &mut pending_workspace_capability_sets,
                        message,
                    );
                    if let Some(config) = init_config_from_message(message) {
                        last_init = Some(config);
                    }
                }
                SessionEntry::Received { message, .. } => {
                    let accepted_capability_receipt =
                        if let FromAgentMessage::WorkspaceCapabilitySetApplied { receipt } = message
                        {
                            accept_workspace_capability_receipt(
                                &mut pending_workspace_capability_sets,
                                &mut last_workspace_capability_set,
                                receipt,
                            )
                        } else {
                            true
                        };
                    if let FromAgentMessage::ConversationSnapshot {
                        protocol_version,
                        messages,
                        ..
                    } = message
                    {
                        if protocol_version
                            == crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                        {
                            semantic_conversation = Some(messages.clone());
                        } else {
                            semantic_conversation = None;
                        }
                    }
                    if accepted_capability_receipt {
                        let _ = state.handle_message(message.clone());
                    }
                }
                SessionEntry::Checkpoint { .. } => {}
            }
        }
        SessionReplay {
            state,
            last_init,
            semantic_conversation,
            last_workspace_capability_set,
            last_process_budget,
        }
    }
}

/// List available sessions in a directory
pub fn list_sessions(sessions_dir: impl AsRef<Path>) -> std::io::Result<Vec<SessionMetadata>> {
    let sessions_dir = sessions_dir.as_ref();
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    // Collect entries up front: a corrupt metadata file is rebuilt and
    // re-persisted below, and a live directory iterator could observe the
    // newly written file and list the session twice.
    let entries = fs::read_dir(sessions_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    for entry in entries {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".meta.json"))
        {
            // Read bytes, not `read_to_string`: a `.meta.json` torn
            // mid-write by a crash can be torn in the middle of a
            // multi-byte UTF-8 character just as easily as in the middle
            // of a JSON token, and `read_to_string` failing on that would
            // otherwise skip this whole `if let` block (and, per the
            // comment below, silently drop the session from the listing)
            // without ever reaching the same rotate-and-rebuild recovery a
            // JSON-parse failure already gets.
            if let Ok(content) = fs::read(&path) {
                match serde_json::from_slice::<SessionMetadata>(&content) {
                    Ok(meta) => sessions.push(meta),
                    Err(err) => {
                        // Previously this silently dropped the session from
                        // the listing, making a crash-torn metadata file
                        // look identical to "session never existed". Rotate
                        // the corrupt file aside, rebuild the metadata from
                        // the still-intact JSONL log, and persist the
                        // reconstruction so the session stays discoverable
                        // in subsequent listings.
                        let id = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .and_then(|n| n.strip_suffix(".meta.json"))
                            .unwrap_or("unknown");
                        eprintln!(
                            "Corrupt session metadata at {}: {err}. Rotating aside and reconstructing from the JSONL log.",
                            path.display()
                        );
                        crate::fs_atomic::rotate_corrupt_aside(&path);
                        let meta = match SessionReader::load(sessions_dir, id) {
                            Ok(reader) => rebuild_metadata(id, reader.entries()),
                            Err(_) => SessionMetadata::new(id),
                        };
                        persist_rebuilt_metadata(&path, &meta)?;
                        sessions.push(meta);
                    }
                }
            }
        }
    }

    // Sort by updated_at descending (most recent first)
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));

    Ok(sessions)
}

/// Delete a session
pub fn delete_session(sessions_dir: impl AsRef<Path>, id: &str) -> std::io::Result<()> {
    let sessions_dir = sessions_dir.as_ref();
    let jsonl_path = sessions_dir.join(format!("{id}.jsonl"));
    let meta_path = sessions_dir.join(format!("{id}.meta.json"));

    if jsonl_path.exists() {
        fs::remove_file(&jsonl_path)?;
    }
    if meta_path.exists() {
        fs::remove_file(&meta_path)?;
    }
    let _ = crate::tool_output::remove_model_tool_spill_dir(sessions_dir, id);

    Ok(())
}

/// Get current timestamp as unix millis
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{ContentBlock, Message, MessageContent, Role};
    use crate::headless::{CodeMode, GovernedToolGrant};
    use tempfile::TempDir;

    fn governed_grant_fixture() -> GovernedToolGrant {
        serde_json::from_value(serde_json::json!({
            "envelope_version": 2,
            "grant_id": "grant-reconnect",
            "grant_version": 8,
            "issuer": "evalops.platform",
            "audience": "evalops.maestro",
            "organization_id": "org-1",
            "workspace_id": "workspace-1",
            "thread_id": "thread-1",
            "turn_id": "turn-1",
            "run_id": "run-1",
            "runtime_generation": 5,
            "grant_epoch": 3,
            "issued_at_ms": 100,
            "not_before_ms": 100,
            "expires_at_ms": 10_000,
            "grant_hash": "sha256:immutable",
            "signing_key_id": "key-1",
            "grant_signature": "hmac-sha256:signed",
            "identity_authorization": {
                "schemaVersion": "identity.tool_authorization.v1",
                "organizationId": "org-1",
                "workspaceId": "workspace-1",
                "applicationId": "deixic",
                "subjectId": "user-1",
                "actorChainDigest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "decisionId": "decision-1",
                "authorizationLineageId": "lineage-1",
                "policyId": "policy-1",
                "policyVersion": "v1",
                "policyDigest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "authorizationFingerprint": "authz_fingerprint_v1_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "capabilityDigest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "actionDigest": "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                "audience": "evalops.maestro",
                "issuedAtMs": 100,
                "expiresAtMs": 10000,
                "revocationEpoch": 3
            },
            "native_tool_ids": ["read"],
            "external_tools": []
        }))
        .unwrap()
    }

    fn workspace_capability_request(generation: u64) -> ApplyWorkspaceCapabilitySet {
        ApplyWorkspaceCapabilitySet {
            organization_id: "org-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            runner_session_id: "runner-1".to_string(),
            runtime_generation: 7,
            activation_generation: generation,
            workspace_snapshot_digest: format!("sha256:snapshot-{generation}"),
            workspace_skill_set_digest: format!("sha256:skills-{generation}"),
            capability_set_digest: format!("sha256:set-{generation}"),
            workspace_instructions: vec![format!("generation {generation}")],
            admitted_catalog: Vec::new(),
            admission_receipt_id: format!("admission-{generation}"),
        }
    }

    fn workspace_capability_receipt(
        request: &ApplyWorkspaceCapabilitySet,
    ) -> WorkspaceCapabilitySetApplied {
        WorkspaceCapabilitySetApplied {
            schema_version: "evalops.maestro.workspace-prompt-capability-set.v1".to_string(),
            organization_id: request.organization_id.clone(),
            workspace_id: request.workspace_id.clone(),
            runner_session_id: request.runner_session_id.clone(),
            runtime_generation: request.runtime_generation,
            activation_generation: request.activation_generation,
            effective_catalog_digest: request.capability_set_digest.clone(),
            accepted_entry_digests: request
                .admitted_catalog
                .iter()
                .map(|entry| entry.entry_digest.clone())
                .collect(),
            rejected_entries: Vec::new(),
            replay_cursor: workspace_capability_replay_cursor(request),
            applied_at: 123,
            controller_binding_sha256: "sha256:binding".to_string(),
            provider_prompt_sha256: "sha256:provider-prompt".to_string(),
            staged_for_next_turn: false,
            idempotent: false,
        }
    }

    fn process_budget_fixture() -> ProcessBudgetState {
        let mut budget =
            ProcessBudgetState::new(crate::agent::process_budget::ProcessBudgetLimits {
                event_id: "event-session-replay".to_owned(),
                max_requests: 4,
                max_total_tokens: 100,
                max_cost_micros: 1_000,
                cost_micros_per_token: 2,
            })
            .expect("valid process budget limits");
        budget.admit_request().expect("first request admission");
        budget
            .observe_usage(3, 4, Some(41))
            .expect("provider usage");
        budget.admit_tools(1).expect("tool admission");
        budget
            .charge_tool("execution-session-1", 13)
            .expect("acknowledged tool cost");
        budget.admit_request().expect("pending second request");
        budget
    }

    fn legacy_sidecar_without_process_budget(path: &std::path::Path) {
        let json = fs::read_to_string(path).expect("read replay sidecar");
        let mut value: serde_json::Value = serde_json::from_str(&json).expect("parse sidecar");
        value
            .as_object_mut()
            .expect("sidecar object")
            .remove("last_process_budget");
        fs::write(
            path,
            serde_json::to_string(&value).expect("serialize legacy sidecar"),
        )
        .expect("write legacy sidecar");
    }

    #[test]
    fn process_budget_evidence_survives_journal_sidecar_periodic_and_resume_paths() {
        let temp = TempDir::new().expect("session root");
        let mut recorder =
            SessionRecorder::with_id(temp.path(), "budget-evidence").expect("session recorder");
        let expected = process_budget_fixture();
        recorder
            .record_received(&FromAgentMessage::ProcessBudgetCheckpoint {
                budget: expected.clone(),
            })
            .expect("record budget checkpoint");

        // Budget observations are evidence only. The active state snapshot is
        // unchanged, so replay cannot install a grant or reset accounting.
        assert_eq!(
            recorder.replay().last_process_budget,
            Some(expected.clone())
        );
        assert_eq!(
            AgentStateCheckpoint::from_state(recorder.replay_state()),
            AgentStateCheckpoint::from_state(&AgentState::default())
        );

        // Cross the ordinary checkpoint interval to exercise the periodic
        // checkpoint path after the budget receipt's forced sync checkpoint.
        for index in 0..CHECKPOINT_INTERVAL {
            recorder
                .record_received(&FromAgentMessage::Status {
                    message: format!("tail status {index}"),
                })
                .expect("record tail status");
        }
        recorder.flush().expect("flush budget journal");

        let journal = fs::read_to_string(recorder.path()).expect("read budget journal");
        assert!(journal.contains("process_budget_checkpoint"));
        assert!(journal.contains("\"last_process_budget\""));
        let sidecar_path = temp.path().join("budget-evidence.replay.json");
        let sidecar = fs::read_to_string(&sidecar_path).expect("read budget sidecar");
        assert!(sidecar.contains("\"last_process_budget\""));

        // Full journal replay and sidecar-plus-tail resume must retain exact
        // pending, spent, and acknowledged cost evidence.
        let full_replay = SessionReader::load(temp.path(), "budget-evidence")
            .expect("load full journal")
            .replay();
        assert_eq!(full_replay.last_process_budget, Some(expected.clone()));
        drop(recorder);
        let resumed = SessionRecorder::resume(temp.path(), "budget-evidence")
            .expect("resume recorder")
            .replay();
        assert_eq!(resumed.last_process_budget, Some(expected));
    }

    #[test]
    fn legacy_sidecar_recovers_budget_prefix_without_dropping_semantic_history() {
        let temp = TempDir::new().expect("session root");
        let session_id = "legacy-budget-sidecar";
        let prefix_budget = process_budget_fixture();
        let mut tail_budget = prefix_budget.clone();
        tail_budget
            .observe_usage(5, 6, Some(23))
            .expect("resolve pending usage");
        tail_budget.admit_tools(1).expect("tail tool admission");
        tail_budget
            .charge_tool("execution-session-2", 17)
            .expect("second acknowledged tool cost");
        let semantic_messages = semantic_tool_pair_fixture();

        let mut recorder =
            SessionRecorder::with_id(temp.path(), session_id).expect("session recorder");
        recorder
            .record_semantic_conversation(semantic_messages.clone())
            .expect("record semantic history");
        recorder
            .flush_checkpoint()
            .expect("persist semantic history");
        recorder
            .record_received(&FromAgentMessage::ProcessBudgetCheckpoint {
                budget: prefix_budget.clone(),
            })
            .expect("record prefix budget");
        recorder
            .record_received(&FromAgentMessage::Status {
                message: "tail before crash".to_owned(),
            })
            .expect("record tail status");
        recorder.flush().expect("flush legacy sidecar fixture");
        let sidecar_path = temp.path().join(format!("{session_id}.replay.json"));
        let journal_path = temp.path().join(format!("{session_id}.jsonl"));
        drop(recorder);

        // Model a pre-budget sidecar whose tail already skips the prefix
        // receipt. Append a later receipt after that anchor so recovery must
        // combine journal-prefix evidence with sidecar-tail replay.
        legacy_sidecar_without_process_budget(&sidecar_path);
        let (legacy_sidecar, has_process_budget_field) =
            load_replay_sidecar(&sidecar_path).expect("load legacy sidecar");
        let prefix_replay =
            replay_from_sidecar_tail(&journal_path, legacy_sidecar, has_process_budget_field)
                .expect("replay legacy sidecar prefix");
        assert!(!has_process_budget_field);
        assert_eq!(prefix_replay.last_process_budget, Some(prefix_budget));
        assert_eq!(
            serde_json::to_value(&prefix_replay.semantic_conversation).expect("replayed messages"),
            serde_json::to_value(Some(sanitize_semantic_conversation(&semantic_messages)))
                .expect("expected messages")
        );
        let tail_entry = SessionEntry::received(FromAgentMessage::ProcessBudgetCheckpoint {
            budget: tail_budget.clone(),
        });
        let mut journal = OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open journal tail");
        writeln!(
            journal,
            "{}",
            serde_json::to_string(&tail_entry).expect("serialize tail budget")
        )
        .expect("append tail budget");
        journal.sync_all().expect("sync journal tail");

        let replay = SessionRecorder::resume(temp.path(), session_id)
            .expect("resume legacy sidecar")
            .replay();
        assert_eq!(replay.last_process_budget, Some(tail_budget));
        assert_eq!(
            serde_json::to_value(&replay.semantic_conversation).expect("replayed messages"),
            serde_json::to_value(Some(sanitize_semantic_conversation(&semantic_messages)))
                .expect("expected messages")
        );
    }

    #[test]
    fn legacy_checkpoint_without_budget_does_not_erase_prior_evidence() {
        let temp = TempDir::new().expect("session root");
        let session_id = "legacy-budget-checkpoint";
        let expected = process_budget_fixture();
        let received = SessionEntry::received(FromAgentMessage::ProcessBudgetCheckpoint {
            budget: expected.clone(),
        });
        let checkpoint = SessionEntry::Checkpoint {
            timestamp: 2,
            state: Box::new(AgentStateCheckpoint::from_state(&AgentState::default())),
            last_init: None,
            semantic_conversation: None,
            last_workspace_capability_set: None,
            last_process_budget: None,
        };
        let mut legacy_checkpoint = serde_json::to_value(checkpoint).expect("serialize checkpoint");
        legacy_checkpoint
            .as_object_mut()
            .expect("checkpoint object")
            .remove("last_process_budget");
        let journal = format!(
            "{}\n{}\n",
            serde_json::to_string(&received).expect("serialize budget"),
            serde_json::to_string(&legacy_checkpoint).expect("serialize legacy checkpoint")
        );
        fs::write(temp.path().join(format!("{session_id}.jsonl")), journal)
            .expect("write legacy journal");

        let replay = SessionReader::load(temp.path(), session_id)
            .expect("load legacy journal")
            .replay();
        assert_eq!(replay.last_process_budget, Some(expected));
        assert_eq!(
            AgentStateCheckpoint::from_state(&replay.state),
            AgentStateCheckpoint::from_state(&AgentState::default())
        );
    }

    #[test]
    fn legacy_sidecar_without_budget_does_not_fabricate_a_zero_checkpoint() {
        let temp = TempDir::new().expect("session root");
        let session_id = "legacy-empty-budget-sidecar";
        let mut recorder =
            SessionRecorder::with_id(temp.path(), session_id).expect("session recorder");
        recorder
            .flush_checkpoint()
            .expect("persist empty checkpoint");
        let sidecar_path = temp.path().join(format!("{session_id}.replay.json"));
        drop(recorder);
        legacy_sidecar_without_process_budget(&sidecar_path);

        let replay = SessionRecorder::resume(temp.path(), session_id)
            .expect("resume legacy sidecar")
            .replay();
        assert!(replay.last_process_budget.is_none());
    }

    #[test]
    fn session_restart_replays_only_the_last_matching_accepted_capability_set() {
        let temp = TempDir::new().expect("session root");
        let mut recorder = SessionRecorder::new(temp.path()).expect("session recorder");
        let session_id = recorder.id().to_string();
        let accepted = workspace_capability_request(1);
        recorder
            .record_sent(&ToAgentMessage::ApplyWorkspaceCapabilitySet {
                request: accepted.clone(),
            })
            .expect("record accepted candidate");

        let mut mismatched = workspace_capability_receipt(&accepted);
        mismatched.organization_id = "org-wrong".to_string();
        recorder
            .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: mismatched,
            })
            .expect("record mismatched receipt");
        recorder
            .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: workspace_capability_receipt(&accepted),
            })
            .expect("record matching receipt");

        recorder
            .record_sent(&ToAgentMessage::ApplyWorkspaceCapabilitySet {
                request: workspace_capability_request(2),
            })
            .expect("record unacknowledged successor");
        recorder.flush().expect("flush journal");
        drop(recorder);

        let replay = SessionRecorder::resume(temp.path(), &session_id)
            .expect("resume recorder")
            .replay();
        assert_eq!(replay.last_workspace_capability_set, Some(accepted));
    }

    #[test]
    fn session_restart_ignores_partial_and_late_older_capability_receipts() {
        let temp = TempDir::new().expect("session root");
        let mut recorder = SessionRecorder::new(temp.path()).expect("session recorder");
        let session_id = recorder.id().to_string();
        let first = workspace_capability_request(1);
        let second = workspace_capability_request(2);
        for request in [&first, &second] {
            recorder
                .record_sent(&ToAgentMessage::ApplyWorkspaceCapabilitySet {
                    request: request.clone(),
                })
                .expect("record candidate");
        }
        let mut partial = workspace_capability_receipt(&second);
        partial.rejected_entries.push("skill.review".to_string());
        recorder
            .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied { receipt: partial })
            .expect("record partial receipt");
        assert!(recorder.replay().last_workspace_capability_set.is_none());

        recorder
            .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: workspace_capability_receipt(&second),
            })
            .expect("record accepted receipt");
        recorder
            .record_received(&FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: workspace_capability_receipt(&first),
            })
            .expect("record late older receipt");
        recorder.flush().expect("flush journal");
        drop(recorder);

        let replay = SessionRecorder::resume(temp.path(), &session_id)
            .expect("resume recorder")
            .replay();
        assert_eq!(replay.last_workspace_capability_set, Some(second));
    }

    #[test]
    fn governed_init_grant_identity_survives_session_replay() {
        let temp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(temp.path()).unwrap();
        let session_id = recorder.id().to_string();
        let grant = governed_grant_fixture();
        recorder
            .record_sent(&ToAgentMessage::GovernedInit {
                system_prompt: None,
                append_system_prompt: None,
                thinking_level: None,
                approval_mode: None,
                history: None,
                code_mode: CodeMode::GovernedCode,
                tool_grant: grant.clone(),
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let replay = SessionReader::load(temp.path(), &session_id)
            .unwrap()
            .replay();
        let restored = replay
            .last_init
            .and_then(|init| init.tool_grant)
            .expect("governed grant restored");
        assert_eq!(restored.identity(), grant.identity());
        assert_eq!(restored, grant);
    }

    mod managed_gateway_renewal;

    fn semantic_tool_pair_fixture() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("inspect the workspace".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "tool-call-1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({ "command": "pwd" }),
                    gemini_context: None,
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-call-1".to_string(),
                    content: "/workspace".to_string(),
                    is_error: None,
                }]),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("The workspace is ready.".to_string()),
            },
        ]
    }

    fn credential_tool_fixture() -> Vec<Message> {
        vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "tool-call-1".to_string(),
                name: "http".to_string(),
                input: serde_json::json!({ "api_key": "sk-secret-value" }),
                gemini_context: None,
            }]),
        }]
    }

    #[test]
    fn test_session_replay_restores_semantic_provider_conversation_with_tool_pairs() {
        let tmp = TempDir::new().unwrap();
        let messages = semantic_tool_pair_fixture();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_semantic_conversation(messages.clone())
            .unwrap();
        recorder.flush_checkpoint().unwrap();

        let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
        assert_eq!(
            serde_json::to_value(replay.semantic_conversation).unwrap(),
            serde_json::to_value(Some(sanitize_semantic_conversation(&messages))).unwrap()
        );
    }

    #[test]
    fn test_semantic_snapshot_checkpoint_redacts_credentials() {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        recorder
            .record_semantic_conversation(credential_tool_fixture())
            .unwrap();
        recorder.flush_checkpoint().unwrap();

        let jsonl = std::fs::read_to_string(recorder.path()).unwrap();
        let sidecar =
            std::fs::read_to_string(tmp.path().join(format!("{}.replay.json", recorder.id())))
                .unwrap();
        assert!(!jsonl.contains("sk-secret-value"));
        assert!(!jsonl.contains("tool-call-1"));
        assert!(!sidecar.contains("sk-secret-value"));
        assert!(sidecar.contains("[REDACTED]"));
        assert!(sidecar.contains("tool-call-1"));
    }

    #[test]
    fn direct_semantic_snapshot_preserves_processed_queue_ids_across_resume() {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        for ids in [vec![7, 9], vec![9, 7, 11]] {
            let message: FromAgentMessage = serde_json::from_value(serde_json::json!({
                "type": "conversation_snapshot",
                "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
                "messages": semantic_tool_pair_fixture(),
                "processed_queue_ids": ids,
            }))
            .unwrap();
            recorder.record_received(&message).unwrap();
        }
        recorder.flush().unwrap();
        drop(recorder);

        let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
        assert_eq!(
            resumed.semantic_processed_queue_ids(),
            &HashSet::from([7, 9, 11])
        );
        assert!(resumed.replay().semantic_conversation.is_some());
        assert!(
            SessionReader::load(tmp.path(), &id)
                .unwrap()
                .received_messages()
                .is_empty()
        );
        let sidecar: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join(format!("{id}.replay.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            sidecar["semantic_processed_queue_ids"],
            serde_json::json!([7, 9, 11])
        );
    }

    #[test]
    fn missing_or_corrupt_semantic_sidecar_does_not_invent_processed_queue_ids() {
        for corrupt in [false, true] {
            let tmp = TempDir::new().unwrap();
            let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
            let id = recorder.id().to_owned();
            recorder
                .record_received(&FromAgentMessage::Status {
                    message: "journal survives".to_owned(),
                })
                .unwrap();
            let snapshot: FromAgentMessage = serde_json::from_value(serde_json::json!({
                "type": "conversation_snapshot",
                "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
                "messages": semantic_tool_pair_fixture(),
                "processed_queue_ids": [7, 9],
            }))
            .unwrap();
            recorder.record_received(&snapshot).unwrap();
            recorder.flush().unwrap();
            drop(recorder);
            let sidecar = tmp.path().join(format!("{id}.replay.json"));
            if corrupt {
                fs::write(&sidecar, "invalid json").unwrap();
            } else {
                fs::remove_file(&sidecar).unwrap();
            }
            let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
            // New journals deliberately omit private snapshots; this fallback
            // cannot recover their provider history or processed-id metadata.
            assert!(resumed.semantic_processed_queue_ids().is_empty());
            assert!(resumed.replay().semantic_conversation.is_none());
            assert_eq!(
                SessionReader::load(tmp.path(), &id)
                    .unwrap()
                    .received_messages()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn legacy_semantic_snapshot_without_queue_ids_remains_readable() {
        let snapshot: FromAgentMessage = serde_json::from_value(serde_json::json!({
            "type": "conversation_snapshot",
            "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
            "messages": semantic_tool_pair_fixture(),
        }))
        .unwrap();
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        recorder.record_received(&snapshot).unwrap();
        recorder.flush().unwrap();
        drop(recorder);
        let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
        assert!(resumed.semantic_processed_queue_ids().is_empty());
        assert!(resumed.replay().semantic_conversation.is_some());
    }

    #[test]
    fn explicit_semantic_snapshot_metadata_preserves_caller_queue_ids() {
        let snapshot: FromAgentMessage = serde_json::from_value(serde_json::json!({
            "type": "conversation_snapshot",
            "protocol_version": crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL,
            "messages": semantic_tool_pair_fixture(),
            "processed_queue_ids": [7, 9],
        }))
        .unwrap();
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        recorder
            .record_received_preserving_credential_references_with_snapshot_metadata(
                &snapshot,
                Some(3),
                &[21, 21, 23],
            )
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);
        let resumed = SessionRecorder::resume(tmp.path(), &id).unwrap();
        assert_eq!(
            resumed.semantic_processed_queue_ids(),
            &HashSet::from([21, 23])
        );
        assert_eq!(resumed.semantic_conversation_attempt, Some(3));
    }

    #[test]
    fn received_semantic_snapshot_is_checkpoint_only_and_strips_private_content() {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        recorder
            .record_received(&FromAgentMessage::ConversationSnapshot {
                processed_queue_ids: vec![7, 9],
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_owned(),
                messages: vec![
                    Message {
                        role: Role::Assistant,
                        content: MessageContent::Blocks(vec![
                            ContentBlock::Thinking {
                                thinking: "secret reasoning".to_owned(),
                                signature: None,
                            },
                            ContentBlock::ToolUse {
                                id: "call-private".to_owned(),
                                name: "read".to_owned(),
                                input: serde_json::json!({}),
                                gemini_context: None,
                            },
                        ]),
                    },
                    Message {
                        role: Role::User,
                        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                            tool_use_id: "call-private".to_owned(),
                            content: "private tool output".to_owned(),
                            is_error: None,
                        }]),
                    },
                ],
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let reader = SessionReader::load(tmp.path(), &id).unwrap();
        assert!(reader.received_messages().is_empty());
        let jsonl = fs::read_to_string(tmp.path().join(format!("{id}.jsonl"))).unwrap();
        let sidecar = fs::read_to_string(tmp.path().join(format!("{id}.replay.json"))).unwrap();
        assert!(!jsonl.contains("secret reasoning"));
        assert!(!jsonl.contains("private tool output"));
        assert!(!jsonl.contains("call-private"));
        assert!(!sidecar.contains("secret reasoning"));
        assert!(!sidecar.contains("private tool output"));
        assert!(sidecar.contains("call-private"));
    }

    #[test]
    fn unsupported_semantic_checkpoint_clears_stale_provider_history() {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        recorder
            .record_received(&FromAgentMessage::ConversationSnapshot {
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_owned(),
                messages: semantic_tool_pair_fixture(),
                processed_queue_ids: vec![7, 9],
            })
            .unwrap();
        assert_eq!(
            recorder.semantic_processed_queue_ids(),
            &HashSet::from([7, 9])
        );
        recorder.flush_checkpoint().unwrap();
        recorder
            .record_received(&FromAgentMessage::ConversationSnapshot {
                processed_queue_ids: vec![7, 9],
                protocol_version: "evalops.maestro.semantic-conversation.v999".to_owned(),
                messages: vec![],
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        assert!(
            SessionRecorder::resume(tmp.path(), &id)
                .unwrap()
                .semantic_processed_queue_ids()
                .is_empty()
        );
        assert!(
            SessionReader::load(tmp.path(), &id)
                .unwrap()
                .replay()
                .semantic_conversation
                .is_none()
        );
    }

    #[test]
    fn replay_sidecar_bounds_semantic_persistence_as_history_grows() {
        fn persisted_bytes(turns: usize) -> u64 {
            let tmp = TempDir::new().unwrap();
            let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
            let id = recorder.id().to_owned();
            for turn in 0..turns {
                let messages = (0..=turn)
                    .map(|index| Message {
                        role: Role::User,
                        content: MessageContent::Text(format!("turn-{index}: {}", "x".repeat(256))),
                    })
                    .collect();
                recorder.record_semantic_conversation(messages).unwrap();
                recorder.flush_checkpoint().unwrap();
            }
            let journal = fs::metadata(recorder.path()).unwrap().len();
            let sidecar = fs::metadata(tmp.path().join(format!("{id}.replay.json")))
                .unwrap()
                .len();
            let journal_text = fs::read_to_string(recorder.path()).unwrap();
            assert!(
                !journal_text.contains("semantic_conversation"),
                "semantic history must not be duplicated in append-only checkpoints"
            );
            journal + sidecar
        }

        let eight_turns = persisted_bytes(8);
        let sixteen_turns = persisted_bytes(16);
        assert!(
            sixteen_turns < eight_turns * 3,
            "latest-sidecar persistence should grow linearly, not quadratically: {eight_turns} -> {sixteen_turns}"
        );
    }

    #[test]
    fn resume_uses_latest_sidecar_anchor_and_applies_only_the_tail() {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        recorder
            .record_sent(&ToAgentMessage::Init {
                system_prompt: Some("historical init".to_owned()),
                append_system_prompt: None,
                thinking_level: None,
                approval_mode: None,
                history: None,
            })
            .unwrap();
        recorder.flush_checkpoint().unwrap();
        for index in 0..10 {
            recorder
                .record_received(&FromAgentMessage::Status {
                    message: format!("historical status {index}"),
                })
                .unwrap();
        }
        recorder
            .record_sent(&ToAgentMessage::Init {
                system_prompt: Some("tail init".to_owned()),
                append_system_prompt: None,
                thinking_level: None,
                approval_mode: None,
                history: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
        assert_eq!(
            replay.last_init.and_then(|init| init.system_prompt),
            Some("tail init".to_owned())
        );
    }

    #[test]
    fn sidecar_publish_before_checkpoint_preserves_new_semantic_snapshot_on_interruption() {
        let tmp = TempDir::new().unwrap();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_owned();
        let messages = semantic_tool_pair_fixture();
        recorder
            .record_semantic_conversation(messages.clone())
            .unwrap();

        // Simulate termination after the pre-checkpoint atomic publication and
        // before the semantic-free journal checkpoint can be appended.
        recorder.write_replay_sidecar().unwrap();
        drop(recorder);

        let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
        assert_eq!(
            serde_json::to_value(replay.semantic_conversation).unwrap(),
            serde_json::to_value(Some(sanitize_semantic_conversation(&messages))).unwrap()
        );
    }

    #[test]
    fn restore_manifest_uses_checkpoint_after_process_death() {
        let tmp = TempDir::new().unwrap();
        let messages = semantic_tool_pair_fixture();
        let mut recorder = SessionRecorder::new(tmp.path()).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_semantic_conversation(messages.clone())
            .unwrap();
        recorder.flush_checkpoint().unwrap();

        // A later facade-only snapshot is the state saved in a drain manifest;
        // it must not erase the durable provider checkpoint before restart.
        recorder
            .apply_snapshot(AgentState::default(), None)
            .unwrap();
        recorder.flush_checkpoint().unwrap();
        drop(recorder);

        let replay = SessionRecorder::resume(tmp.path(), &id).unwrap().replay();
        assert_eq!(
            serde_json::to_value(replay.semantic_conversation).unwrap(),
            serde_json::to_value(Some(sanitize_semantic_conversation(&messages))).unwrap()
        );
    }

    #[test]
    fn test_session_record_and_load() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        // Create and record a session
        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();

        // Record a prompt
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Hello, world!".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();

        // Record a response
        recorder
            .record_received(&FromAgentMessage::Ready {
                protocol_version: Some("2026-03-30".to_string()),
                model: "claude-3-opus".to_string(),
                provider: "anthropic".to_string(),
                session_id: Some("sess_123".to_string()),
            })
            .unwrap();

        recorder.flush().unwrap();
        drop(recorder);

        // Load the session
        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.prompts().len(), 1);
        assert_eq!(reader.prompts()[0], "Hello, world!");
        assert_eq!(reader.metadata().title.as_deref(), Some("Hello, world!"));
        assert_eq!(reader.metadata().model.as_deref(), Some("claude-3-opus"));
        assert_eq!(
            reader.metadata().protocol_version.as_deref(),
            Some("2026-03-30")
        );
        assert_eq!(
            reader.metadata().agent_session_id.as_deref(),
            Some("sess_123")
        );
    }

    #[test]
    fn recorder_entry_points_create_missing_session_directories() {
        let tmp = TempDir::new().unwrap();
        let new_sessions = tmp.path().join("new").join("sessions");
        let resumed_sessions = tmp.path().join("resumed").join("sessions");

        let new_recorder = SessionRecorder::with_id(&new_sessions, "new-session").unwrap();
        assert!(new_sessions.is_dir());
        assert!(new_recorder.path().exists());

        let resumed_recorder =
            SessionRecorder::resume(&resumed_sessions, "resumed-session").unwrap();
        assert!(resumed_sessions.is_dir());
        assert!(resumed_recorder.path().exists());
    }

    #[test]
    fn received_executable_args_are_portable_redacted_before_persistence() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();
        let client_secret = "sk-ant-abcdefghijklmnopqrstuvwxyz123456";
        let server_secret = "correct-horse-battery-staple";

        let mut recorder = SessionRecorder::with_id(sessions_dir, "redacted-session").unwrap();
        recorder
            .record_received(&FromAgentMessage::ClientToolRequest {
                call_id: "client-call".to_string(),
                tool_execution_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({
                    "command": format!(
                        "curl -H 'Authorization: Bearer {client_secret}' example.test"
                    )
                }),
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::ServerRequest {
                request_id: "server-request".to_string(),
                request_type: ServerRequestType::ClientTool,
                call_id: "server-call".to_string(),
                tool_execution_id: None,
                tool: "http".to_string(),
                args: serde_json::json!({
                    "payload": format!("password={server_secret}"),
                }),
                reason: "execute on client".to_string(),
                started_at_ms: None,
            })
            .unwrap();

        let live_args = serde_json::to_string(&recorder.replay_state().pending_client_tools)
            .expect("serialize live pending client tools");
        assert!(live_args.contains(client_secret));
        assert!(live_args.contains(server_secret));
        recorder.maybe_write_checkpoint(true).unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let jsonl =
            fs::read_to_string(sessions_dir.join("redacted-session.jsonl")).expect("read JSONL");
        assert!(!jsonl.contains(client_secret), "{jsonl}");
        assert!(!jsonl.contains(server_secret), "{jsonl}");
        assert!(
            jsonl.contains("[REDACTED:token:portable-export]"),
            "{jsonl}"
        );
        assert!(
            jsonl.contains("[REDACTED:password:portable-export]"),
            "{jsonl}"
        );

        let reader = SessionReader::load(sessions_dir, "redacted-session").unwrap();
        let recorded =
            serde_json::to_string(reader.received_messages().as_slice()).expect("serialize replay");
        let replayed = serde_json::to_string(&reader.replay_state().pending_client_tools)
            .expect("replay state");
        for persisted in [&recorded, &replayed] {
            assert!(!persisted.contains(client_secret), "{persisted}");
            assert!(!persisted.contains(server_secret), "{persisted}");
            assert!(persisted.contains("[REDACTED:"), "{persisted}");
        }
    }

    #[test]
    fn test_session_reader_replay_restores_state_and_init() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();

        recorder
            .record_sent(&ToAgentMessage::Init {
                system_prompt: Some("You are Maestro".to_string()),
                append_system_prompt: Some("Stay concise".to_string()),
                thinking_level: Some(super::super::messages::ThinkingLevel::High),
                approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
                history: None,
            })
            .unwrap();

        recorder
            .record_received(&FromAgentMessage::Ready {
                protocol_version: Some("2026-03-30".to_string()),
                model: "claude-3-opus".to_string(),
                provider: "anthropic".to_string(),
                session_id: Some("sess_ready".to_string()),
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::SessionInfo {
                session_id: Some("sess_info".to_string()),
                cwd: "/tmp/project".to_string(),
                git_branch: Some("main".to_string()),
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::ResponseStart {
                response_id: "resp_1".to_string(),
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::ResponseChunk {
                response_id: "resp_1".to_string(),
                content: "Partial reply".to_string(),
                is_thinking: false,
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::ToolCall {
                call_id: "call_1".to_string(),
                tool_execution_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({ "cmd": "git status" }),
                requires_approval: true,
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::UtilityCommandStarted {
                command_id: "cmd_owned".to_string(),
                command: "echo hi".to_string(),
                cwd: Some("/tmp/project".to_string()),
                shell_mode: super::super::messages::UtilityCommandShellMode::Direct,
                terminal_mode: super::super::messages::UtilityCommandTerminalMode::Pipe,
                pid: Some(1234),
                columns: None,
                rows: None,
                owner_connection_id: Some("conn_owned".to_string()),
            })
            .unwrap();
        recorder
            .record_received(&FromAgentMessage::UtilityFileWatchStarted {
                watch_id: "watch_owned".to_string(),
                root_dir: "/tmp/project".to_string(),
                include_patterns: Some(vec!["src/**".to_string()]),
                exclude_patterns: Some(vec!["dist/**".to_string()]),
                debounce_ms: 50,
                owner_connection_id: Some("conn_owned".to_string()),
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        let replay = reader.replay();

        assert_eq!(
            replay.last_init,
            Some(InitConfig {
                system_prompt: Some("You are Maestro".to_string()),
                append_system_prompt: Some("Stay concise".to_string()),
                thinking_level: Some(super::super::messages::ThinkingLevel::High),
                approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
                history: None,
                code_mode: None,
                tool_grant: None,
            })
        );
        assert_eq!(replay.state.protocol_version.as_deref(), Some("2026-03-30"));
        assert_eq!(replay.state.session_id.as_deref(), Some("sess_info"));
        assert_eq!(replay.state.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(replay.state.git_branch.as_deref(), Some("main"));
        assert!(replay.state.is_ready);
        assert!(replay.state.is_responding);
        assert_eq!(
            replay
                .state
                .current_response
                .as_ref()
                .map(|response| response.text.as_str()),
            Some("Partial reply")
        );
        assert_eq!(replay.state.pending_approvals.len(), 1);
        assert_eq!(replay.state.pending_approvals[0].tool, "bash");
        assert_eq!(
            replay
                .state
                .active_utility_commands
                .get("cmd_owned")
                .and_then(|command| command.owner_connection_id.as_deref()),
            Some("conn_owned")
        );
        assert_eq!(
            replay
                .state
                .active_file_watches
                .get("watch_owned")
                .and_then(|watch| watch.owner_connection_id.as_deref()),
            Some("conn_owned")
        );
    }

    #[test]
    fn test_last_init_returns_most_recent_init_message() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();

        recorder
            .record_sent(&ToAgentMessage::Init {
                system_prompt: Some("First".to_string()),
                append_system_prompt: None,
                thinking_level: Some(super::super::messages::ThinkingLevel::Low),
                approval_mode: Some(super::super::messages::ApprovalMode::Auto),
                history: None,
            })
            .unwrap();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Hello".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder
            .record_sent(&ToAgentMessage::Init {
                system_prompt: Some("Second".to_string()),
                append_system_prompt: None,
                thinking_level: Some(super::super::messages::ThinkingLevel::Ultra),
                approval_mode: Some(super::super::messages::ApprovalMode::Fail),
                history: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        assert_eq!(
            reader.last_init(),
            Some(InitConfig {
                system_prompt: Some("Second".to_string()),
                append_system_prompt: None,
                thinking_level: Some(super::super::messages::ThinkingLevel::Ultra),
                approval_mode: Some(super::super::messages::ApprovalMode::Fail),
                history: None,
                code_mode: None,
                tool_grant: None,
            })
        );
    }

    #[test]
    fn test_list_sessions() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        // Create a few sessions
        let mut r1 = SessionRecorder::new(sessions_dir).unwrap();
        r1.record_sent(&ToAgentMessage::Prompt {
            content: "First session".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
        r1.flush().unwrap();

        let mut r2 = SessionRecorder::new(sessions_dir).unwrap();
        r2.record_sent(&ToAgentMessage::Prompt {
            content: "Second session".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .unwrap();
        r2.flush().unwrap();

        // List sessions
        let sessions = list_sessions(sessions_dir).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_delete_session() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        // Create a session
        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Test".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        // Verify files exist
        assert!(sessions_dir.join(format!("{}.jsonl", id)).exists());
        assert!(sessions_dir.join(format!("{}.meta.json", id)).exists());
        let spill_dir = sessions_dir.join("tool-output").join(&id);
        fs::create_dir_all(&spill_dir).unwrap();
        fs::write(spill_dir.join("large.txt"), "output").unwrap();

        // Delete the session
        delete_session(sessions_dir, &id).unwrap();

        // Verify files are gone
        assert!(!sessions_dir.join(format!("{}.jsonl", id)).exists());
        assert!(!sessions_dir.join(format!("{}.meta.json", id)).exists());
        assert!(!spill_dir.exists());
    }

    #[test]
    fn delete_session_succeeds_when_post_commit_spill_cleanup_fails() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();
        let id = "spill-cleanup-failure";
        fs::write(sessions_dir.join(format!("{id}.jsonl")), "{}\n").unwrap();
        fs::write(sessions_dir.join(format!("{id}.meta.json")), "{}").unwrap();
        fs::write(sessions_dir.join("tool-output"), "blocks spill directory").unwrap();

        delete_session(sessions_dir, id).expect("transcript deletion is the commit point");

        assert!(!sessions_dir.join(format!("{id}.jsonl")).exists());
        assert!(!sessions_dir.join(format!("{id}.meta.json")).exists());
        assert!(sessions_dir.join("tool-output").exists());
    }

    #[test]
    fn test_resume_session() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        // Create initial session
        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "First message".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        // Resume the session
        let mut recorder = SessionRecorder::resume(sessions_dir, &id).unwrap();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Second message".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        // Load and verify
        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        assert_eq!(reader.prompts().len(), 2);
    }

    /// Regression test for a torn `.meta.json`: before the fix, both
    /// `SessionRecorder::resume` and `SessionReader::load` hard-failed with
    /// an `InvalidData` IO error on a corrupt metadata file, even though the
    /// JSONL log (the actual conversation history) was fully intact. A user
    /// hitting a crash right as metadata flushed would be unable to resume
    /// a session that was otherwise perfectly recoverable.
    #[test]
    fn resume_tolerates_corrupt_metadata_instead_of_hard_failing() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Before the crash".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        // Simulate a crash mid-`fs::write` of the metadata file: truncated,
        // invalid JSON.
        let meta_path = sessions_dir.join(format!("{id}.meta.json"));
        fs::write(&meta_path, "{\"id\":\"partial").unwrap();

        // resume() must succeed rather than propagating a parse error.
        let mut recorder = SessionRecorder::resume(sessions_dir, &id)
            .expect("resume must tolerate corrupt metadata, not hard-fail");
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "After the resume".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        // The corrupt file must be preserved as forensic evidence, not
        // silently overwritten in place.
        let rotated: Vec<_> = fs::read_dir(sessions_dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".meta.json.corrupt.")
            })
            .collect();
        assert_eq!(
            rotated.len(),
            1,
            "corrupt metadata should be rotated aside, not discarded"
        );

        // The conversation history survived the crash even though metadata
        // did not: both prompts (before and after) must still be present.
        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        assert_eq!(reader.prompts().len(), 2);

        // list_sessions must not hide a session just because its metadata
        // was corrupt -- that would make a crash look identical to the
        // session never having existed.
        let sessions = list_sessions(sessions_dir).unwrap();
        assert!(sessions.iter().any(|meta| meta.id == id));
    }

    /// Regression test: a `.meta.json` torn mid-write can just as easily be
    /// torn in the middle of a multi-byte UTF-8 character as in the middle
    /// of a JSON token. Before the fix, `load_metadata_tolerant` used
    /// `fs::read_to_string`, which failed with `InvalidData` before the
    /// tolerant JSON-parsing branch (exercised by
    /// `resume_tolerates_corrupt_metadata_instead_of_hard_failing` above)
    /// ever ran -- so this exact kind of corruption still hard-failed
    /// `resume` and silently dropped the session from `list_sessions`,
    /// even though the equivalent ASCII-corruption case was already fixed.
    #[test]
    fn resume_and_list_sessions_tolerate_invalid_utf8_metadata() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Before the crash".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        // Simulate a crash mid-write torn in the middle of a multi-byte
        // UTF-8 character: a valid JSON prefix followed by a lone
        // continuation byte, which is invalid UTF-8 on its own.
        let meta_path = sessions_dir.join(format!("{id}.meta.json"));
        let mut torn = br#"{"id":"partial","title":"caf"#.to_vec();
        torn.push(0xE9); // incomplete multi-byte sequence, not valid UTF-8
        fs::write(&meta_path, &torn).unwrap();
        assert!(
            std::str::from_utf8(&torn).is_err(),
            "test fixture must actually be invalid UTF-8"
        );

        let mut recorder = SessionRecorder::resume(sessions_dir, &id)
            .expect("resume must tolerate invalid-UTF-8 metadata, not hard-fail");
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "After the resume".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let rotated: Vec<_> = fs::read_dir(sessions_dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".meta.json.corrupt.")
            })
            .collect();
        assert_eq!(
            rotated.len(),
            1,
            "invalid-UTF-8 metadata should be rotated aside, not discarded"
        );

        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        assert_eq!(reader.prompts().len(), 2);

        let sessions = list_sessions(sessions_dir).unwrap();
        assert!(sessions.iter().any(|meta| meta.id == id));
    }

    /// When corrupt metadata is rotated aside on resume, the lost fields
    /// (title, message count) are rebuilt from the still-intact JSONL log
    /// instead of being permanently reset to defaults on the next flush.
    #[test]
    fn resume_rebuilds_corrupt_metadata_from_jsonl_log() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Recovered title".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let meta_path = sessions_dir.join(format!("{id}.meta.json"));
        fs::write(&meta_path, "{\"id\":\"partial").unwrap();

        let mut recorder = SessionRecorder::resume(sessions_dir, &id).unwrap();
        assert_eq!(
            recorder.metadata().title.as_deref(),
            Some("Recovered title"),
            "title must be rebuilt from the JSONL log, not reset"
        );
        assert_eq!(recorder.metadata().message_count, 1);
        recorder.flush().unwrap();
        drop(recorder);

        let reader = SessionReader::load(sessions_dir, &id).unwrap();
        assert_eq!(reader.metadata().title.as_deref(), Some("Recovered title"));
    }

    /// Regression test: calling `SessionReader::load` directly (not through
    /// `SessionRecorder::resume`) on a session with corrupt metadata must
    /// also rebuild the lost fields from the JSONL log, not silently reset
    /// them to defaults -- and must persist that rebuild, since this
    /// read-only path has no later `flush()` to do so, and without it a
    /// later `list_sessions` scan would find no `.meta.json` for this
    /// session at all (it was rotated aside above, with nothing to replace
    /// it) and treat a fully recoverable session as if it never existed.
    #[test]
    fn direct_session_reader_load_rebuilds_and_persists_corrupt_metadata() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Loaded directly".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let meta_path = sessions_dir.join(format!("{id}.meta.json"));
        fs::write(&meta_path, "{\"id\":\"partial").unwrap();

        let reader = SessionReader::load(sessions_dir, &id)
            .expect("load must tolerate corrupt metadata, not hard-fail");
        assert_eq!(
            reader.metadata().title.as_deref(),
            Some("Loaded directly"),
            "title must be rebuilt from the JSONL log via a direct load, not reset"
        );
        assert_eq!(reader.metadata().message_count, 1);

        // The rebuild must be persisted, not just returned in memory: a
        // later listing has to be able to find this session again.
        assert!(
            meta_path.exists(),
            "rebuilt metadata must be persisted after a direct load, not left absent"
        );
        let sessions = list_sessions(sessions_dir).unwrap();
        let found = sessions
            .iter()
            .find(|meta| meta.id == id)
            .expect("session must still be discoverable after a direct load rebuilt its metadata");
        assert_eq!(found.title.as_deref(), Some("Loaded directly"));
    }

    #[test]
    fn rebuilt_metadata_persistence_errors_are_propagated() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("session.meta.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("occupied"), "keep").unwrap();

        let metadata = SessionMetadata::new("session");
        let err = persist_rebuilt_metadata(&target, &metadata)
            .expect_err("replacing a non-empty directory must fail");

        assert!(
            !err.to_string().is_empty(),
            "the replacement error must reach the caller"
        );
        assert!(target.join("occupied").exists());
    }

    /// After `list_sessions` rotates a corrupt metadata file aside, it
    /// persists the rebuilt metadata so subsequent listings keep showing
    /// the session instead of hiding it (no `.meta.json` left to match).
    #[test]
    fn list_sessions_persists_rebuilt_metadata_after_rotation() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path();

        let mut recorder = SessionRecorder::new(sessions_dir).unwrap();
        let id = recorder.id().to_string();
        recorder
            .record_sent(&ToAgentMessage::Prompt {
                content: "Still here".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            })
            .unwrap();
        recorder.flush().unwrap();
        drop(recorder);

        let meta_path = sessions_dir.join(format!("{id}.meta.json"));
        fs::write(&meta_path, "{\"id\":\"partial").unwrap();

        let first = list_sessions(sessions_dir).unwrap();
        assert!(first.iter().any(|meta| meta.id == id));
        assert!(
            meta_path.exists(),
            "rebuilt metadata must be persisted after rotation"
        );

        let second = list_sessions(sessions_dir).unwrap();
        let meta = second
            .iter()
            .find(|meta| meta.id == id)
            .expect("session must stay discoverable in subsequent listings");
        assert_eq!(meta.title.as_deref(), Some("Still here"));
        assert_eq!(meta.message_count, 1);
    }

    #[test]
    fn test_session_metadata_usage() {
        let mut metadata = SessionMetadata::new("test");

        // Add some usage
        metadata.add_usage(&TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: None,
            total_tokens: None,
            model_id: None,
            provider: None,
        });

        metadata.add_usage(&TokenUsage {
            input_tokens: 150,
            output_tokens: 300,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: None,
            total_tokens: None,
            model_id: None,
            provider: None,
        });

        assert_eq!(metadata.total_input_tokens, 250);
        assert_eq!(metadata.total_output_tokens, 500);
    }

    #[test]
    fn test_title_truncation() {
        let mut metadata = SessionMetadata::new("test");

        let long_message = "a".repeat(200);
        metadata.set_title_from_prompt(&long_message);

        assert!(metadata.title.as_ref().unwrap().len() <= 80);
        assert!(metadata.title.as_ref().unwrap().ends_with("..."));
    }
}
