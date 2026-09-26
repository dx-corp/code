//! Native Rust agent implementation
//!
//! A fully native agent implementation that communicates directly with AI providers,
//! replacing the previous Node.js subprocess architecture with pure Rust for better
//! performance, type safety, and integration.
//!
//! # Architecture
//!
//! The agent uses a background task architecture to enable non-blocking operations:
//!
//! - **[`NativeAgent`]**: Lightweight handle held by the composing host. All methods
//!   return immediately, sending commands via channels.
//! - **`NativeAgentRunner`**: Private background task that owns mutable state, processes
//!   commands, and manages the AI conversation loop.
//! - **Channel communication**: All interaction happens through Tokio MPSC channels,
//!   enabling true async/non-blocking behavior.
//!
//! # Lifecycle
//!
//! ```text
//! 1. Host calls NativeAgent::start_with_resolved_client(...)
//!    ├─> Spawns background tokio::spawn(runner.run())
//!    └─> Returns (agent_handle, event_receiver)
//!
//! 2. Host calls agent.prompt(message)
//!    └─> Sends AgentCommand::Prompt via channel (returns immediately)
//!
//! 3. Background runner receives command
//!    ├─> Adds message to conversation history
//!    ├─> Calls AI provider API (streaming)
//!    ├─> Sends FromAgent::ResponseChunk events
//!    └─> Handles tool calls if requested
//!
//! 4. Host receives events from event_receiver
//!    └─> Updates UI in real-time
//! ```
//!
//! # Async Task Spawning
//!
//! The agent uses `tokio::spawn` to run the background task. This allows the host
//! task to remain responsive while the agent processes long-running AI requests:
//!
//! ```rust,ignore
//! tokio::spawn(async move {
//!     runner.run().await;
//! });
//! ```
//!
//! The spawned task runs independently and communicates exclusively via channels.
//!
//! # Channel Communication (MPSC)
//!
//! Three unbounded MPSC (multi-producer, single-consumer) channels coordinate
//! communication between the host and agent:
//!
//! 1. **Command channel** (`mpsc::UnboundedSender<AgentCommand>`):
//!    - Host sends commands (prompt, cancel, `set_model`, etc.)
//!    - Agent receives and processes in order
//!
//! 2. **Event channel** (`mpsc::UnboundedSender<FromAgent>`):
//!    - Agent sends events (response chunks, tool calls, errors)
//!    - Host receives and projects events
//!
//! 3. **Tool response channel** (`mpsc::UnboundedSender<ToolResponseMessage>`):
//!    - Host sends user approval for tool execution
//!    - Agent waits for approval before executing restricted tools
//!
//! Unbounded channels are used because:
//! - Commands are user-initiated and low-volume
//! - Hosts consume streamed events and own their presentation or transport
//! - Tool responses are synchronous (one response per tool call)
//!
//! # Cancellation
//!
//! The agent supports mid-request cancellation using `CancellationToken`:
//!
//! ```rust,ignore
//! // In runner
//! let cancel_token = CancellationToken::new();
//! tokio::select! {
//!     res = self.run_loop() => res,
//!     _ = cancel_token.cancelled() => {
//!         Err(anyhow::anyhow!("Request cancelled"))
//!     }
//! }
//! ```
//!
//! When the user presses Escape or sends `AgentCommand::Cancel`, the token is
//! triggered and the current request stops gracefully.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::credential_store::CredentialAttestation;
use super::extensions::{
    BatchEndContext, ExtensionRegistry, ExtensionVerdict,
    ToolCallContext as ExtensionToolCallContext, ToolResultContext as ExtensionToolResultContext,
    ToolResultPayload, TurnEndContext, TurnStartContext,
};
use super::message_queue::{
    MAX_PENDING_MESSAGES, MessageQueue, PendingMessage, PromptKind, QueuePlacement,
};
use super::native_host::{
    ApprovalMode, NativeContextEffect, NativeExecutionHostHandle, NativeFirewallVerdict,
    NativeHookEvent, NativeHookResult, NativeModelCapabilities, NativeModelRoute,
    NativeResolvedClient, NativeToolExecutionOptions, QueueMode, ToolDefinition,
};
use super::reminders::{ReminderEngine, ToolOutcome as ReminderToolOutcome};
use super::safety::stable_stringify;
use super::safety::{DenialMemory, WorkflowStateTracker, apply_workflow_state_hooks};
use super::text_loop::{
    LoopKind, TextLoopDetector, billed_empty_reminder_message, loop_reminder_message,
};
use super::turn_budget::{DEFAULT_MAX_TURN_STEPS, TurnOutcome, TurnStepBudget};
use super::{
    CredentialVault, DenialReason, ExecutionPhase, ExecutionSource, FromAgent,
    ManagedInferenceAuthorization, ManagedPolicyMetadata, TokenUsage, ToolExecution, ToolOutcome,
    ToolResult, ensure_untrusted_content_policy,
};
use crate::ai::{
    AiProvider, ContentBlock, ImageSource, Message, MessageContent, ProviderStreamErrorKind,
    RequestConfig, Role, StopReason, StreamEvent, ThinkingConfig, Tool, UnifiedClient,
    is_retryable_partial_content_stream_failure, provider_model_name,
};
use crate::{
    approval_span, record_model_usage, record_outcome, terminal_span, tool_span_for_call, turn_span,
};
use tracing::Instrument;

pub fn managed_turn_lineage_id(
    organization_id: &str,
    workspace_id: &str,
    thread_id: &str,
    run_id: &str,
    turn_id: &str,
) -> String {
    let mut material = b"maestro-managed-turn-v2".to_vec();
    for value in [organization_id, workspace_id, thread_id, run_id, turn_id] {
        material.extend_from_slice(&(value.len() as u64).to_be_bytes());
        material.extend_from_slice(value.as_bytes());
    }
    format!("maestro-turn-v2:{:x}", Sha256::digest(material))
}

#[derive(Debug)]
struct EmptyAssistantResponse;

impl std::fmt::Display for EmptyAssistantResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "empty_assistant_response: provider completed the turn without assistant text or tool calls",
        )
    }
}

impl std::error::Error for EmptyAssistantResponse {}

/// A host refused admission for a logical provider request.  This is kept as
/// a distinct error so the request retry classifier cannot mistake a lease or
/// event-ledger decision for a transient provider failure and open a fresh
/// request round.
#[derive(Debug)]
struct ProviderAdmissionDenied {
    kind: String,
    request_id: String,
    reason: String,
}

impl std::fmt::Display for ProviderAdmissionDenied {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider admission denied for {} request {}: {}",
            self.kind, self.request_id, self.reason
        )
    }
}

impl std::error::Error for ProviderAdmissionDenied {}

/// Derive the identity of one logical provider request from its immutable
/// input snapshot.  The identity survives an outer request retry and a
/// worker replay while changing when the model receives a new history, model,
/// or provider-request kind.  Transport-level retries happen below this
/// boundary and therefore reuse the same admission.
fn provider_request_id(kind: &str, model: &str, messages: &[Message]) -> Result<String> {
    provider_request_id_with_tail(kind, model, messages, None)
}

fn provider_request_id_with_tail(
    kind: &str,
    model: &str,
    messages: &[Message],
    tail: Option<&str>,
) -> Result<String> {
    // Borrow the transcript instead of cloning its text, images and tool JSON.
    // The serialized sequence is identical to appending one owned user message.
    let tail_message = tail.map(|text| Message {
        role: Role::User,
        content: MessageContent::text(text),
    });
    let encoded_messages = if let Some(tail_message) = &tail_message {
        let sequence: Vec<&Message> = messages
            .iter()
            .chain(std::iter::once(tail_message))
            .collect();
        serde_json::to_vec(&sequence)?
    } else {
        serde_json::to_vec(messages)?
    };
    // Keep the v1 length-delimited hash material exactly, without a second
    // transcript-sized allocation just to concatenate it before hashing.
    let mut digest = Sha256::new();
    for value in [
        kind.as_bytes(),
        model.as_bytes(),
        encoded_messages.as_slice(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    Ok(format!("native-provider-v1:{kind}:{:x}", digest.finalize()))
}

/// Describe a batch's tool results for the reminder engine.
///
/// A `ToolResult` block carries the call id, not the tool name, so the names
/// come from the `ToolUse` blocks of `assistant` -- the assistant message the
/// batch answers, which the runner pushes immediately above the batch.
fn tool_outcomes_for_batch(
    host: &NativeExecutionHostHandle,
    assistant: Option<&Message>,
    results: &[ContentBlock],
) -> Vec<ReminderToolOutcome> {
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    if let Some(Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(blocks),
    }) = assistant
    {
        for block in blocks {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    results
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let tool = names
                    .get(tool_use_id.as_str())
                    .map_or_else(|| "unknown".to_string(), |name| name.to_lowercase());
                let success = !is_error.unwrap_or(false);
                let open_todos = (tool == "todo" && success)
                    .then(|| host.open_todo_count(content))
                    .flatten();
                Some(ReminderToolOutcome {
                    tool,
                    success,
                    open_todos,
                })
            }
            _ => None,
        })
        .collect()
}

/// Append reminder text to the last `ToolResult` block of a batch.
///
/// Returns whether a block was found. The reminder never becomes its own
/// message: a provider request pairs every `tool_use` with a `tool_result`,
/// and inserting a message between them makes the request invalid.
fn append_reminder_to_last_tool_result(results: &mut [ContentBlock], reminder: &str) -> bool {
    for block in results.iter_mut().rev() {
        if let ContentBlock::ToolResult { content, .. } = block {
            content.push_str("\n\n");
            content.push_str(reminder);
            return true;
        }
    }
    false
}

fn begin_queued_user_turn(
    reminders: &mut ReminderEngine,
    denial_memory: &mut DenialMemory,
    step_budget: &mut TurnStepBudget,
) {
    reminders.reset_turn();
    denial_memory.begin_turn();
    step_budget.reset();
}

/// How long one text-delta chunk may spend inside the loop detector.
///
/// The detector is O(period limit) per character, so a pathological chunk
/// could cost real time on the streaming path. The check is abandoned for
/// that chunk when the budget runs out and resumes on the next one.
const TEXT_LOOP_CHECK_BUDGET: Duration = Duration::from_millis(500);

/// The model repeated itself again after one steering attempt.
#[derive(Debug)]
struct AssistantTextLoop {
    kind: LoopKind,
}

impl std::fmt::Display for AssistantTextLoop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "assistant_text_loop: the model repeated the same {} pattern {} times after a \
             steering reminder, so the turn was stopped",
            self.kind.label(),
            self.kind.repetitions(),
        )
    }
}

impl std::error::Error for AssistantTextLoop {}

#[derive(Debug)]
struct ProviderStreamFailure {
    kind: ProviderStreamErrorKind,
    message: String,
}

impl std::fmt::Display for ProviderStreamFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "provider_stream_error: {}", self.message)
    }
}

impl std::error::Error for ProviderStreamFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestFailureOwner {
    Request,
    /// `partial_content_retryable` is set from
    /// [`is_retryable_partial_content_stream_failure`]: whether the failure
    /// is a stream that closed or stalled mid-response *after* content had
    /// already been forwarded downstream, rather than every other typed
    /// stream failure.
    ProviderStream {
        partial_content_retryable: bool,
    },
}

fn request_retry_decision(
    retry_policy: &mut super::retry::RetryPolicy,
    error_kind: super::retry::ErrorKind,
    owner: RequestFailureOwner,
) -> super::retry::RetryDecision {
    match owner {
        RequestFailureOwner::ProviderStream {
            partial_content_retryable: true,
        } => {
            // The stream closed or stalled mid-response after forwarding
            // assistant content, but `run_loop_inner` discards that partial
            // content instead of committing it to conversation history
            // before returning this failure (see its `stream_failed`
            // branch). Retrying therefore replays the request from the last
            // committed message, not the tokens the provider already sent,
            // so -- unlike every other provider-stream failure below -- one
            // dropped stream does not have to end the whole agent session.
            // Bounded and backed off by the same policy every other request
            // retry uses, and the turn's discarded-attempt budget still caps
            // the total regardless of how many times this fires.
            match retry_policy.should_retry(super::retry::ErrorKind::Transient) {
                super::retry::RetryDecision::Retry { delay, attempt, .. } => {
                    super::retry::RetryDecision::Retry {
                        delay,
                        attempt,
                        reason: "Provider stream closed mid-response; discarding the partial reply and retrying the model turn"
                            .to_string(),
                    }
                }
                give_up @ super::retry::RetryDecision::GiveUp { .. } => give_up,
            }
        }
        RequestFailureOwner::ProviderStream {
            partial_content_retryable: false,
        } => {
            // UnifiedClient owns retries once a provider stream has opened. A
            // typed stream failure is therefore already the terminal outcome of
            // that policy. Retrying the entire request here would multiply the
            // stream budget and can keep a hosted turn non-terminal beyond its
            // controller deadline. Request/open failures still use this outer
            // policy because no stream-level retry owner exists for them.
            super::retry::RetryDecision::GiveUp {
                reason: "Provider stream retry policy reached a terminal outcome".to_string(),
            }
        }
        RequestFailureOwner::Request => retry_policy.should_retry(error_kind),
    }
}

fn coding_turn_completed_event(
    executor: &NativeExecutionHostHandle,
    response_id: &str,
) -> Result<FromAgent, String> {
    let (coding_completion, coding_child_records) = match executor.coding_completion()? {
        Some(completion) => (Some(completion.submission), completion.child_records),
        None => (None, Vec::new()),
    };
    Ok(FromAgent::TurnCompleted {
        response_id: response_id.to_owned(),
        coding_completion,
        coding_child_records,
    })
}

fn closed_tool_response_failure(call_id: &str) -> anyhow::Error {
    anyhow::Error::new(ProviderStreamFailure {
        kind: ProviderStreamErrorKind::TransientProtocol,
        message: format!("tool approval response channel closed before `{call_id}` completed"),
    })
}
mod attachments;
mod cancellation;
mod codex;
mod commands;
mod context;
#[cfg(test)]
mod context_overflow_tests;
mod deferred_tool_schemas;
#[cfg(test)]
mod deferred_tool_tests;
mod model_dynamics;
mod provider_history;
mod provider_loop;
mod read_only_tools;
mod side_questions;
#[cfg(test)]
mod token_efficiency_tests;
mod tool_execution;
mod tool_responses;
mod tool_results;

pub use self::deferred_tool_schemas::ExternalToolSchemaPolicy;
use self::deferred_tool_schemas::{
    ToolProfile, effective_tool_definitions, initial_active_tool_names, tool_search_profile_allows,
};

use self::tool_execution::{
    ApprovalDecision, DeferredToolCall, DeferredToolCallDisposition, PostExecutionHookInput,
    PostExecutionHooks, ToolCallContext, abort_pending_tools_after_stream_error,
    append_hook_context, approved_inline_env_change_rejection, approved_input_change_rejection,
    cancel_deferred_suffix, cancelled_deferred_tool, clear_stashed_prompts,
    deferred_approved_policy_rejection, deferred_firewall_verdict, deferred_hook_block,
    deferred_policy_rejection_event, deferred_rejection_output_event,
    deferred_safety_rejection_event, deferred_tool_call_disposition, deferred_tool_call_event,
    emit_deferred_failure, emit_deferred_policy_failure, invalidate_cache_after_serial_tool,
    normalize_post_hook_tool_args, parse_tool_input, repeat_refusal_message,
    rerun_deferred_pre_tool_use, run_post_execution_hooks, run_pre_tool_use_hook,
    tool_args_for_execution, tool_is_visible_to_model, tool_requires_approval,
};

use self::read_only_tools::{
    QueuedReadOnlyToolExecution, execute_native_read_only_tool_wave,
    is_explicit_inline_read_only_tool, is_native_parallel_read_only_tool_call,
};
use self::tool_responses::repair_orphaned_tool_calls;
/// Compatibility exports for callers that historically imported these types
/// from `maestro_runtime::agent`.
pub use crate::{ToolResponseConsumption, ToolResponseMessage};
use crate::{ToolResponseCoordinator, ToolResponseWait};

fn provider_id(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::Anthropic => "anthropic",
        AiProvider::Bedrock => "bedrock",
        AiProvider::OpenAI => "openai",
        AiProvider::Mistral => "mistral",
        AiProvider::Google => "google",
        AiProvider::Groq => "groq",
        AiProvider::VertexAi => "vertex-ai",
        AiProvider::DeepSeek => "deepseek",
        AiProvider::Moonshot => "moonshot",
        AiProvider::Qwen => "dashscope",
        AiProvider::MiniMax => "minimax",
        AiProvider::Zai => "zai",
        AiProvider::Scripted => "scripted-replay",
    }
}

fn policy_model_id(model: &str) -> String {
    if model.contains('/') {
        model.to_string()
    } else {
        let provider = AiProvider::from_model(model);
        format!("{}/{}", provider_id(provider), model)
    }
}

fn provider_request_policy_model_id(active_model: &str, request_model: &str) -> String {
    let active = policy_model_id(active_model);
    let Some((provider, _)) = active.split_once('/') else {
        return policy_model_id(request_model);
    };
    if request_model
        .split_once('/')
        .is_some_and(|(request_provider, model_id)| {
            request_provider.eq_ignore_ascii_case(provider) && !model_id.is_empty()
        })
    {
        request_model.to_owned()
    } else {
        format!("{provider}/{request_model}")
    }
}

fn is_tool_result_only_user_message(message: &Message) -> bool {
    message.role == Role::User
        && matches!(
            &message.content,
            MessageContent::Blocks(blocks)
                if !blocks.is_empty()
                    && blocks
                        .iter()
                        .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
        )
}

/// Drop legacy-alias properties from the model-facing tool schema.
///
/// Execution still accepts aliases in the tool handlers; they are only omitted
/// from the request payload to shrink every-turn tool definitions.
fn compact_tool_for_model(mut tool: Tool) -> Tool {
    let Some(properties) = tool
        .input_schema
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    else {
        return tool;
    };
    properties.retain(|_name, schema| {
        let description = schema
            .get("description")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        !description.to_ascii_lowercase().contains("legacy alias")
    });
    tool
}

fn count_transcript_entries_before(messages: &[Message], first_kept_index: usize) -> usize {
    messages
        .iter()
        .take(first_kept_index.min(messages.len()))
        .filter(|message| !is_tool_result_only_user_message(message))
        .count()
}

fn emit_compaction_event(
    event_tx: &mpsc::UnboundedSender<FromAgent>,
    messages: &[Message],
    summary: &str,
    cut_point: Option<&super::compaction::CutPoint>,
    continuation: Option<&super::compaction::ContinuationRecord>,
    auto: bool,
) {
    let first_kept_entry_index = cut_point
        .map(|point| count_transcript_entries_before(messages, point.first_kept_index))
        .unwrap_or(0);
    let tokens_before = cut_point.map(|point| point.tokens_before).unwrap_or(0);

    let _ = event_tx.send(FromAgent::Compaction {
        summary: summary.to_string(),
        first_kept_entry_index,
        tokens_before,
        auto,
        custom_instructions: None,
        continuation: continuation.cloned(),
        timestamp: Utc::now().to_rfc3339(),
    });
}

/// Provenance for the per-request output limit.
///
/// Catalog-derived limits follow model switches. Explicit limits are stable
/// across switches, even when their numeric value equals a catalog default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensSource {
    Catalog,
    Explicit,
}

/// Configuration for the native agent
///
/// Defines the AI model settings, system prompt, thinking capabilities, and execution
/// environment for the agent. All fields can be updated at runtime via agent methods.
///
/// # Examples
///
/// ```rust,ignore
/// use maestro_runtime::agent::{ApprovalMode, NativeAgentConfig};
///
/// // Default configuration (Codex on OpenAI)
/// let config = NativeAgentConfig::default();
/// assert_eq!(config.model, "gpt-5.1-codex-max");
///
/// // Custom configuration with thinking enabled
/// let config = NativeAgentConfig {
///     model: "claude-opus-4-5-20251101".to_string(),
///     max_tokens: 32768,
///     max_tokens_source: maestro_runtime::agent::MaxTokensSource::Explicit,
///     system_prompt: Some("You are a helpful coding assistant.".to_string()),
///     thinking_enabled: true,
///     thinking_budget: 20000,
///     cwd: "/path/to/project".to_string(),
///     approval_mode: ApprovalMode::Selective,
///     context_window: None,
///     sandbox_policy: None,
///     max_turn_steps: maestro_runtime::agent::DEFAULT_MAX_TURN_STEPS,
///     allow_unbounded_turn: false,
///     retry_config: maestro_runtime::agent::retry::RetryConfig::default(),
///     managed_mcp_policy: None,
///     model_dynamics: Default::default(),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct NativeAgentConfig {
    /// Model to use (e.g., "gpt-5.1-codex-max", "claude-opus-4-5-20251101")
    ///
    /// The model string is parsed by `UnifiedClient` to determine the provider
    /// (Anthropic, `OpenAI`, etc.) and model variant.
    pub model: String,

    /// Maximum tokens for responses
    ///
    /// Limits the length of generated responses. Different models support different
    /// max token values (check provider documentation).
    pub max_tokens: u32,

    /// Whether `max_tokens` came from model metadata or explicit configuration.
    pub max_tokens_source: MaxTokensSource,

    /// System prompt
    ///
    /// Optional instructions prepended to every conversation. Used to set the agent's
    /// role, coding standards, and behavioral guidelines.
    pub system_prompt: Option<String>,

    /// Whether extended thinking is enabled
    ///
    /// When true, the model uses a separate reasoning phase before generating the
    /// final response. Currently only supported by Claude Opus 4.5 and newer.
    pub thinking_enabled: bool,

    /// Token budget for thinking (if enabled)
    ///
    /// Maximum tokens allocated to the thinking/reasoning phase. Only used when
    /// `thinking_enabled` is true. Typical values: 5000-20000.
    pub thinking_budget: u32,

    /// Explicit local routing preferences, loaded from the user configuration by default.
    pub model_dynamics: super::model_dynamics::ModelDynamicsConfig,

    /// Current working directory
    ///
    /// The directory where file operations and commands are executed. Tools like
    /// `bash`, `read`, and `write` use this as their base path.
    pub cwd: String,

    /// Active approval mode for the tool-execution gate.
    ///
    /// This is the single source of truth for whether a tool call needs
    /// human approval before the runner executes it inline (see
    /// `NativeAgentRunner::run_loop`'s `requires_approval` computation).
    /// Callers embedding a caller-owned approval UI (the interactive TUI,
    /// headless server, etc.) must keep this in sync with their own mode
    /// selector via `NativeAgent::set_approval_mode` so the runner's
    /// auto-execute decision and the caller's approval UI never disagree.
    pub approval_mode: ApprovalMode,

    /// Optional explicit context-window override. When absent, the runner uses
    /// the active model catalog instead of a fixed compaction threshold.
    pub context_window: Option<u64>,

    /// Native OS sandbox policy applied to the runner's *own* tool executor.
    ///
    /// This is the executor that actually runs auto-approved calls: every
    /// [`ApprovalMode::Yolo`] call, and every [`ApprovalMode::Selective`]
    /// call the per-tool heuristic doesn't flag for approval, executes
    /// through `NativeAgentRunner::execute_tool` (see `run_loop`), which
    /// dispatches to this executor -- not to whatever separately-configured
    /// executor a caller (the interactive TUI's `App`, `print_mode`, the
    /// headless server) might use for its own approval-gated calls. A
    /// caller that resolves a sandbox policy for itself but does not also
    /// pass it here gets no sandboxing at all for the common case: only
    /// calls that actually reach a human approval prompt would ever have
    /// been sandboxed, and Yolo mode never asks a human anything.
    pub sandbox_policy: Option<maestro_sandbox::SandboxPolicy>,

    /// Maximum provider round trips inside a single turn.
    ///
    /// One step is one request/response pair with the provider. A turn spends
    /// a step every time the model answers with tool calls and the runner has
    /// to ask again with the results. `run_loop` refuses the tool batch that
    /// would need a step past this bound and ends the turn with
    /// [`TurnOutcome::StepBudgetExhausted`] rather than looping forever.
    ///
    /// Ignored when `allow_unbounded_turn` is true. Values below 1 are
    /// clamped to 1.
    pub max_turn_steps: usize,

    /// Remove the `max_turn_steps` ceiling for this agent.
    ///
    /// Set this only for a caller that owns an equivalent bound of its own or
    /// deliberately runs unattended without one. An unbounded turn has no
    /// other terminator: the doom-loop detector in
    /// [`crate::agent::safety`] blocks only three identical consecutive
    /// calls, so a model alternating between two calls never stops.
    pub allow_unbounded_turn: bool,

    /// Retry policy for provider request failures within the current turn.
    ///
    /// Callers may extend this bounded window, but retries remain inside the
    /// owning runtime so completed tool effects are never replayed.
    pub retry_config: super::retry::RetryConfig,

    /// Doom-loop and per-tool rate-limit thresholds enforced by the loop's
    /// safety tenant.
    pub safety_config: super::safety::SafetyConfig,

    /// Initial model visibility for caller-owned tool schemas.
    ///
    /// Deferred schemas remain registered and discoverable through
    /// `tool_search`, then become visible on the following provider request.
    /// The eager default preserves existing embedding contracts.
    pub external_tool_schema_policy: ExternalToolSchemaPolicy,
}

impl NativeAgentConfig {
    /// The step ceiling `run_loop` enforces, after the unbounded opt-out.
    #[must_use]
    pub fn resolved_max_turn_steps(&self) -> usize {
        if self.allow_unbounded_turn {
            usize::MAX
        } else {
            self.max_turn_steps.max(1)
        }
    }
}

impl Default for NativeAgentConfig {
    fn default() -> Self {
        let model = "gpt-5.1-codex-max".to_string();
        Self {
            model,
            // The host replaces catalog-derived values when it composes the
            // runtime.  Keep this fallback independent of the TUI catalog so
            // the runtime remains usable by headless hosts.
            max_tokens: 16_384,
            max_tokens_source: MaxTokensSource::Catalog,
            system_prompt: None,
            thinking_enabled: false,
            thinking_budget: 10000,
            model_dynamics: super::model_dynamics::ModelDynamicsConfig::default(),
            cwd: std::env::current_dir()
                .map_or_else(|_| ".".to_string(), |p| p.to_string_lossy().to_string()),
            approval_mode: ApprovalMode::default(),
            context_window: None,
            sandbox_policy: None,
            max_turn_steps: DEFAULT_MAX_TURN_STEPS,
            allow_unbounded_turn: false,
            retry_config: super::retry::RetryConfig::default(),
            safety_config: super::safety::SafetyConfig::default(),
            external_tool_schema_policy: ExternalToolSchemaPolicy::default(),
        }
    }
}

#[derive(Clone)]
struct ModelToolCache {
    goal_tools_visible: bool,
    include_ide_tools: bool,
    active_tool_names: HashSet<String>,
    tools: Arc<Vec<Tool>>,
}

fn validate_tools_with_host(
    host: &NativeExecutionHostHandle,
    allowed_tools: Option<&HashSet<String>>,
    external_tool_definitions: &[ToolDefinition],
) -> Result<()> {
    if let Some(allowed_tools) = allowed_tools {
        for name in allowed_tools {
            let normalized = name.to_ascii_lowercase();
            if !host.has_native_tool(&normalized) || host.is_reserved_tool(name) {
                return Err(anyhow::anyhow!("Unknown allowed tool `{name}`"));
            }
        }
    }
    let native_names = host
        .tool_definitions()
        .iter()
        .map(|definition| definition.tool.name.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut external_names = HashSet::new();
    for definition in external_tool_definitions {
        let name = definition.tool.name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(anyhow::anyhow!("External tool name must not be empty"));
        }
        if native_names.contains(&name) || host.is_mcp_tool(&name) || host.is_reserved_tool(&name) {
            return Err(anyhow::anyhow!(
                "External tool name `{name}` collides with a host, MCP, or reserved tool"
            ));
        }
        if !external_names.insert(name.clone()) {
            return Err(anyhow::anyhow!(
                "Ambiguous external tool name `{name}` has multiple owners"
            ));
        }
    }
    Ok(())
}

fn goal_tools_visible_from_execution(execution: &ToolExecution) -> Option<bool> {
    let ToolOutcome::Succeeded { output } = &execution.outcome else {
        return None;
    };
    let response: Value = serde_json::from_str(output.as_str()).ok()?;
    let status = response.get("goal")?.get("status")?.as_str()?;
    Some(matches!(status, "active" | "paused" | "blocked"))
}

/// Command sent to the background agent runner
///
/// Internal enum used for communication between `NativeAgent` (handle) and
/// `NativeAgentRunner` (background task). These commands are sent via the
/// command channel and processed sequentially by the runner.
///
/// This enum is private to the module - external code interacts through
/// `NativeAgent` methods which create and send these commands.
enum AgentCommand {
    /// Acknowledge only from the outer loop, after active-turn cleanup.
    AwaitIdle {
        reply: oneshot::Sender<()>,
    },
    ApplySelectiveSummary {
        messages: Vec<Message>,
        digest: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SelectiveSummaryPreview {
        reply: oneshot::Sender<Result<super::SelectiveSummaryPreview>>,
    },
    SelectiveSummary {
        selection: super::RangeSelection,
        digest: String,
        instructions: Option<String>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<super::SelectiveSummaryOutcome>,
    },
    Boost,
    SetContextToolExcluded {
        name: String,
        excluded: bool,
    },
    /// User submitted a prompt
    ///
    /// Adds the user message to conversation history and triggers a new
    /// AI completion request. The runner will stream the response via
    /// `FromAgent::ResponseChunk` events.
    Prompt {
        content: String,
        attachments: Vec<String>,
        kind: PromptKind,
        /// Optional queue id for correlating queued prompts with UI state.
        queue_id: Option<u64>,
        /// Correlation identity bound to this exact governed runtime turn.
        managed_request_lineage: Option<String>,
        /// Opaque capability bound to this exact managed runtime turn.
        managed_inference_authorization: Option<ManagedInferenceAuthorization>,
    },

    /// Reinsert a follow-up at the front of the follow-up subsection.
    RequeueFollowUpFront {
        content: String,
        attachments: Vec<String>,
        queue_id: u64,
        managed_request_lineage: Option<String>,
    },

    /// Cancel the current operation
    ///
    /// Triggers the cancellation token to stop the active AI request.
    /// The runner will clean up and send a `FromAgent::ResponseEnd` event.
    Cancel {
        clear_pending: bool,
    },

    /// Cancel a queued prompt by id
    CancelQueued {
        id: u64,
    },

    /// Reorder a queued prompt without changing its id or contents.
    ReorderQueued {
        id: u64,
        placement: QueuePlacement,
    },

    /// Change the active model
    ///
    /// Switches to a different AI model (e.g., from Claude to GPT-5).
    /// The conversation history is preserved.
    SetModel {
        model: String,
    },

    /// Re-resolve catalog/runtime limits for the already active model.
    RefreshModelBudgets,

    /// Update thinking configuration
    ///
    /// Enables or disables the extended thinking mode and sets the token budget.
    SetThinking {
        enabled: bool,
        budget: u32,
    },

    /// Update the per-request output-token limit
    ///
    /// `max_tokens` bounds a single provider response, so a caller enforcing a
    /// budget across a whole run (the subagent scheduler) lowers it between
    /// requests to the allowance that is still unspent.
    SetMaxTokens {
        max_tokens: u32,
    },

    /// Cap the cumulative output tokens this runner may request across a run.
    ///
    /// A caller enforcing a whole-run budget cannot do it with `SetMaxTokens`:
    /// that bounds one request, so the caller has to lower it between
    /// responses, and the command can arrive after the runner has already built
    /// the next request. This hands the accounting to the runner, which
    /// subtracts what each response spent and clamps the request it is about to
    /// build. Sent once, before the prompt it applies to.
    InstallProcessBudget {
        limits: super::process_budget::ProcessBudgetLimits,
        checkpoint: Option<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>>,
        applied: oneshot::Sender<
            Result<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>>,
        >,
    },

    ClearProcessBudget {
        system_prompt: String,
        applied: oneshot::Sender<Result<()>>,
    },

    SetOutputTokenBudget {
        max_total_output_tokens: u32,
    },

    /// Point the runner's tool executor at a different subagent scope.
    ///
    /// The runner's executor -- not the caller's -- is the one that spawns
    /// children, so it stamps the scope onto every child it starts. A new or
    /// resumed conversation rotates the scope on both sides; without this the
    /// caller would drain a scope no new child is ever tagged with.
    SetSubagentParentScope {
        parent_scope_id: String,
    },

    /// Tell the runner which conversation is active.
    ///
    /// The hook system lives here, not in the caller, so this is the only way
    /// the active session id reaches hook payloads and the only place
    /// `SessionStart` and `SessionEnd` can be dispatched from. The runner
    /// compares against the session it currently holds and fires the
    /// transition, so callers just report the new state.
    SetSessionContext {
        session_id: Option<String>,
        /// Canonical persisted JSONL path for observation-only lifecycle
        /// adapters. This is not restore authority.
        transcript_path: Option<String>,
        /// Why the session changed, published as the hook's `source` /
        /// `reason` (`new`, `resume`, `fork`, `exit`).
        reason: String,
        /// Whether this session has a durable owner that will clean up model
        /// tool-output spill files when the session is deleted.
        owns_persistent_tool_spills: bool,
        preserve_compacted_checkpoint: bool,
    },

    /// Point the hook system at a log file (test harness / diagnostics).
    ///
    /// Does not load project hook config; only enables
    /// [`IntegratedHookSystem`]'s existing `log_event` writer so session and
    /// recovery dispatches become assertable without trusting a temp workspace.
    SetHookLogFile {
        path: String,
    },

    /// Enable or disable the live hook system owned by the execution host.
    SetHooksEnabled {
        enabled: bool,
    },

    /// Reload hook configuration in the live execution host.
    ReloadHooks,

    /// Read live hook counts and metrics from the execution host.
    InspectHooks {
        reply: oneshot::Sender<super::native_host::NativeHookRuntimeSnapshot>,
    },

    /// Update whether the goal lifecycle tools are exposed to the model.
    SetGoalToolsVisible {
        visible: bool,
    },

    /// Update the active approval mode
    ///
    /// Keeps the runner's tool-execution gate (`requires_approval`) in sync
    /// with the caller's approval UI so the two never disagree about whether
    /// a tool needs approval before it executes.
    SetApprovalMode {
        mode: ApprovalMode,
    },

    /// Replace the exact governed native allowlist and caller-owned tools
    /// without rebuilding the runner or losing its private conversation state.
    ReplaceGovernedTools {
        allowed_tools: HashSet<String>,
        external_tool_definitions: Vec<ToolDefinition>,
    },

    /// Update steering queue drain mode.
    SetSteeringMode {
        mode: QueueMode,
    },

    /// Update follow-up queue drain mode.
    SetFollowUpMode {
        mode: QueueMode,
    },

    /// Update the system prompt
    ///
    /// Replaces the base system prompt used for subsequent requests.
    SetSystemPrompt {
        system_prompt: String,
    },

    /// Materialize the provider session that owns the current standing prompt.
    ///
    /// Unlike `SetSystemPrompt`, this is acknowledged only after Codex
    /// app-server has accepted `thread/start`. HTTP providers are stateless,
    /// so updating their request configuration is already the install boundary.
    EnsureProviderPromptInstalled {
        applied: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    },

    /// Stage a system prompt to take effect when the next queued prompt runs.
    ///
    /// A prompt queued while the agent is busy needs its skills active for its
    /// own turn. `SetSystemPrompt` cannot express that: the runner drains
    /// commands inside the tool loop, so a prompt sent at enqueue time changes
    /// the turn that is already running.
    ///
    /// Keyed by queue id so each queued prompt gets the skills its own text
    /// triggered. An unkeyed staged value let a prompt inherit instructions
    /// activated only by a later queued prompt, which is wrong regardless of
    /// the order they run in.
    ///
    /// Applying an older entry after a newer one is correct here, not a bug:
    /// each entry is that prompt's own state, so a steer that jumps the queue
    /// does not leak its skills into the prompts behind it.
    SetSystemPromptForQueuedPrompt {
        queue_id: u64,
        system_prompt: String,
    },

    /// Clear conversation history
    ///
    /// Removes all messages from the conversation, starting fresh. Does not
    /// affect configuration (model, thinking, etc.).
    ClearHistory,

    /// Test-only idle barrier; observes actual runner state after prior commands.
    #[cfg(test)]
    InspectSession {
        capture: bool,
        reply: tokio::sync::oneshot::Sender<tests::session_scenarios::SessionState>,
    },

    /// Replace conversation history (used by /rewind and /fork rebuilds).
    ReplaceHistory {
        messages: Vec<Message>,
        continuation: Option<super::compaction::ContinuationRecord>,
    },

    /// Replace history for a delegated child resume without clearing the
    /// credential vault shared with its parent runner.
    ReplaceHistoryPreservingCredentials {
        messages: Vec<Message>,
    },

    /// Append a host-generated user note to conversation history without
    /// starting a model turn. Used for background-task lifecycle notices so
    /// the next completion request sees them (does not trigger a response).
    InjectUserNote {
        content: String,
        applied: tokio::sync::oneshot::Sender<()>,
        consumed: tokio::sync::oneshot::Sender<()>,
    },

    /// Continue from current context without a new user message
    ///
    /// Used for retrying after transient errors (rate limits, 5xx errors),
    /// continuing after context compaction, or resuming interrupted tool execution.
    Continue,
}

/// Handle to the shared native actor.
///
/// Hosts construct it with a resolved provider client and execution adapter,
/// send commands, and consume the returned event stream. The runner owns
/// mutable conversation state and turn execution.
///
/// Use [`Self::shutdown`] to cancel and await cleanup. Dropping the handle
/// preserves the local host's historical detached-task behavior; hosted owners
/// must retain the handle until admitted work has drained.
pub struct NativeAgent {
    managed_authorization: Arc<super::managed_authorization::ManagedAuthorizationCoordinator>,
    host: NativeExecutionHostHandle,
    external_tool_schema_policy: ExternalToolSchemaPolicy,
    managed_run_id: String,
    /// Channel to send commands to the background runner
    ///
    /// Commands are processed sequentially by the runner. Sending is non-blocking.
    command_tx: mpsc::UnboundedSender<AgentCommand>,

    /// Sender for tool responses (kept for creating receivers)
    ///
    /// When the TUI approves or denies a tool execution, it sends the response
    /// via this channel. The agent waits for these responses before proceeding.
    tool_response_tx: mpsc::UnboundedSender<ToolResponseMessage>,

    /// Direct cancellation path shared with the background runner.
    active_cancellation: Arc<Mutex<ActiveCancellation>>,

    /// Channel to send events to the TUI (for `send_ready`)
    ///
    /// Used by helper methods like `send_ready()` and `send_session_info()` to
    /// emit events without going through the background task.
    event_tx: mpsc::UnboundedSender<FromAgent>,

    /// Model name
    ///
    /// Cached for emitting `FromAgent::Ready` events. Updated when the model
    /// is changed via `set_model()`.
    model_name: String,

    /// Provider name
    ///
    /// Cached provider identifier (e.g., "Anthropic", "`OpenAI`"). Used for
    /// status displays and debugging.
    provider_name: String,

    runtime_audit: Arc<RwLock<RuntimeAuditSnapshot>>,
    /// Priority lifecycle signal that prevents buffered prompts from starting
    /// once orderly shutdown begins.
    shutdown_token: CancellationToken,

    /// Background runner lifecycle retained for orderly signal shutdown.
    ///
    /// Normal drops preserve the historical detached-runner behavior. Signal
    /// shutdown consumes the agent, closes the command channel, and awaits
    /// this handle so queued cancellation and tool cleanup finish before App
    /// and session state are dropped.
    runner_handle: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct RuntimeAuditSnapshot {
    pub request_cache: Option<maestro_context::token_counting::RequestCacheSnapshot>,
    pub cache_reuse: Option<maestro_context::token_counting::CacheReuse>,
    pub request_context: Option<super::RequestContextUsage>,
    pub excluded_context_tools: HashSet<String>,
    pub prompt_revision: u64,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
    pub context_window: Option<u64>,
}

/// Input headroom retained when clamping a response to the model context window.
pub const REQUEST_CONTEXT_SAFETY_TOKENS: u64 = 64;

#[derive(Default)]
struct ActiveCancellation {
    request: Option<CancellationToken>,
    tool: Option<CancellationToken>,
    approval: Option<CancellationToken>,
    tool_batch_active: bool,
    terminal_drain_required: bool,
    operation_interrupted: bool,
}

impl ActiveCancellation {
    fn activate_request(&mut self) -> CancellationToken {
        let token = CancellationToken::new();
        self.set_request(Some(token.clone()));
        token
    }

    fn set_request(&mut self, token: Option<CancellationToken>) {
        self.request = token;
        if self.request.is_none() {
            // An interruption marker is meaningful only within the request
            // whose active operation observed it.
            self.operation_interrupted = false;
            self.tool_batch_active = false;
            self.terminal_drain_required = false;
        }
    }

    fn set_tool(&mut self, token: Option<CancellationToken>, terminal_drain_required: bool) {
        if let Some(token) = token.as_ref() {
            if self.operation_interrupted {
                token.cancel();
            }
            self.terminal_drain_required |= terminal_drain_required;
        }
        self.tool = token;
    }

    fn finish_tool_batch(&mut self) -> bool {
        self.tool_batch_active = false;
        self.terminal_drain_required = false;
        std::mem::take(&mut self.operation_interrupted)
    }
}

fn cancel_active_operation(active_cancellation: &Arc<Mutex<ActiveCancellation>>) {
    let mut active = active_cancellation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(token) = active.tool.as_ref() {
        token.cancel();
        active.operation_interrupted = true;
    } else if let Some(token) = active.approval.as_ref() {
        token.cancel();
        active.operation_interrupted = true;
    } else if active.tool_batch_active {
        // The assistant ToolUse message is already in provider history.
        // Let the runner reach a cleanup boundary and repair/close every call
        // instead of dropping the request future from the outer selector.
        active.operation_interrupted = true;
    } else if let Some(token) = active.request.as_ref() {
        token.cancel();
    }
}

fn prompt_kind_starts_main_request(kind: PromptKind) -> bool {
    kind != PromptKind::SideQuestion
}

fn should_defer_prompt_command(kind: PromptKind, cancellation_seen: bool) -> bool {
    kind == PromptKind::Prompt || (cancellation_seen && prompt_kind_starts_main_request(kind))
}

fn append_completed_thinking_block(
    assistant_content: &mut Vec<ContentBlock>,
    current_thinking: &mut String,
    thinking_signature: Option<String>,
) {
    // An omitted thinking block has empty text but still carries the signed
    // encrypted payload needed when this assistant turn is replayed. Keep it
    // whenever a signature arrived, even though no thinking delta was emitted.
    if !current_thinking.is_empty() || thinking_signature.is_some() {
        assistant_content.push(ContentBlock::Thinking {
            thinking: std::mem::take(current_thinking),
            signature: thinking_signature,
        });
    }
}

impl NativeAgent {
    /// Install the exact reviewed child history only if the original is still
    /// unchanged and idle. Credential references retain their existing vault.
    pub fn apply_selective_summary(
        &self,
        messages: Vec<Message>,
        expected_history_digest: String,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        let (reply, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::ApplySelectiveSummary {
                messages,
                digest: expected_history_digest,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Agent is unavailable"))?;
        Ok(receiver)
    }

    /// Preview authoritative provider turns without changing the conversation.
    pub fn start_selective_summary_preview(
        &self,
    ) -> Result<oneshot::Receiver<Result<super::SelectiveSummaryPreview>>> {
        let (reply, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::SelectiveSummaryPreview { reply })
            .map_err(|_| anyhow::anyhow!("Agent is unavailable"))?;
        Ok(receiver)
    }

    /// Request a proposed child history. Cancel explicitly and retain the receiver
    /// to account for any usage already reported by the provider.
    pub fn start_selective_summary(
        &self,
        selection: super::RangeSelection,
        expected_history_digest: String,
    ) -> Result<super::SelectiveSummaryRequest> {
        self.start_selective_summary_with_instructions(selection, expected_history_digest, None)
    }

    pub fn start_selective_summary_with_instructions(
        &self,
        selection: super::RangeSelection,
        expected_history_digest: String,
        instructions: Option<String>,
    ) -> Result<super::SelectiveSummaryRequest> {
        let (reply, receiver) = oneshot::channel();
        let cancellation = CancellationToken::new();
        self.command_tx
            .send(AgentCommand::SelectiveSummary {
                selection,
                digest: expected_history_digest,
                instructions,
                cancellation: cancellation.clone(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Agent is unavailable"))?;
        Ok(super::SelectiveSummaryRequest {
            receiver,
            cancellation,
        })
    }

    /// Start the actor with the host's resolved provider, tool, and policy decisions.
    pub fn start_with_resolved_client(
        config: NativeAgentConfig,
        host: NativeExecutionHostHandle,
        external_tool_definitions: Vec<ToolDefinition>,
        credential_vault: CredentialVault,
        allowed_tools: Option<&HashSet<String>>,
        resolved_client: NativeResolvedClient,
    ) -> Result<(Self, mpsc::UnboundedReceiver<FromAgent>)> {
        // Caller-owned tools must not overwrite host-owned dispatch or inherit
        // an approval grant for a host or dynamic MCP tool. Validate every
        // host definition, not only the active governed subset, before
        // building the registry.
        validate_tools_with_host(&host, allowed_tools, &external_tool_definitions)?;
        let policy_id = policy_model_id(&config.model);
        if let Some(reason) = host.model_allowed(&policy_id) {
            return Err(anyhow::anyhow!(reason));
        }

        let NativeResolvedClient {
            mut client,
            provider_name,
            model_route,
        } = resolved_client;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let managed_authorization = Arc::new(
            super::managed_authorization::ManagedAuthorizationCoordinator::new(event_tx.clone()),
        );
        if let Some(client) = client.as_mut() {
            client.set_managed_authorization_provider(managed_authorization.clone());
        }
        let (tool_response_tx, tool_response_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let shutdown_token = CancellationToken::new();
        // The host has already composed the concrete registry.  Copy only its
        // immutable model-facing projection; the runner never owns a second
        // registry or dispatch path.
        let mut tools: HashMap<String, ToolDefinition> = host
            .tool_definitions()
            .into_iter()
            .filter(|td| {
                allowed_tools.is_none_or(|allowed| allowed.contains(&td.tool.name.to_lowercase()))
            })
            .map(|td| (td.tool.name.clone(), td))
            .collect();
        let external_tools = external_tool_definitions
            .iter()
            .map(|definition| definition.tool.name.to_lowercase())
            .collect::<HashSet<_>>();
        for definition in external_tool_definitions {
            tools.insert(definition.tool.name.to_lowercase(), definition);
        }
        if config.external_tool_schema_policy == ExternalToolSchemaPolicy::Deferred
            && !external_tools.is_empty()
            && !tools.contains_key("tool_search")
        {
            anyhow::bail!(
                "Deferred external tool schemas require the `tool_search` discovery tool"
            );
        }
        let goal_tools_visible = host.goal_tools_visible();
        let include_ide_tools = host.include_ide_tools();
        let tool_profile = ToolProfile::from_env();
        let explicitly_allowed_tools = allowed_tools.cloned().unwrap_or_default();
        let active_tool_names = initial_active_tool_names(
            tool_profile,
            &tools,
            &external_tools,
            allowed_tools,
            config.external_tool_schema_policy,
        );
        let runtime_audit = Arc::new(RwLock::new(RuntimeAuditSnapshot {
            request_cache: None,
            cache_reuse: None,
            request_context: None,
            excluded_context_tools: HashSet::new(),
            prompt_revision: 0,
            system_prompt: config.system_prompt.clone(),
            tools: effective_tool_definitions(
                &tools,
                &active_tool_names,
                &external_tools,
                goal_tools_visible,
                include_ide_tools,
            ),
            max_output_tokens: config.max_tokens,
            context_window: config
                .context_window
                .or_else(|| host.model_context_window(&config.model)),
        }));

        // Tool execution, hook loading, identity binding, and policy all live
        // in the host adapter.  The loop keeps two handles only for source
        // compatibility with the existing call sites; both point to this one
        // concrete host and therefore cannot execute a second path.
        let tool_executor = host.clone();
        let hooks = host.clone();

        // Create the agent extension registry. Loop behaviors -- doom-loop and
        // rate-limit enforcement today -- are registered tenants, not branches
        // inside `run_loop`. See `docs/agent-extensions.md`.
        let dynamics = Arc::new(std::sync::Mutex::new(
            super::model_dynamics::DynamicsState::default(),
        ));
        let mut extensions =
            ExtensionRegistry::with_default_tenants_and_safety(config.safety_config.clone());
        extensions.register(Box::new(
            super::extensions::model_dynamics::ModelDynamicsExtension::new(
                Arc::clone(&dynamics),
                event_tx.clone(),
            ),
        ));

        // Create context compactor for handling long conversations
        let compactor = super::compaction::ContextCompactor::new(
            super::compaction::CompactionConfig::for_model(
                &config.model,
                config
                    .context_window
                    .or_else(|| host.model_context_window(&config.model)),
            ),
        );

        // Create retry policy for transient API errors
        let retry_policy = super::retry::RetryPolicy::new(config.retry_config.clone());

        // Create message queue for pending prompts (bounded)
        let pending_messages = MessageQueue::with_max_size(MAX_PENDING_MESSAGES);
        // The runner drains this queue between tool calls. A tool that blocks
        // holds the turn past that point, so blocking tools get the queue's
        // steering signal and stop waiting when a user message lands.
        tool_executor.set_steer_signal(pending_messages.steer_signal());
        let active_cancellation = Arc::new(Mutex::new(ActiveCancellation::default()));
        let managed_run_id = Uuid::new_v4().to_string();

        // The composing host owns product telemetry.  The runtime forwards the
        // exact event stream once; a TUI/headless consumer may record or render
        // it without introducing a second loop or event transformation layer.

        let runner = NativeAgentRunner {
            managed_authorization: managed_authorization.clone(),
            client,
            model_route,
            codex_session: None,
            codex_history_restore_prefix_len: None,
            codex_active_turn_id: None,
            codex_current_prompt_started: false,
            managed_run_id: managed_run_id.clone(),
            next_managed_turn_id: 0,
            config: config.clone(),
            messages: Arc::new(Vec::new()),
            tools,
            model_tool_cache: None,
            goal_tools_visible,
            include_ide_tools,
            tool_profile,
            experiment_assignment: None,
            baseline_tool_profile: tool_profile,
            explicitly_allowed_tools,
            active_tool_names,
            external_tools,
            tool_executor,
            credential_vault,
            event_tx: event_tx.clone(),
            tool_response_coordinator: ToolResponseCoordinator::new(tool_response_rx),
            command_rx,
            busy: false,
            cancel_token: None,
            active_cancellation: Arc::clone(&active_cancellation),
            shutdown_token: shutdown_token.clone(),
            clear_pending_on_cancel: true,
            hooks,
            owns_persistent_tool_spills: false,
            extensions,
            dynamics,
            boost_original: None,
            current_turn_id: String::new(),
            turn_index: 0,
            turn_tool_calls: 0,
            denial_memory: DenialMemory::new(),
            workflow_state: WorkflowStateTracker::default(),
            compactor,
            token_calibrated_models: HashSet::new(),
            semantic_continuation: None,
            retry_policy,
            pending_messages,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            deferred_commands: VecDeque::new(),
            pending_user_note_consumptions: Vec::new(),
            active_user_note_consumptions: Vec::new(),
            pending_user_note_texts: Vec::new(),
            active_user_note_texts: Vec::new(),
            current_request_user_message_index: None,
            processed_prompt_queue_ids: HashSet::new(),
            prompt_context: None,
            output_token_budget: None,
            output_tokens_spent: 0,
            process_budget: None,
            queued_system_prompts: HashMap::new(),
            system_prompt_revision: 0,
            runtime_prompt_revision: 0,
            runtime_audit: Arc::clone(&runtime_audit),
            codex_correlations: CodexTurnCorrelations::default(),
        };

        let host = runner.tool_executor.clone();

        // Spawn the background task
        let runner_handle = tokio::spawn(async move {
            runner.run().await;
        });

        let agent = Self {
            managed_authorization,
            host,
            external_tool_schema_policy: config.external_tool_schema_policy,
            managed_run_id,
            command_tx,
            tool_response_tx,
            active_cancellation,
            event_tx,
            model_name: config.model,
            provider_name,
            runtime_audit,
            shutdown_token,
            runner_handle: Some(runner_handle),
        };

        Ok((agent, event_rx))
    }

    /// Stable actor identity used by host telemetry and request lineage.
    #[must_use]
    pub fn managed_run_id(&self) -> &str {
        &self.managed_run_id
    }

    /// Validate and replace the externally governed tool catalog for the next command boundary.
    pub fn replace_governed_tools(
        &self,
        allowed_tools: HashSet<String>,
        external_tool_definitions: Vec<ToolDefinition>,
    ) -> Result<()> {
        validate_tools_with_host(&self.host, Some(&allowed_tools), &external_tool_definitions)?;
        if self.external_tool_schema_policy == ExternalToolSchemaPolicy::Deferred
            && !external_tool_definitions.is_empty()
            && !allowed_tools
                .iter()
                .any(|name| name.eq_ignore_ascii_case("tool_search"))
        {
            anyhow::bail!(
                "Deferred external tool schemas require the `tool_search` discovery tool"
            );
        }
        self.command_tx
            .send(AgentCommand::ReplaceGovernedTools {
                allowed_tools,
                external_tool_definitions,
            })
            .map_err(|_| anyhow::anyhow!("Agent command channel closed"))
    }

    /// Return the transient rendezvous for authenticated host authorization replies.
    #[must_use]
    pub fn managed_authorization_coordinator(
        &self,
    ) -> Arc<super::managed_authorization::ManagedAuthorizationCoordinator> {
        self.managed_authorization.clone()
    }

    /// Get the sender for tool responses.
    #[must_use]
    pub fn tool_response_sender(&self) -> mpsc::UnboundedSender<ToolResponseMessage> {
        self.tool_response_tx.clone()
    }

    /// Send the ready event
    pub fn send_ready(&self) {
        let _ = self.event_tx.send(FromAgent::Ready {
            model: self.model_name.clone(),
            provider: self.provider_name.clone(),
        });
    }

    /// Send session info (cwd, git branch, etc.)
    pub fn send_session_info(
        &self,
        cwd: &str,
        session_id: Option<String>,
        git_branch: Option<String>,
    ) {
        let _ = self.event_tx.send(FromAgent::SessionInfo {
            session_id,
            cwd: cwd.to_string(),
            git_branch,
        });
    }

    /// Process a user prompt (non-blocking - sends to background task)
    ///
    /// Sends a prompt to the background agent runner and returns immediately.
    /// The actual AI request happens asynchronously, with results arriving via
    /// the event channel as `FromAgent::ResponseChunk` messages.
    ///
    /// # Parameters
    ///
    /// - `content`: The user's message/prompt
    /// - `attachments`: File paths to attach (images or text files)
    ///
    /// # Returns
    ///
    /// `Ok(())` if the command was sent successfully, `Err` if the channel is closed.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// agent.prompt("Explain async/await in Rust".to_string(), vec![]).await?;
    /// // Returns immediately, response arrives via events
    /// ```
    pub async fn prompt(&self, content: String, attachments: Vec<String>) -> Result<()> {
        self.prompt_with_kind(content, attachments, PromptKind::Prompt, None)
            .await
    }

    /// Send a prompt with an explicit kind (prompt/steer/follow-up).
    pub async fn prompt_with_kind(
        &self,
        content: String,
        attachments: Vec<String>,
        kind: PromptKind,
        queue_id: Option<u64>,
    ) -> Result<()> {
        self.prompt_with_kind_and_lineage(content, attachments, kind, queue_id, None)
            .await
    }

    /// Send a prompt with correlation bound to the prompt rather than session state.
    pub async fn prompt_with_kind_and_lineage(
        &self,
        content: String,
        attachments: Vec<String>,
        kind: PromptKind,
        queue_id: Option<u64>,
        managed_request_lineage: Option<String>,
    ) -> Result<()> {
        self.prompt_with_kind_and_managed_context(
            content,
            attachments,
            kind,
            queue_id,
            managed_request_lineage,
            None,
        )
        .await
    }

    /// Send a prompt with managed correlation and authorization bound to it.
    pub async fn prompt_with_kind_and_managed_context(
        &self,
        content: String,
        attachments: Vec<String>,
        kind: PromptKind,
        queue_id: Option<u64>,
        managed_request_lineage: Option<String>,
        managed_inference_authorization: Option<ManagedInferenceAuthorization>,
    ) -> Result<()> {
        self.command_tx
            .send(AgentCommand::Prompt {
                content,
                attachments,
                kind,
                queue_id,
                managed_request_lineage,
                managed_inference_authorization,
            })
            .map_err(|e| anyhow::anyhow!("Failed to send prompt: {e}"))?;
        Ok(())
    }

    pub async fn requeue_follow_up_front(
        &self,
        content: String,
        attachments: Vec<String>,
        queue_id: u64,
    ) -> Result<()> {
        self.command_tx
            .send(AgentCommand::RequeueFollowUpFront {
                content,
                attachments,
                queue_id,
                managed_request_lineage: None,
            })
            .map_err(|e| anyhow::anyhow!("Failed to requeue follow-up: {e}"))?;
        Ok(())
    }

    /// Clear conversation history
    pub fn clear_history(&self) {
        let _ = self.command_tx.send(AgentCommand::ClearHistory);
    }

    /// Replace conversation history with the provided messages.
    pub fn replace_history(&self, messages: Vec<Message>) {
        self.replace_history_with_continuation(messages, None);
    }

    pub fn replace_history_with_continuation(
        &self,
        messages: Vec<Message>,
        continuation: Option<super::compaction::ContinuationRecord>,
    ) {
        let _ = self.command_tx.send(AgentCommand::ReplaceHistory {
            messages,
            continuation,
        });
    }

    /// Replace conversation history while retaining credentials supplied by
    /// the parent runner for a delegated child resume.
    pub fn replace_history_preserving_credentials(&self, messages: Vec<Message>) {
        let _ = self
            .command_tx
            .send(AgentCommand::ReplaceHistoryPreservingCredentials { messages });
    }

    /// Append a host tool/user note to history without starting a turn.
    ///
    /// Callers must only flush these when the agent is idle so provider
    /// message order stays valid (no user turn between tool results).
    pub fn inject_user_note(
        &self,
        content: impl Into<String>,
    ) -> Result<(
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    )> {
        let content = content.into();
        if content.trim().is_empty() {
            anyhow::bail!("Cannot inject an empty user note");
        }
        let (applied, applied_receiver) = tokio::sync::oneshot::channel();
        let (consumed, consumed_receiver) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(AgentCommand::InjectUserNote {
                content,
                applied,
                consumed,
            })
            .map_err(|error| anyhow::anyhow!("Failed to inject user note: {error}"))?;
        Ok((applied_receiver, consumed_receiver))
    }

    /// Set the model
    pub fn set_model(&self, model: impl Into<String>) -> Result<()> {
        let model = model.into();
        self.command_tx
            .send(AgentCommand::SetModel { model })
            .map_err(|e| anyhow::anyhow!("Failed to set model: {e}"))?;
        Ok(())
    }

    /// Refresh context and catalog-derived output limits for the active model.
    pub fn refresh_model_budgets(&self) -> Result<()> {
        self.command_tx
            .send(AgentCommand::RefreshModelBudgets)
            .map_err(|e| anyhow::anyhow!("Failed to refresh model budgets: {e}"))
    }

    /// Request a bounded boost at the next safe provider request boundary.
    pub fn boost(&self) -> Result<()> {
        self.command_tx
            .send(AgentCommand::Boost)
            .map_err(|e| anyhow::anyhow!("Failed to request boost: {e}"))
    }

    /// Set thinking level
    pub fn set_thinking(&self, enabled: bool, budget: u32) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetThinking { enabled, budget })
            .map_err(|e| anyhow::anyhow!("Failed to set thinking: {e}"))?;
        Ok(())
    }

    /// Set the per-request output-token limit.
    ///
    /// The limit applies to each provider response, so a caller that owns a
    /// budget for an entire run must lower it between requests. The runner
    /// drains this command in its tool loop, so it takes effect on the next
    /// request it builds.
    pub fn set_max_tokens(&self, max_tokens: u32) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetMaxTokens { max_tokens })
            .map_err(|e| anyhow::anyhow!("Failed to set max tokens: {e}"))?;
        Ok(())
    }

    /// Install the signed per-event budget and wait for native acknowledgement.
    /// An existing checkpoint retains spent usage across agent replacement.
    pub async fn install_process_budget(
        &self,
        limits: super::process_budget::ProcessBudgetLimits,
        checkpoint: Option<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>>,
    ) -> Result<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>> {
        let (applied, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::InstallProcessBudget {
                limits,
                checkpoint,
                applied,
            })
            .map_err(|_| anyhow::anyhow!("process budget runner unavailable"))?;
        receiver
            .await
            .context("process budget admission was not acknowledged")?
    }

    /// Retire Process authority at an inactive, verified grant boundary.
    /// Acknowledgement covers both budget removal and the ordinary system prompt.
    pub async fn clear_process_budget(&self, system_prompt: String) -> Result<()> {
        let (applied, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::ClearProcessBudget {
                system_prompt,
                applied,
            })
            .map_err(|_| anyhow::anyhow!("process budget runner unavailable"))?;
        receiver
            .await
            .context("process budget retirement was not acknowledged")?
    }

    /// Cap cumulative output tokens before the prompt they apply to.
    pub fn set_output_token_budget(&self, max_total_output_tokens: u32) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetOutputTokenBudget {
                max_total_output_tokens,
            })
            .map_err(|e| anyhow::anyhow!("Failed to set output token budget: {e}"))?;
        Ok(())
    }

    /// Stage a system prompt for the next queued prompt to start.
    ///
    /// Applied when the runner prepares the next pending message, so it does
    /// not change the turn that is running when it is sent. Superseded by any
    /// later `set_system_prompt`, which is authoritative.
    pub fn set_system_prompt_for_queued_prompt(
        &self,
        queue_id: u64,
        system_prompt: String,
    ) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetSystemPromptForQueuedPrompt {
                queue_id,
                system_prompt,
            })
            .map_err(|e| anyhow::anyhow!("Failed to set queued system prompt: {e}"))?;
        Ok(())
    }

    /// Point the runner's tool executor at a different subagent scope.
    ///
    /// Send this whenever the caller rotates its own scope, so children started
    /// after the change are stamped with the scope the caller now drains.
    pub fn set_subagent_parent_scope(&self, parent_scope_id: String) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetSubagentParentScope { parent_scope_id })
            .map_err(|e| anyhow::anyhow!("Failed to set subagent parent scope: {e}"))?;
        Ok(())
    }

    /// Tell the runner which conversation is active.
    ///
    /// Stamps the session id onto subsequent hook payloads and dispatches the
    /// `SessionEnd` / `SessionStart` hooks for the transition. Pass `None` to
    /// report that the active session ended without a replacement. Set
    /// `owns_persistent_tool_spills` only when a durable session owner also
    /// owns deletion of that session's spill directory.
    pub fn set_session_context(
        &self,
        session_id: Option<String>,
        reason: impl Into<String>,
        owns_persistent_tool_spills: bool,
    ) -> Result<()> {
        self.set_session_context_with_transcript(
            session_id,
            None,
            reason,
            owns_persistent_tool_spills,
        )
    }

    /// Tell the runner which persisted conversation and transcript are active.
    pub fn set_session_context_with_transcript(
        &self,
        session_id: Option<String>,
        transcript_path: Option<String>,
        reason: impl Into<String>,
        owns_persistent_tool_spills: bool,
    ) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetSessionContext {
                session_id,
                transcript_path,
                reason: reason.into(),
                owns_persistent_tool_spills,
                preserve_compacted_checkpoint: false,
            })
            .map_err(|e| anyhow::anyhow!("Failed to set session context: {e}"))?;
        Ok(())
    }

    /// Continue a reviewed summary checkpoint in its newly persisted child.
    /// Unrelated session changes must use the reset-by-default method above.
    pub fn set_compacted_session_context_with_transcript(
        &self,
        session_id: String,
        transcript_path: Option<String>,
        owns_persistent_tool_spills: bool,
    ) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetSessionContext {
                session_id: Some(session_id),
                transcript_path,
                reason: "summarize".into(),
                owns_persistent_tool_spills,
                preserve_compacted_checkpoint: true,
            })
            .map_err(|e| anyhow::anyhow!("Failed to adopt compacted session context: {e}"))?;
        Ok(())
    }

    /// Enable hook event logging to `path` (test harness / diagnostics).
    pub fn set_hook_log_file(&self, path: impl Into<String>) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetHookLogFile { path: path.into() })
            .map_err(|e| anyhow::anyhow!("Failed to set hook log file: {e}"))?;
        Ok(())
    }

    /// Enable or disable hooks in the live native execution host.
    pub fn set_hooks_enabled(&self, enabled: bool) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetHooksEnabled { enabled })
            .map_err(|e| anyhow::anyhow!("Failed to update hook state: {e}"))?;
        Ok(())
    }

    /// Reload hooks from the execution host's authoritative configuration.
    pub fn reload_hooks(&self) -> Result<()> {
        self.command_tx
            .send(AgentCommand::ReloadHooks)
            .map_err(|e| anyhow::anyhow!("Failed to reload hooks: {e}"))?;
        Ok(())
    }

    /// Return the live hook state owned by the native execution host.
    pub async fn inspect_hooks(&self) -> Result<super::native_host::NativeHookRuntimeSnapshot> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::InspectHooks { reply })
            .map_err(|e| anyhow::anyhow!("Failed to inspect hooks: {e}"))?;
        response
            .await
            .map_err(|e| anyhow::anyhow!("Hook inspection was interrupted: {e}"))
    }

    /// Set whether the goal lifecycle tools are exposed to the model.
    pub fn set_goal_tools_visible(&self, visible: bool) {
        let _ = self
            .command_tx
            .send(AgentCommand::SetGoalToolsVisible { visible });
    }

    /// Set the active approval mode.
    ///
    /// The runner is the sole owner of the tool-execution approval decision;
    /// callers with their own approval UI (interactive TUI, headless server)
    /// must call this whenever their user-facing mode selector changes so the
    /// runner's inline auto-execute gate stays consistent with what the UI
    /// tells the user will happen.
    pub fn set_approval_mode(&self, mode: ApprovalMode) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetApprovalMode { mode })
            .map_err(|e| anyhow::anyhow!("Failed to set approval mode: {e}"))?;
        Ok(())
    }

    /// Set steering queue drain mode.
    pub fn set_steering_mode(&self, mode: QueueMode) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetSteeringMode { mode })
            .map_err(|e| anyhow::anyhow!("Failed to set steering mode: {e}"))?;
        Ok(())
    }

    /// Set follow-up queue drain mode.
    pub fn set_follow_up_mode(&self, mode: QueueMode) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetFollowUpMode { mode })
            .map_err(|e| anyhow::anyhow!("Failed to set follow-up mode: {e}"))?;
        Ok(())
    }

    /// Replace the base system prompt
    pub fn set_system_prompt(&self, system_prompt: impl Into<String>) -> Result<()> {
        let system_prompt = system_prompt.into();
        self.command_tx
            .send(AgentCommand::SetSystemPrompt { system_prompt })
            .map_err(|e| anyhow::anyhow!("Failed to set system prompt: {e}"))?;
        Ok(())
    }

    /// Wait until the active provider has accepted the current standing prompt.
    pub async fn ensure_provider_prompt_installed(&self) -> Result<()> {
        let (applied, receiver) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(AgentCommand::EnsureProviderPromptInstalled { applied })
            .map_err(|error| {
                anyhow::anyhow!("Failed to request provider prompt install: {error}")
            })?;
        receiver
            .await
            .map_err(|error| {
                anyhow::anyhow!("Provider prompt install acknowledgement dropped: {error}")
            })?
            .map_err(anyhow::Error::msg)
    }

    pub fn set_context_tool_excluded(&self, name: String, excluded: bool) -> Result<()> {
        self.command_tx
            .send(AgentCommand::SetContextToolExcluded { name, excluded })
            .map_err(|_| anyhow::anyhow!("The agent is not running."))
    }

    #[must_use]
    pub fn runtime_audit_snapshot(&self) -> RuntimeAuditSnapshot {
        self.runtime_audit
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Restore diagnostic hashes from this session, before its first request.
    pub fn restore_request_cache(
        &self,
        snapshot: Option<maestro_context::token_counting::RequestCacheSnapshot>,
    ) {
        let mut audit = self
            .runtime_audit
            .write()
            .unwrap_or_else(|p| p.into_inner());
        audit.request_cache = snapshot;
        audit.cache_reuse = None;
    }

    /// Continue from current context without a new user message
    ///
    /// Used for:
    /// - Retrying after transient errors (rate limits, 5xx errors, overload)
    /// - Continuing after context compaction
    /// - Resuming interrupted tool execution
    ///
    /// # Returns
    ///
    /// `Ok(())` if the continue command was sent, `Err` if the channel is closed.
    pub fn continue_execution(&self) -> Result<()> {
        self.command_tx
            .send(AgentCommand::Continue)
            .map_err(|e| anyhow::anyhow!("Failed to send continue: {e}"))?;
        Ok(())
    }
}

async fn wait_for_retry_delay(
    delay: std::time::Duration,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = request_cancel.cancelled() => false,
        () = shutdown_token.cancelled() => false,
    }
}

async fn wait_for_codex_auth_refresh(
    host: &NativeExecutionHostHandle,
    auth_path: &std::path::Path,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
    max_wait: Duration,
) -> bool {
    let baseline = tokio::fs::read(auth_path).await.ok();
    let wait = async {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let current = tokio::fs::read(auth_path).await.ok();
            if current != baseline && host.codex_auth_is_usable(auth_path) {
                return true;
            }
        }
    };
    tokio::select! {
        result = tokio::time::timeout(max_wait, wait) => result.unwrap_or(false),
        () = request_cancel.cancelled() => false,
        () = shutdown_token.cancelled() => false,
    }
}

enum CancellableLoad<T> {
    Loaded(T),
    RequestCancelled,
    Shutdown,
}

async fn load_until_cancelled<F, T>(
    load: F,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
) -> CancellableLoad<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(load);
    tokio::select! {
        biased;
        () = shutdown_token.cancelled() => CancellableLoad::Shutdown,
        () = request_cancel.cancelled() => CancellableLoad::RequestCancelled,
        loaded = &mut load => CancellableLoad::Loaded(loaded),
    }
}

const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const CODEX_SIDE_QUESTION_TIMEOUT: Duration = Duration::from_mins(2);
const CODEX_SIDE_QUESTION_MAX_CONTEXT_TOKENS: u64 = 20_000;

async fn run_request_with_cancellation<F>(
    request: F,
    request_cancel: &CancellationToken,
    shutdown_token: &CancellationToken,
    active_cancellation: &Arc<Mutex<ActiveCancellation>>,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::pin!(request);
    tokio::select! {
        biased;
        () = shutdown_token.cancelled() => {
            let terminal_drain_required = || {
                active_cancellation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .terminal_drain_required
            };
            if terminal_drain_required() {
                // A mutating native tool may already have committed its
                // workspace change before bounded follow-up work (for example
                // LSP diagnostics) finishes. Keep the request alive through
                // batch cleanup so its one receipt-bearing terminal remains
                // truthful and cannot invite a duplicate retry.
                (&mut request).await
            } else {
                // Provider and legacy waits that have not crossed a mutating
                // native boundary retain bounded shutdown. Recheck after the
                // bound because polling the request may have crossed into a
                // mutating tool after shutdown won the outer selector.
                match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut request).await {
                    Ok(result) => result,
                    Err(_) if terminal_drain_required() => (&mut request).await,
                    Err(_) => Err(anyhow::anyhow!("Request cancelled")),
                }
            }
        }
        () = request_cancel.cancelled() => {
            Err(anyhow::anyhow!("Request cancelled"))
        }
        result = &mut request => result,
    }
}

fn native_tool_requires_terminal_drain(
    _tool_executor: &NativeExecutionHostHandle,
    tool_name: &str,
    args: &serde_json::Value,
) -> bool {
    let action = || args.get("action").and_then(serde_json::Value::as_str);

    match tool_name.to_ascii_lowercase().as_str() {
        // Shell syntax admits filesystem and process effects that cannot be
        // exhaustively proven read-only. Bash execution consumes shutdown
        // cancellation and reaps its process tree, so conservatively retain
        // its receipt-bearing terminal independent of approval-version pins.
        "bash" => true,
        "write" | "notebook_edit" | "todo" => true,
        "edit" => !args
            .get("dryRun")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        "background_tasks" => matches!(action(), Some("start" | "stop")),
        "gh_pr" => matches!(action(), Some("create" | "checkout" | "comment")),
        "gh_issue" => matches!(action(), Some("create" | "comment" | "close")),
        "gh_repo" => matches!(action(), Some("fork" | "clone")),
        // These built-ins are observation-only even when a separate policy
        // asks for approval (for example screenshot capture).
        "read"
        | "glob"
        | "grep"
        | "diff"
        | "list"
        | "find"
        | "search"
        | "parallel_ripgrep"
        | "websearch"
        | "codesearch"
        | "status"
        | "ask_user"
        | "extract_document"
        | "web_fetch"
        | "webfetch"
        | "read_image"
        | "screenshot"
        | "mcp_list_resources"
        | "mcp_list_prompts"
        | "mcp_read_resource"
        | "mcp_get_prompt"
        | "vscode_get_diagnostics"
        | "vscode_get_definition"
        | "vscode_find_references"
        | "vscode_read_file_range"
        | "jetbrains_get_diagnostics"
        | "jetbrains_get_definition"
        | "jetbrains_find_references"
        | "jetbrains_read_file_range" => false,
        // Unknown serial extensions are conservatively receipt-bearing.
        // Explicit read-only inline tools are drained by the parallel wave
        // before reaching this path.
        _ => true,
    }
}

fn command_after_shutdown_check(
    command: AgentCommand,
    shutdown_token: &CancellationToken,
) -> Option<AgentCommand> {
    (!shutdown_token.is_cancelled()).then_some(command)
}

/// The background agent runner that owns mutable state
///
/// This struct is private to the module and runs in a background tokio task.
/// It owns all mutable state (conversation history, configuration) and is the
/// only component that makes AI API calls.
///
/// # Ownership and Mutability
///
/// The runner is moved into `tokio::spawn` and owns:
/// - Conversation history (Vec<Message>)
/// - Configuration (`NativeAgentConfig`)
/// - AI client (`UnifiedClient`)
/// - All channel receivers
///
/// This ensures exclusive ownership and prevents data races - only the background
/// task can modify the agent state.
///
/// # Event Loop
///
/// The `run()` method processes commands in an event loop:
///
/// ```text
/// loop {
///     match command_rx.recv().await {
///         Prompt => run_loop() to handle AI request,
///         Cancel => trigger cancellation token,
///         SetModel => update client,
///         SetSystemPrompt => update base system prompt,
///         ClearHistory => clear messages,
///     }
/// }
/// ```
struct NativeAgentRunner {
    managed_authorization: Arc<super::managed_authorization::ManagedAuthorizationCoordinator>,
    /// Direct AI client for HTTP-provider turns.
    ///
    /// This is absent for Codex app-server models: Codex app-server owns the
    /// ChatGPT login and all turn transport for those models.
    client: Option<UnifiedClient>,

    /// Live Codex app-server session for `openai-codex/*` models.
    ///
    /// Lazily created on the first prompt so ChatGPT OAuth refresh and
    /// `thread/start` / `turn/start` stay owned by Codex.
    codex_session: Option<super::codex_app_server_turns::CodexAppServerTurnSession>,

    /// Number of restored messages owned by the next fresh Codex thread.
    ///
    /// Captured when history is replaced and consumed after a successful
    /// `thread/inject_items`, so prompts appended before lazy session startup
    /// remain live-turn input rather than restored history.
    codex_history_restore_prefix_len: Option<usize>,

    /// Whether the current live prompt crossed a successful Codex
    /// `turn/start` boundary. Pre-turn failures may be retried, but a terminal
    /// GiveUp must not persist an undelivered prompt as provider history.
    codex_current_prompt_started: bool,
    /// Stable identity for this managed-gateway request lineage.
    managed_run_id: String,
    /// Monotonic turn number within the agent run.
    next_managed_turn_id: u64,
    model_route: NativeModelRoute,

    /// Configuration
    ///
    /// Current agent settings. Updated via commands like `SetModel`,
    /// `SetThinking`, and `SetSystemPrompt`.
    config: NativeAgentConfig,

    /// Conversation history
    ///
    /// Stores all messages (user prompts, assistant responses, tool results)
    /// in the current conversation. Cleared via `ClearHistory` command.
    messages: Arc<Vec<Message>>,

    /// Tool definitions
    ///
    /// Map of tool name to tool definition. Loaded from the tool registry
    /// at startup and remains constant.
    tools: HashMap<String, ToolDefinition>,
    codex_active_turn_id: Option<String>,

    /// Cached model-facing tool schemas. The registry is immutable for the
    /// lifetime of a runner; only goal visibility and the IDE-tools flag can
    /// change the filtered view.
    model_tool_cache: Option<ModelToolCache>,

    /// Tool schemas currently exposed to the model. The native `tool_search`
    /// path expands this set on demand without rebuilding the executor.
    active_tool_names: HashSet<String>,

    /// The profile constrains dynamic discovery as well as initial schemas.
    tool_profile: ToolProfile,
    experiment_assignment: Option<maestro_runtime_contracts::experiments::ExperimentAssignment>,
    baseline_tool_profile: ToolProfile,

    /// Built-ins explicitly requested by a caller may bypass profile defaults.
    explicitly_allowed_tools: HashSet<String>,

    /// Whether the current goal exposes `get_goal` and `update_goal`.
    /// Updated by explicit app synchronization and successful `update_goal`
    /// executions; it is intentionally not reloaded from disk per request.
    goal_tools_visible: bool,

    /// Whether the process opted into IDE-only tool schemas. This is static
    /// process configuration, so read it once when the runner is created.
    include_ide_tools: bool,

    /// Tools whose execution is owned by the calling client.
    external_tools: HashSet<String>,

    /// Tool executor for running tools
    ///
    /// Handles actual tool execution (bash, read, write, etc.) and determines
    /// which tools require approval based on command content.
    tool_executor: NativeExecutionHostHandle,

    /// Shared vault for this agent session and its tool executors.
    credential_vault: CredentialVault,

    /// Channel to send events to the TUI
    ///
    /// Used to stream response chunks, tool calls, errors, etc. back to the UI.
    event_tx: mpsc::UnboundedSender<FromAgent>,

    /// Caller-owned tool response state, including the receiver, keyed
    /// responses, and cancellation tombstones.
    tool_response_coordinator: ToolResponseCoordinator,

    /// Channel to receive commands
    ///
    /// Main input for the runner. Receives prompts, cancellation requests,
    /// configuration changes, etc.
    command_rx: mpsc::UnboundedReceiver<AgentCommand>,

    /// Whether currently processing
    ///
    /// Guards against concurrent prompts. Only one AI request can be active
    /// at a time.
    busy: bool,

    /// Cancellation token for the current request
    ///
    /// Created when a prompt starts, triggered when `Cancel` command arrives.
    /// Used with `tokio::select!` to support graceful cancellation.
    cancel_token: Option<CancellationToken>,

    /// Token mirror reachable from the public agent handle while the runner is
    /// blocked awaiting a tool.
    active_cancellation: Arc<Mutex<ActiveCancellation>>,

    /// Priority lifecycle signal checked before deferred or buffered commands.
    shutdown_token: CancellationToken,

    /// Whether a cancellation should also clear pending messages.
    clear_pending_on_cancel: bool,

    /// Hook system for tool interception
    ///
    /// Executes pre/post tool hooks for safety checks, logging, and context injection.
    /// Loaded from ~/.composer/hooks.toml and .composer/hooks.toml.
    hooks: NativeExecutionHostHandle,

    /// Whether the active session owns cleanup of persistent tool-output
    /// spills. An ephemeral session can still have a hook session id.
    owns_persistent_tool_spills: bool,

    /// Ordered agent extensions invoked at the loop's declared hook points.
    ///
    /// Doom-loop and rate-limit enforcement live here as the `doom-loop`
    /// tenant. New loop behavior is registered here rather than branched into
    /// `run_loop`; see `docs/agent-extensions.md`.
    extensions: ExtensionRegistry,
    dynamics: Arc<std::sync::Mutex<super::model_dynamics::DynamicsState>>,
    boost_original: Option<super::model_dynamics::ModelChoice>,

    /// Turn identifier carried in every extension hook context.
    current_turn_id: String,

    /// How many turns `run_loop` has started, including the current one.
    turn_index: u64,

    /// How many tool calls the current turn has planned.
    turn_tool_calls: u64,
    /// Tool calls the user refused during the current turn.
    ///
    /// A denied call with identical arguments is refused again without a
    /// second prompt, and every refusal is retired at the next user turn.
    denial_memory: DenialMemory,

    /// Workflow state tracker for PII redaction enforcement
    workflow_state: WorkflowStateTracker,

    /// Context compactor for conversations approaching the model limit.
    compactor: super::compaction::ContextCompactor,
    /// Models already probed for exact provider token counting.
    token_calibrated_models: HashSet<String>,
    semantic_continuation: Option<super::compaction::ContinuationRecord>,

    /// Retry policy for handling transient API errors
    ///
    /// Implements exponential backoff with jitter for rate limits and server errors.
    retry_policy: super::retry::RetryPolicy,

    /// Message queue for pending user prompts
    ///
    /// When the agent is busy processing a request, incoming prompts are queued
    /// instead of rejected. After each request completes, pending messages are
    /// automatically processed.
    pending_messages: MessageQueue,

    /// Queue drain mode for steering messages.
    steering_mode: QueueMode,

    /// Queue drain mode for follow-up messages.
    follow_up_mode: QueueMode,

    /// Commands observed while the agent is inside a turn and deferred until idle.
    deferred_commands: VecDeque<AgentCommand>,

    /// Applied notes waiting to be included in the next main model turn.
    pending_user_note_consumptions: Vec<tokio::sync::oneshot::Sender<()>>,

    /// Note acknowledgements assigned to the currently running main turn.
    active_user_note_consumptions: Vec<tokio::sync::oneshot::Sender<()>>,

    /// Exact note text paired with pending consumption acknowledgements.
    pending_user_note_texts: Vec<String>,

    /// Exact note text assigned to the currently running main turn.
    active_user_note_texts: Vec<String>,

    /// User message created for the current prompt, if this is a prompt turn.
    current_request_user_message_index: Option<usize>,

    /// Explicit queue ids represented by the next semantic checkpoint.
    processed_prompt_queue_ids: HashSet<u64>,

    /// Extra system prompt context for the current request
    ///
    /// Set by prompt-related hooks and cleared after each request completes.
    prompt_context: Option<String>,

    /// Cumulative output-token ceiling for the whole run, if a caller set one.
    ///
    /// Set by `SetOutputTokenBudget`. `None` means unbounded, which is the
    /// interactive default; the subagent scheduler sets it for delegated runs.
    output_token_budget: Option<u32>,

    /// Output tokens this runner has already spent against
    /// [`Self::output_token_budget`].
    output_tokens_spent: u64,
    process_budget: Option<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>>,

    /// System prompt staged for the next queued prompt to start, with the
    /// [`Self::system_prompt_revision`] that was current when it was staged.
    ///
    /// Populated by `SetSystemPromptForQueuedPrompt` and consumed in
    /// `prepare_pending_message`. A mismatched revision means an authoritative
    /// `SetSystemPrompt` landed after the staging, so the staged value is stale
    /// and dropped.
    queued_system_prompts: HashMap<u64, (u64, String)>,

    /// Bumped by every authoritative system-prompt update.
    ///
    /// Used only to detect that a staged queued prompt has been overtaken.
    system_prompt_revision: u64,

    /// Revision of the prompt actually applied to a model request.
    runtime_prompt_revision: u64,

    runtime_audit: Arc<RwLock<RuntimeAuditSnapshot>>,

    /// Item metadata belongs to one Codex turn, including late same-turn
    /// approvals. Never carry patches or unfinished approvals into another turn.
    codex_correlations: CodexTurnCorrelations,
}

/// itemId → path → per-path patch metadata (may be an empty object).
type CodexFileChangeItemCache = HashMap<String, Map<String, Value>>;

#[derive(Default)]
struct CodexTurnCorrelations {
    file_changes: CodexFileChangeItemCache,
    approved: HashMap<String, CodexNativeToolCorrelation>,
    pending_completions: HashMap<String, bool>,
}

impl CodexTurnCorrelations {
    fn reset(&mut self) {
        // Drop both entries and high-water allocations from large patch turns.
        *self = Self::default();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexNativeToolCorrelation {
    call_id: String,
    tool_name: String,
}

/// One finished Codex tool call, ready to be turned into a wire response.
struct CodexToolOutcome<'a> {
    tool_name: &'a str,
    call_id: &'a str,
    args: &'a Value,
    /// Raw tool output, which is what the hooks contract on.
    hook_output: &'a str,
    /// Model-facing body that injected context is appended to.
    result_text: String,
    is_error: bool,
    /// Context a `PreToolUse` hook asked to add to this call's result.
    pre_hook_context: Option<&'a str>,
    /// Wall-clock time the tool took, for the hooks' `durationMs`.
    duration_ms: u64,
}

/// Build standing instructions from caller context and the model selected for
/// this request. Rebuild from the base prompt so switches and retries never
/// accumulate stale model identities. Shared by HTTP and Codex transports.
#[must_use]
pub fn runtime_system_prompt(
    base: Option<&str>,
    context: Option<&str>,
    model: &str,
    mut capabilities: NativeModelCapabilities,
) -> Option<String> {
    let mut system = base.unwrap_or_default().to_owned();
    if let Some(context) = context.filter(|context| !context.trim().is_empty()) {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(context);
    }
    if !system.is_empty() {
        system.push_str("\n\n");
    }
    system.push_str("Your model is provided by Deixic.\n");
    let model = model.trim();
    if model.is_empty() {
        system.push_str("The current model identifier is unavailable. Do not guess it.");
    } else {
        system.push_str(&format!(
            "The model identifier selected for this request is {model:?}."
        ));
    }
    system.push_str(concat!(
        "\nDeixic provides the agent experience; this does not mean Deixic trained the underlying model. ",
        "A configured model identifier may be an alias; do not invent a more specific model identity."
    ));
    // A zero ceiling is missing metadata, never evidence of a zero-token model.
    capabilities.context_tokens = capabilities.context_tokens.filter(|tokens| *tokens > 0);
    capabilities.output_tokens = capabilities.output_tokens.filter(|tokens| *tokens > 0);
    system.push_str("\nCatalog-reported model capabilities (null means unknown; token limits are ceilings, not remaining budget):\n");
    system.push_str(&serde_json::json!(capabilities).to_string());
    system.push_str(concat!(
        "\nUse these facts to plan work in bounded chunks and keep responses within the runtime's limits. ",
        "If image input is unsupported, use an available tool to obtain text or ask for a text description. ",
        "Only the tools supplied for this request are available; model tool-calling support does not grant access. ",
        "Reasoning support does not say whether reasoning is enabled. ",
        "If the task needs capabilities or reasoning beyond this configuration, explain the concrete limitation ",
        "and ask the caller to adjust the model or reasoning settings. Do not claim a model or setting changed ",
        "until the runtime reports it. Do not infer capability, intelligence, price, or permission from a model name."
    ));
    ensure_untrusted_content_policy(Some(system))
}

/// The staged system prompt to apply to a queued message starting now.
///
/// `None` when nothing is staged, or when an authoritative `SetSystemPrompt`
/// arrived after the staging and bumped `current_revision`. In that case the
/// authoritative prompt is newer and, because skill activation is cumulative,
/// already contains the skills the staged one carried; applying the stale
/// snapshot would revert the newer update.
fn staged_system_prompt_to_apply(
    staged: Option<(u64, String)>,
    current_revision: u64,
) -> Option<String> {
    match staged {
        Some((staged_revision, prompt)) if staged_revision == current_revision => Some(prompt),
        _ => None,
    }
}

fn apply_staged_system_prompt(
    staged_prompts: &mut HashMap<u64, (u64, String)>,
    queue_id: u64,
    authority_revision: u64,
    system_prompt: &mut Option<String>,
    runtime_revision: &mut u64,
) -> bool {
    let Some(staged) =
        staged_system_prompt_to_apply(staged_prompts.remove(&queue_id), authority_revision)
    else {
        return false;
    };
    *system_prompt = Some(staged);
    *runtime_revision = runtime_revision.saturating_add(1);
    true
}

/// The tool name a Codex-native operation is presented to policy hooks under.
///
/// Codex runs these itself, so they have no entry in the Maestro tool registry.
/// A hook still has to be able to name them, and matching on the raw app-server
/// method would tie a policy to protocol spelling, so they are mapped to stable
/// names alongside the tools a hook already knows.
fn codex_native_policy_tool(method: &str) -> &'static str {
    match method {
        "item/fileChange/requestApproval" | "applyPatchApproval" => "codex_file_change",
        _ => "codex_command_execution",
    }
}

/// Characters of model-generated payload in a Codex-native operation request.
///
/// The approval request carries the command line or the patch the model wrote,
/// which is the model-produced half of the exchange. The matching
/// `item/completed` notification carries the operation's *output*, which is
/// input to the model rather than output from it and is deliberately not
/// counted here.
fn codex_native_operation_chars(params: Option<&Value>) -> u64 {
    params
        .and_then(|params| serde_json::to_string(params).ok())
        .map_or(0, |json| json.chars().count() as u64)
}

fn codex_completion_usage_value(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(map) => {
            if let Some(usage) = map.get("usage").filter(|value| value.is_object()) {
                return Some(usage);
            }
            map.values().find_map(codex_completion_usage_value)
        }
        Value::Array(values) => values.iter().find_map(codex_completion_usage_value),
        _ => None,
    }
}

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

pub(super) fn codex_token_usage_from_completion(value: &Value) -> Option<TokenUsage> {
    let usage = codex_completion_usage_value(value)?;
    let input_tokens = usage_u64(
        usage,
        &[
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
            "input",
        ],
    );
    let output_tokens = usage_u64(
        usage,
        &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
            "output",
        ],
    );
    let cache_read_tokens = usage_u64(
        usage,
        &[
            "cache_read_tokens",
            "cacheReadTokens",
            "cachedInputTokens",
            "cached_tokens",
            "cachedTokens",
            "cache_read",
        ],
    )
    .or_else(|| {
        [
            "input_tokens_details",
            "inputTokensDetails",
            "prompt_tokens_details",
        ]
        .iter()
        .find_map(|key| {
            usage
                .get(*key)
                .and_then(|details| usage_u64(details, &["cached_tokens", "cachedTokens"]))
        })
    });
    let cache_write_tokens = usage_u64(
        usage,
        &[
            "cache_write_tokens",
            "cacheWriteTokens",
            "cacheWriteInputTokens",
            "cache_creation_input_tokens",
            "cacheCreationInputTokens",
            "cache_write",
        ],
    );
    if input_tokens.is_none()
        && output_tokens.is_none()
        && cache_read_tokens.is_none()
        && cache_write_tokens.is_none()
    {
        return None;
    }
    Some(TokenUsage {
        input_tokens: input_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
        cache_read_tokens: cache_read_tokens.unwrap_or(0),
        cache_write_tokens: cache_write_tokens.unwrap_or(0),
        cost: usage_u64(usage, &["cost_micros"])
            .map(|micros| micros as f64 / 1_000_000.0)
            .or_else(|| usage.get("cost").and_then(Value::as_f64))
            .or_else(|| usage.get("total_cost_usd").and_then(Value::as_f64)),
    })
}

fn choose_codex_turn_usage(
    completion: &Value,
    usage_notifications: &[Value],
) -> Option<TokenUsage> {
    codex_token_usage_from_completion(completion).or_else(|| {
        usage_notifications
            .iter()
            .rev()
            .find_map(codex_token_usage_from_completion)
    })
}
/// The Codex `sandbox` value matching a Maestro sandbox policy.
///
/// Codex accepts `read-only`, `workspace-write`, and `danger-full-access` on
/// `thread/start`. `None` means the caller set no policy, which leaves the
/// Codex default in place.
fn codex_sandbox_mode(policy: Option<&maestro_sandbox::SandboxPolicy>) -> Option<String> {
    match policy? {
        maestro_sandbox::SandboxPolicy::ReadOnly => Some("read-only".to_owned()),
        maestro_sandbox::SandboxPolicy::WorkspaceWrite { .. } => Some("workspace-write".to_owned()),
        maestro_sandbox::SandboxPolicy::DangerFullAccess => Some("danger-full-access".to_owned()),
    }
}

/// Whether this configuration forbids the agent from changing anything.
///
/// Used to decline Codex-native mutation approvals outright. Codex is asked to
/// sandbox itself via `thread/start`, but that is an external process honoring
/// a request; refusing the approval RPC is enforcement inside Maestro.
fn config_denies_mutation(policy: Option<&maestro_sandbox::SandboxPolicy>) -> bool {
    matches!(policy, Some(maestro_sandbox::SandboxPolicy::ReadOnly))
}

/// Whether the active tool allowlist excludes the Codex-native operation.
///
/// A restrictive specialist profile such as `tools: [read, grep]` removes
/// Maestro mutation tools from the registry-facing set, but Codex-native
/// `commandExecution` and `fileChange` never pass through that set. Without
/// this check, Yolo + a writable sandbox would still accept those RPCs and
/// break the profile's documented narrowing.
fn codex_native_denied_by_active_tools(
    method: &str,
    active_tool_names: &HashSet<String>,
) -> Option<&'static str> {
    let has = |name: &str| {
        active_tool_names
            .iter()
            .any(|active| active.eq_ignore_ascii_case(name))
    };
    match codex_native_policy_tool(method) {
        "codex_file_change" => {
            if has("write") || has("edit") {
                None
            } else {
                Some("active tool allowlist excludes file mutation tools (write/edit)")
            }
        }
        _ => {
            if has("bash") {
                None
            } else {
                Some("active tool allowlist excludes command execution (bash)")
            }
        }
    }
}

/// Whether a Codex `item/tool/call` names a tool absent from the live
/// governed allowlist.
fn codex_tool_call_denied_by_active_tools(
    tool_name: &str,
    active_tool_names: &HashSet<String>,
) -> Option<String> {
    if active_tool_names
        .iter()
        .any(|active| active.eq_ignore_ascii_case(tool_name))
    {
        None
    } else {
        Some(format!("active tool allowlist excludes `{tool_name}`"))
    }
}

fn model_tool_spill_dir_for_active_tools(
    host: Option<&NativeExecutionHostHandle>,
    active_tool_names: &HashSet<String>,
    cwd: &str,
    session_id: Option<&str>,
    owns_persistent_tool_spills: bool,
) -> Option<std::path::PathBuf> {
    if !owns_persistent_tool_spills || !active_tool_names.contains("read") {
        return None;
    }
    session_id.and_then(|session_id| host.map(|host| host.model_tool_spill_dir(cwd, session_id)))
}

/// Merge patch metadata objects.
///
/// Keys present in `from` overwrite `into` (later notifications win). Keys
/// only present in `into` are kept so a partial update does not erase fields
/// the later payload omitted.
fn merge_json_object_fields(into: &mut Value, from: Value) {
    let Value::Object(from_map) = from else {
        if into.is_null() || into == &Value::Object(Map::new()) {
            *into = from;
        }
        return;
    };
    let Value::Object(into_map) = into else {
        *into = Value::Object(from_map);
        return;
    };
    for (key, value) in from_map {
        into_map.insert(key, value);
    }
}

/// Record one path and its patch metadata. Later updates for the same path
/// overwrite keys they carry and keep keys they omit.
fn insert_file_change_entry(entries: &mut Map<String, Value>, path: &str, meta: Value) {
    if let Some(existing) = entries.get_mut(path) {
        merge_json_object_fields(existing, meta);
    } else {
        entries.insert(path.to_owned(), meta);
    }
}

/// Move destinations named on a path entry (`move_path` / `kind.move_path`).
fn file_change_move_destinations(meta: &Value) -> Vec<&str> {
    let mut dests = Vec::new();
    if let Some(dest) = meta
        .get("move_path")
        .or_else(|| meta.get("movePath"))
        .and_then(Value::as_str)
    {
        dests.push(dest);
    }
    if let Some(dest) = meta
        .pointer("/kind/move_path")
        .or_else(|| meta.pointer("/kind/movePath"))
        .and_then(Value::as_str)
    {
        dests.push(dest);
    }
    dests
}

/// Record a path in encounter order and merge its metadata.
fn push_file_change_entry(
    order: &mut Vec<String>,
    entries: &mut Map<String, Value>,
    path: &str,
    meta: Value,
) {
    if !entries.contains_key(path) {
        order.push(path.to_owned());
    }
    insert_file_change_entry(entries, path, meta);
}

/// Record a source path and every move destination it names.
fn push_file_change_entry_with_moves(
    order: &mut Vec<String>,
    entries: &mut Map<String, Value>,
    path: &str,
    meta: Value,
) {
    for dest in file_change_move_destinations(&meta) {
        push_file_change_entry(order, entries, dest, json!({}));
    }
    push_file_change_entry(order, entries, path, meta);
}

/// Paths plus per-path metadata named in a Codex file-change payload.
///
/// Encounter order is preserved (not map-key sort order) so multi-path
/// firewall checks keep a stable first-to-last path sequence.
///
/// Every shape that can carry a rename/move must push **both** the source and
/// every destination path. Missing a destination in one shape is the class of
/// bug that lets Yolo approve a contained source while the out-of-workspace
/// destination is never firewall-checked.
///
/// Observed Codex shapes:
/// - legacy `applyPatchApproval`: paths are keys of a `fileChanges` object
/// - speculative/array forms (`files`, `changes`, single `path`) still accepted
/// - v2 `item/fileChange/requestApproval` often carries only `itemId`; pass
///   previously observed item entries via `known_item_paths`
fn codex_native_file_change_entries(
    params: &Value,
    known_item_paths: Option<&CodexFileChangeItemCache>,
) -> Vec<(String, Value)> {
    let mut order: Vec<String> = Vec::new();
    let mut entries = Map::new();

    // Top-level single path with optional sibling patch fields as metadata.
    if let Some(path) = params
        .get("file_path")
        .or_else(|| params.get("path"))
        .or_else(|| params.get("filePath"))
        .and_then(Value::as_str)
    {
        let mut meta = Map::new();
        for key in ["content", "diff", "patch", "kind", "move_path", "movePath"] {
            if let Some(value) = params.get(key) {
                meta.insert(key.to_owned(), value.clone());
            }
        }
        push_file_change_entry_with_moves(&mut order, &mut entries, path, Value::Object(meta));
    }

    // Legacy applyPatchApproval: { "fileChanges": { "/path": {..., "move_path"?}, ... } }
    if let Some(file_changes) = params
        .get("fileChanges")
        .or_else(|| params.get("file_changes"))
        .and_then(Value::as_object)
    {
        for (path, value) in file_changes {
            push_file_change_entry_with_moves(&mut order, &mut entries, path, value.clone());
        }
    }

    if let Some(files) = params.get("files").and_then(Value::as_array) {
        for file in files {
            if let Some(path) = file.as_str() {
                push_file_change_entry(&mut order, &mut entries, path, json!({}));
            } else if let Some(path) = file
                .get("path")
                .or_else(|| file.get("file_path"))
                .and_then(Value::as_str)
            {
                push_file_change_entry_with_moves(&mut order, &mut entries, path, file.clone());
            }
        }
    }

    // v2 FileChangePatchUpdatedNotification: changes[].path plus
    // changes[].kind.move_path for renames/moves.
    if let Some(changes) = params.get("changes").and_then(Value::as_array) {
        for change in changes {
            if let Some(path) = change
                .get("path")
                .or_else(|| change.get("file_path"))
                .and_then(Value::as_str)
            {
                push_file_change_entry_with_moves(&mut order, &mut entries, path, change.clone());
            } else {
                // Pathless change objects still contribute any move destinations.
                for dest in file_change_move_destinations(change) {
                    push_file_change_entry(&mut order, &mut entries, dest, json!({}));
                }
            }
        }
    }

    // Nested item payload (some notifications wrap the file change).
    if let Some(item) = params.get("item") {
        for (path, meta) in codex_native_file_change_entries(item, None) {
            push_file_change_entry(&mut order, &mut entries, &path, meta);
        }
    }

    // Always union entries cached under this itemId.
    if let Some(item_id) = params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .and_then(Value::as_str)
    {
        if let Some(known) = known_item_paths.and_then(|map| map.get(item_id)) {
            for (path, meta) in known {
                push_file_change_entry(&mut order, &mut entries, path, meta.clone());
            }
        }
    }

    order
        .into_iter()
        .filter_map(|path| entries.remove(&path).map(|meta| (path, meta)))
        .collect()
}

/// Paths named in a Codex file-change payload (encounter order).
fn codex_native_file_change_paths(
    params: &Value,
    known_item_paths: Option<&CodexFileChangeItemCache>,
) -> Vec<String> {
    codex_native_file_change_entries(params, known_item_paths)
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

/// Record paths and patch metadata on a Codex notification under its item id.
fn remember_codex_file_change_item_paths(
    params: &Value,
    known_item_paths: &mut CodexFileChangeItemCache,
) {
    let Some(item_id) = params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .or_else(|| params.pointer("/item/id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let entries = codex_native_file_change_entries(params, None);
    if entries.is_empty() {
        return;
    }
    let entry = known_item_paths.entry(item_id).or_default();
    for (path, meta) in entries {
        insert_file_change_entry(entry, &path, meta);
    }
}

/// Preserve file-change paths carried by an `item/completed` notification.
///
/// The completion stream has a separate lifecycle owner from the ordinary
/// file-change notification stream, but a later item-id-only approval still
/// needs the completed item's paths for policy and firewall evaluation.
fn remember_codex_file_change_completion_paths(
    notification: &crate::codex_app_server::Notification,
    known_item_paths: &mut CodexFileChangeItemCache,
) {
    let Some(params) = notification.params.as_ref() else {
        return;
    };
    let is_file_change = params
        .pointer("/item/type")
        .or_else(|| params.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("fileChange"));
    if is_file_change {
        remember_codex_file_change_item_paths(params, known_item_paths);
    }
}

fn codex_native_item_id(params: Option<&Value>) -> Option<&str> {
    let params = params?;
    params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .or_else(|| params.pointer("/item/id"))
        .and_then(Value::as_str)
        .filter(|item_id| !item_id.trim().is_empty())
}

fn remember_approved_codex_native_operation(
    params: Option<&Value>,
    call_id: &str,
    tool_name: &str,
    correlations: &mut HashMap<String, CodexNativeToolCorrelation>,
) {
    let Some(item_id) = codex_native_item_id(params) else {
        return;
    };
    correlations.insert(
        item_id.to_owned(),
        CodexNativeToolCorrelation {
            call_id: call_id.to_owned(),
            tool_name: tool_name.to_owned(),
        },
    );
}

fn codex_native_completion(
    notification: &crate::codex_app_server::Notification,
) -> Option<(String, bool)> {
    if notification.method != "item/completed" {
        return None;
    }
    let params = notification.params.as_ref()?;
    let item = params.get("item")?;
    if !matches!(
        item.get("type").and_then(Value::as_str),
        Some("fileChange" | "commandExecution")
    ) {
        return None;
    }
    let item_id = item
        .get("id")
        .or_else(|| params.get("itemId"))
        .and_then(Value::as_str)?
        .trim();
    if item_id.is_empty() {
        return None;
    }
    let failed_status = item
        .get("status")
        .or_else(|| params.get("status"))
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "cancelled" | "canceled"));
    let is_error = item
        .get("isError")
        .or_else(|| item.get("is_error"))
        .or_else(|| params.get("isError"))
        .or_else(|| params.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let nonzero_exit = matches!(
        item.get("type").and_then(Value::as_str),
        Some("commandExecution")
    ) && item
        .get("exitCode")
        .or_else(|| item.get("exit_code"))
        .or_else(|| params.get("exitCode"))
        .or_else(|| params.get("exit_code"))
        .and_then(Value::as_i64)
        .is_some_and(|exit_code| exit_code != 0);
    Some((
        item_id.to_owned(),
        !failed_status && !is_error && !nonzero_exit,
    ))
}

#[cfg(test)]
fn project_codex_native_completion(
    notification: &crate::codex_app_server::Notification,
    correlations: &mut HashMap<String, CodexNativeToolCorrelation>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> Option<FromAgent> {
    let (item_id, success) = codex_native_completion(notification)?;
    project_codex_native_completion_parts(item_id, success, correlations, receipt_policy)
}

fn project_codex_native_completion_parts(
    item_id: String,
    success: bool,
    correlations: &mut HashMap<String, CodexNativeToolCorrelation>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> Option<FromAgent> {
    let correlation = correlations.remove(&item_id)?;
    let result = if success {
        ToolResult::success(format!("{} completed", correlation.tool_name))
    } else {
        ToolResult::failure(format!("{} failed", correlation.tool_name))
    };
    let execution = ToolExecution::from_legacy(
        &correlation.call_id,
        &correlation.tool_name,
        ExecutionSource::Native,
        result.clone(),
    )
    .with_managed_policy(receipt_policy);
    Some(FromAgent::ToolEnd {
        call_id: correlation.call_id,
        success,
        result: Some(result),
        receipt: Some(execution.receipt),
    })
}

/// Project a completion immediately when its approval correlation already
/// exists, or retain the authoritative outcome until that correlation is
/// recorded. The app-server may deliver `item/completed` before the approval
/// request, so dropping an unmatched completion would lose the terminal tool
/// receipt permanently.
fn project_or_defer_codex_native_completion(
    notification: &crate::codex_app_server::Notification,
    correlations: &mut HashMap<String, CodexNativeToolCorrelation>,
    pending: &mut HashMap<String, bool>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> Option<FromAgent> {
    let (item_id, success) = codex_native_completion(notification)?;
    if let Some(event) = project_codex_native_completion_parts(
        item_id.clone(),
        success,
        correlations,
        receipt_policy.clone(),
    ) {
        return Some(event);
    }
    pending.insert(item_id, success);
    None
}

fn project_deferred_codex_native_completion(
    params: Option<&Value>,
    pending: &mut HashMap<String, bool>,
    correlations: &mut HashMap<String, CodexNativeToolCorrelation>,
    receipt_policy: Option<ManagedPolicyMetadata>,
) -> Option<FromAgent> {
    let item_id = codex_native_item_id(params)?;
    let success = pending.remove(item_id)?;
    project_codex_native_completion_parts(item_id.to_owned(), success, correlations, receipt_policy)
}

fn discard_deferred_codex_native_completion(
    params: Option<&Value>,
    pending: &mut HashMap<String, bool>,
) {
    if let Some(item_id) = codex_native_item_id(params) {
        pending.remove(item_id);
    }
}

/// Canonical policy-hook input for a Codex-native file-change approval.
///
/// # Contract (class fix for alias/metadata review loops)
///
/// Policy hooks must treat these fields as authoritative:
/// - `paths`: ordered full path set (sources + move destinations)
/// - `fileChanges`: path → metadata object (kind/content/diff/move_path/…)
///
/// Every original path-bearing alias present on the approval is also fully
/// rewritten to that same complete set (`file_changes`, `files`, `changes`).
/// Prefer `paths` + `fileChanges`; aliases exist only for back-compat.
///
/// Correlation: v2 itemId-only approvals are filled from
/// `known_item_paths` (path + metadata cache from earlier notifications).
fn codex_native_policy_hook_args(
    method: &str,
    params: Option<&Value>,
    known_item_paths: &CodexFileChangeItemCache,
) -> Value {
    let base = params.cloned().unwrap_or(Value::Null);
    if codex_native_policy_tool(method) != "codex_file_change" {
        return base;
    }
    let empty = Value::Null;
    let raw = params.unwrap_or(&empty);
    let correlated_entries = codex_native_file_change_entries(raw, Some(known_item_paths));
    if correlated_entries.is_empty() {
        return base;
    }

    let mut enriched = match base {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        other => {
            let mut map = Map::new();
            map.insert("original".to_owned(), other);
            map
        }
    };

    let had_files = enriched.contains_key("files");
    let had_changes = enriched.contains_key("changes");
    let files_prefer_objects = enriched
        .get("files")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(Value::is_object));

    // Canonical complete views — always rewritten from the correlated set.
    let paths: Vec<String> = correlated_entries
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    let mut file_changes = Map::new();
    for (path, meta) in &correlated_entries {
        file_changes.insert(path.clone(), meta.clone());
    }
    let file_changes_value = Value::Object(file_changes);

    enriched.insert("paths".to_owned(), json!(paths));
    // Dual-write map aliases so neither camelCase nor snake_case consumers
    // can see a partial view (the class of bug that produced serial P1s).
    enriched.insert("fileChanges".to_owned(), file_changes_value.clone());
    enriched.insert("file_changes".to_owned(), file_changes_value);

    // Fully rewrite original array aliases to the complete correlated set.
    if had_files {
        enriched.insert(
            "files".to_owned(),
            materialize_path_array(&correlated_entries, files_prefer_objects),
        );
    }
    if had_changes {
        enriched.insert(
            "changes".to_owned(),
            materialize_path_array(&correlated_entries, true),
        );
    }

    Value::Object(enriched)
}

/// Codex executes native approvals from its original request. We cannot hand
/// it vaulted arguments, so deny any payload whose strings would change under
/// vaulting before it reaches hooks or the approval channel. Inspect object
/// keys as well as values: vault_in_json intentionally preserves JSON keys.
fn codex_native_contains_plaintext_credential(vault: &CredentialVault, value: &Value) -> bool {
    match value {
        Value::String(text) => vault.vault_in_text(text) != *text,
        Value::Array(items) => items
            .iter()
            .any(|item| codex_native_contains_plaintext_credential(vault, item)),
        Value::Object(entries) => entries.iter().any(|(key, item)| {
            vault.vault_in_text(key) != *key
                || codex_native_contains_plaintext_credential(vault, item)
        }),
        _ => false,
    }
}

/// Build a `files`/`changes` array from the correlated path/metadata set.
fn materialize_path_array(entries: &[(String, Value)], as_objects: bool) -> Value {
    let items: Vec<Value> = entries
        .iter()
        .map(|(path, meta)| {
            if as_objects {
                let mut entry = match meta {
                    Value::Object(map) => map.clone(),
                    _ => Map::new(),
                };
                entry
                    .entry("path".to_owned())
                    .or_insert_with(|| Value::String(path.clone()));
                Value::Object(entry)
            } else {
                Value::String(path.clone())
            }
        })
        .collect();
    Value::Array(items)
}

/// Map a Codex-native approval request onto Maestro tool argument sets the
/// action firewall already understands.
///
/// Codex does not speak Maestro tool names; its approval params carry the
/// command line or the paths being patched. Normalizing them to `bash` /
/// `write` lets the same dangerous-command and path checks that guard
/// `item/tool/call` also guard the native mutation RPCs. File-change
/// requests with multiple paths produce one `write` argument set per path.
fn codex_native_firewall_arg_sets(
    method: &str,
    params: Option<&Value>,
    known_item_paths: Option<&CodexFileChangeItemCache>,
) -> Vec<(&'static str, Value)> {
    let Some(params) = params else {
        return Vec::new();
    };
    match codex_native_policy_tool(method) {
        "codex_file_change" => {
            let content = params
                .get("content")
                .or_else(|| params.get("diff"))
                .or_else(|| params.get("patch"))
                .and_then(Value::as_str)
                .unwrap_or("");
            codex_native_file_change_paths(params, known_item_paths)
                .into_iter()
                .map(|path| {
                    (
                        "write",
                        json!({
                            "file_path": path,
                            "content": content,
                        }),
                    )
                })
                .collect()
        }
        _ => {
            let command = params
                .get("command")
                .or_else(|| params.get("command_line"))
                .or_else(|| params.get("commandLine"))
                .or_else(|| params.get("cmd"))
                .and_then(|value| {
                    if let Some(text) = value.as_str() {
                        return Some(text.to_owned());
                    }
                    value.as_array().map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                });
            command
                .map(|command| vec![("bash", json!({ "command": command }))])
                .unwrap_or_default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexNativeFirewallDecision {
    Allow,
    RequireApproval { reason: String },
    Block { reason: String },
}

fn codex_native_firewall_decision(
    host: &NativeExecutionHostHandle,
    method: &str,
    params: Option<&Value>,
    workflow_state: Option<&crate::agent::safety::WorkflowStateSnapshot>,
    known_item_paths: Option<&CodexFileChangeItemCache>,
) -> CodexNativeFirewallDecision {
    let arg_sets = codex_native_firewall_arg_sets(method, params, known_item_paths);
    if arg_sets.is_empty() {
        return CodexNativeFirewallDecision::Block {
            reason: match codex_native_policy_tool(method) {
                "codex_file_change" => "Codex file-change approval carried no recoverable paths for the action firewall".to_owned(),
                _ => "Codex command approval carried no recoverable command for the action firewall".to_owned(),
            },
        };
    }
    let fallback_workflow_state = super::workflow_state::WorkflowStateTracker::default().snapshot();
    let workflow_state = workflow_state.unwrap_or(&fallback_workflow_state);
    let mut approval_reason = None;
    for (tool_name, args) in arg_sets {
        let verdict = host.firewall_verdict(tool_name, &args, workflow_state, None, false);
        match verdict {
            NativeFirewallVerdict::Block { reason } => {
                return CodexNativeFirewallDecision::Block { reason };
            }
            NativeFirewallVerdict::RequireApproval { reason } => {
                approval_reason.get_or_insert(reason);
            }
            NativeFirewallVerdict::Allow => {}
        }
    }
    approval_reason.map_or(CodexNativeFirewallDecision::Allow, |reason| {
        CodexNativeFirewallDecision::RequireApproval { reason }
    })
}
#[must_use]
fn codex_native_approval_requires_user(mode: ApprovalMode) -> bool {
    mode != ApprovalMode::Yolo
}

/// Exercise the production Codex effect-policy adapter with a concrete host.
#[cfg(feature = "test-support")]
pub fn codex_native_effect_denial_for_test(
    host: &NativeExecutionHostHandle,
    method: &str,
    params: Option<&Value>,
) -> Option<String> {
    match codex_native_firewall_decision(host, method, params, None, None) {
        CodexNativeFirewallDecision::Allow => None,
        CodexNativeFirewallDecision::Block { reason }
        | CodexNativeFirewallDecision::RequireApproval { reason } => Some(reason),
    }
}

/// Exercise the production serial-tool cache boundary from a composing-crate
/// integration test without exposing the private lifecycle module.
#[cfg(feature = "test-support")]
pub fn invalidate_cache_after_serial_tool_for_test(
    host: &NativeExecutionHostHandle,
    tool_name: &str,
    executed: bool,
) {
    invalidate_cache_after_serial_tool(host, tool_name, executed);
}

/// Exercise the production deferred firewall adapter from a composing-crate
/// integration test with the real host boundary.
#[cfg(feature = "test-support")]
pub fn deferred_firewall_verdict_for_test(
    host: &NativeExecutionHostHandle,
    tool_name: &str,
    args: &Value,
    workflow_snapshot: &super::safety::WorkflowStateSnapshot,
    annotations: Option<&super::native_host::NativeToolAnnotations>,
    is_external_tool: bool,
) -> NativeFirewallVerdict {
    deferred_firewall_verdict(
        host,
        tool_name,
        args,
        workflow_snapshot,
        annotations,
        is_external_tool,
    )
}

/// Build the production deferred policy-rejection event for an integration
/// test without making the private deferred-call context part of the API.
#[cfg(feature = "test-support")]
pub fn deferred_policy_rejection_event_for_test(
    call_id: &str,
    tool_name: &str,
    reason: &str,
) -> FromAgent {
    let call = ToolCallContext {
        call_id: call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        args: Value::Null,
        safe_args: Value::Null,
        extra_context: None,
        pre_hook_args: Value::Null,
        initial_firewall_verdict: NativeFirewallVerdict::Allow,
        approval_inline_env: None,
    };
    deferred_policy_rejection_event(&call, reason, None)
}

/// Run the production deferred PreToolUse helper against a concrete host from
/// a composing-crate integration test.
#[cfg(feature = "test-support")]
pub async fn rerun_deferred_pre_tool_use_for_test(
    host: &NativeExecutionHostHandle,
    call_id: &str,
    tool_name: &str,
    pre_hook_args: &Value,
) -> std::result::Result<(Value, Option<String>), String> {
    let call = ToolCallContext {
        call_id: call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        args: pre_hook_args.clone(),
        safe_args: pre_hook_args.clone(),
        extra_context: None,
        pre_hook_args: pre_hook_args.clone(),
        initial_firewall_verdict: NativeFirewallVerdict::Allow,
        approval_inline_env: None,
    };
    rerun_deferred_pre_tool_use(host, &call).await
}

/// Output allowance for one request under an optional cumulative budget.
///
/// `configured` is the per-request `max_tokens`. With no budget it is used
/// unchanged, which is the interactive case. With a budget the request is also
/// clamped to the unspent part, so a run that calls tools is not granted the
/// full allowance again on every request.
///
/// The floor of 1 keeps the request valid for providers that reject
/// `max_tokens: 0`; ending a run that has spent its budget is the job of the
/// caller that set it.
fn output_token_allowance(configured: u32, budget: Option<u32>, spent: u64) -> u32 {
    let Some(budget) = budget else {
        return configured;
    };
    let unspent = u64::from(budget).saturating_sub(spent);
    let unspent = u32::try_from(unspent).unwrap_or(u32::MAX);
    let allowance = configured.min(unspent);
    if allowance == 0 { 1 } else { allowance }
}

/// Smallest response worth asking a provider for.
///
/// Below this the request is not worth sending: the model cannot finish a tool
/// call in the space left, so the history has to give way instead. Returning
/// `None` at this floor is what turns "the window is full" into a compaction
/// rather than into a provider rejection.
pub const MIN_RESPONSE_RESERVE_TOKENS: u32 = 1_024;

/// The output allowance that fits beside this request's input.
///
/// Providers charge `max_tokens` against the same context window as the input,
/// so a request whose input plus output ceiling exceeds the window is rejected
/// outright however small the response turns out to be. `configured` is
/// therefore an upper bound, not a promise; what fits is the floor of the two.
///
/// Returns `None` when less than `floor` is left, which the caller treats as a
/// request that cannot be sent as composed.
fn clamp_output_to_remaining_context(
    configured: u32,
    context_tokens: u64,
    estimated_input_tokens: u64,
    floor: u32,
) -> Option<u32> {
    let remaining = context_tokens
        .saturating_sub(estimated_input_tokens)
        .saturating_sub(REQUEST_CONTEXT_SAFETY_TOKENS);
    let remaining = u32::try_from(remaining).unwrap_or(u32::MAX);
    (remaining >= floor.max(1)).then(|| configured.min(remaining))
}

/// A request whose input leaves no room for a usable response.
///
/// Raised while the request is being built, before it costs a provider round
/// trip. The turn loop treats it as a context overflow: compaction is the only
/// thing that changes the answer, and the numbers here say by how much.
#[derive(Debug)]
pub(crate) struct RequestExceedsContextWindow {
    pub(crate) estimated_input_tokens: u64,
    pub(crate) context_tokens: u64,
    pub(crate) minimum_response_tokens: u32,
}

impl std::fmt::Display for RequestExceedsContextWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request input is too long for the model context window: {} estimated input tokens leave less than {} tokens for a response inside a {}-token window",
            self.estimated_input_tokens, self.minimum_response_tokens, self.context_tokens
        )
    }
}

impl std::error::Error for RequestExceedsContextWindow {}

async fn recv_command_or_shutdown(
    shutdown_token: &CancellationToken,
    command_rx: &mut mpsc::UnboundedReceiver<AgentCommand>,
) -> Option<AgentCommand> {
    tokio::select! {
        biased;
        () = shutdown_token.cancelled() => None,
        command = command_rx.recv() => command,
    }
}

async fn await_side_question_or_shutdown<F>(
    shutdown_token: &CancellationToken,
    side_question: F,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::select! {
        biased;
        () = shutdown_token.cancelled() => None,
        output = side_question => Some(output),
    }
}

fn history_storage<T>(messages: Vec<Message>) -> T
where
    T: From<Vec<Message>>,
{
    T::from(messages)
}

fn vault_provider_history(
    messages: &[Message],
    credential_vault: &CredentialVault,
) -> Result<Vec<Message>> {
    // Scan the final provider projection as a last boundary check. This also
    // catches raw material introduced by prompts, hooks, or extensions after
    // tool output was first vaulted. Existing references remain opaque.
    let serialized = serde_json::to_value(messages).context("serialize provider history")?;
    let safe = credential_vault.vault_in_json(&serialized);
    serde_json::from_value(safe).context("deserialize vaulted provider history")
}

fn vault_provider_history_shared(
    messages: &Arc<Vec<Message>>,
    credential_vault: &CredentialVault,
) -> Result<Arc<Vec<Message>>> {
    #[cfg(feature = "test-support")]
    PROVIDER_HISTORY_VAULT_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Arc::new(vault_provider_history(
        messages,
        credential_vault,
    )?))
}

#[cfg(feature = "test-support")]
static PROVIDER_HISTORY_VAULT_PASSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Return and reset the number of full history vault passes in this process.
/// This deterministic work counter is only compiled for benchmark builds.
#[cfg(feature = "test-support")]
pub fn take_provider_history_vault_passes_for_bench() -> u64 {
    PROVIDER_HISTORY_VAULT_PASSES.swap(0, std::sync::atomic::Ordering::Relaxed)
}

/// The only direct-provider payload constructed by the native turn loop.
/// Its fields stay private so another path cannot pass an unexamined history
/// or config to a provider by accident.
struct ProviderSafeRequest {
    messages: Arc<Vec<Message>>,
    config: RequestConfig,
    vault_attestation: CredentialAttestation,
}

impl ProviderSafeRequest {
    fn prepare(
        messages: &Arc<Vec<Message>>,
        config: RequestConfig,
        vault: &CredentialVault,
    ) -> Result<Self> {
        let messages = vault_provider_history_shared(messages, vault)?;
        Self::prepare_vaulted(messages, config, vault)
    }

    // The provider loop already vaults its projected history before computing
    // request usage. Keep that projection and recheck it after config
    // preparation, which can discover credentials in system or tool text.
    fn prepare_vaulted(
        messages: Arc<Vec<Message>>,
        mut config: RequestConfig,
        vault: &CredentialVault,
    ) -> Result<Self> {
        config.system = config.system.map(|system| vault.vault_in_text(&system));
        config.tools = vault_provider_tools(config.tools.as_ref(), vault)?;
        // Config preparation may discover a credential present in history.
        let messages = vault_provider_history_shared(&messages, vault)?;
        config.system = config.system.map(|system| vault.vault_in_text(&system));
        let history_json = serde_json::to_value(messages.as_ref())?;
        let attestation = vault
            .attest_provider_json(&history_json)
            .map_err(anyhow::Error::msg)?;
        if let Some(system) = &config.system {
            let system_attestation = vault
                .attest_provider_text(system)
                .map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                attestation == system_attestation,
                "credential vault changed during provider request preparation"
            );
        }
        let tools_json = serde_json::to_value(config.tools.as_ref())?;
        let tools_attestation = vault
            .attest_provider_json(&tools_json)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            attestation == tools_attestation,
            "credential vault changed during provider request preparation"
        );
        Ok(Self {
            messages,
            config,
            vault_attestation: attestation,
        })
    }

    fn ensure_current(&self, vault: &CredentialVault) -> Result<()> {
        anyhow::ensure!(
            vault.has_attestation(self.vault_attestation),
            "credential vault changed after provider request preparation"
        );
        Ok(())
    }
}

fn vault_provider_tools(
    tools: &[Tool],
    credential_vault: &CredentialVault,
) -> Result<Arc<Vec<Tool>>> {
    let mut safe_tools = Vec::with_capacity(tools.len());
    for tool in tools {
        if credential_vault.vault_in_text(&tool.name) != tool.name {
            anyhow::bail!("credential detected in provider tool name");
        }
        let mut safe = tool.clone();
        safe.description = credential_vault.vault_in_text(&tool.description);
        safe.input_schema = vault_provider_schema(&tool.input_schema, credential_vault)?;
        safe_tools.push(safe);
    }
    // A later description can introduce a known value that appeared earlier
    // only as a schema key or tool name. Validate structural names again with
    // the complete vault before handing the definitions to the provider.
    for safe in &mut safe_tools {
        if credential_vault.vault_in_text(&safe.name) != safe.name {
            anyhow::bail!("credential detected in provider tool name");
        }
        safe.description = credential_vault.vault_in_text(&safe.description);
        safe.input_schema = vault_provider_schema(&safe.input_schema, credential_vault)?;
    }
    Ok(Arc::new(safe_tools))
}

fn vault_provider_schema(value: &Value, credential_vault: &CredentialVault) -> Result<Value> {
    match value {
        Value::String(text) => Ok(Value::String(credential_vault.vault_in_text(text))),
        Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(|value| vault_provider_schema(value, credential_vault))
                .collect::<Result<Vec<_>>>()?,
        )),
        Value::Object(entries) => {
            let mut safe = serde_json::Map::new();
            for (key, value) in entries {
                if credential_vault.vault_in_text(key) != *key {
                    anyhow::bail!("credential detected in provider tool schema key");
                }
                safe.insert(key.clone(), vault_provider_schema(value, credential_vault)?);
            }
            Ok(Value::Object(safe))
        }
        _ => Ok(value.clone()),
    }
}

fn opaque_codex_tool_result_for_wire(vaulted_content: &str) -> String {
    vaulted_content.to_owned()
}

fn user_message_text(message: &Message) -> Option<String> {
    match (&message.role, &message.content) {
        (Role::User, MessageContent::Text(text)) => Some(text.clone()),
        (Role::User, MessageContent::Blocks(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn codex_app_server_user_text(
    messages: &[Message],
    injected_notes: &[String],
    current_user_message_index: Option<usize>,
) -> String {
    let current_prompt = current_user_message_index
        .and_then(|index| messages.get(index))
        .and_then(user_message_text);
    let mut parts = injected_notes.to_vec();
    if let Some(current_prompt) = current_prompt {
        parts.push(current_prompt);
    }
    if parts.is_empty() {
        return messages
            .iter()
            .rev()
            .find_map(user_message_text)
            .unwrap_or_default();
    }
    parts.join("\n\n")
}

fn refresh_model_budgets_with_host(
    host: &NativeExecutionHostHandle,
    config: &mut NativeAgentConfig,
    compactor: &mut super::compaction::ContextCompactor,
    model: &str,
) {
    if config.max_tokens_source == MaxTokensSource::Catalog {
        config.max_tokens = host.default_max_output_tokens(model);
    }
    *compactor =
        super::compaction::ContextCompactor::new(super::compaction::CompactionConfig::for_model(
            model,
            config
                .context_window
                .or_else(|| host.model_context_window(model)),
        ));
}

fn process_provider_cost_micros(cost: f64) -> Result<u64> {
    let micros = cost * 1_000_000.0;
    if !micros.is_finite() || micros < 0.0 || micros >= u64::MAX as f64 {
        anyhow::bail!("invalid process provider cost");
    }
    // Gateway integer microcosts pass through one f64 division and multiplication.
    // Normalize only their two-operation rounding envelope; genuine fractions
    // outside that envelope still round up to the next billable micro-unit.
    let nearest = micros.round();
    let roundoff = 2.0 * f64::EPSILON * micros.abs();
    let charged = if (micros - nearest).abs() <= roundoff {
        nearest
    } else {
        micros.ceil()
    };
    Ok(charged as u64)
}

fn set_explicit_max_tokens(config: &mut NativeAgentConfig, max_tokens: u32) {
    config.max_tokens = max_tokens;
    config.max_tokens_source = MaxTokensSource::Explicit;
}

impl NativeAgentRunner {
    async fn admit_provider_request(
        &self,
        kind: &str,
        request_id: &str,
        model: Option<&str>,
    ) -> Result<()> {
        let policy_model =
            model.map(|model| provider_request_policy_model_id(&self.config.model, model));
        if let Some(reason) = policy_model
            .as_deref()
            .and_then(|model| self.tool_executor.model_allowed(model))
        {
            return Err(anyhow::Error::new(ProviderAdmissionDenied {
                kind: kind.to_owned(),
                request_id: request_id.to_owned(),
                reason,
            }));
        }
        let result = self
            .hooks
            .hook_pre_provider_request(kind, request_id, model)
            .await;
        let reason = match result {
            NativeHookResult::Continue => return Ok(()),
            NativeHookResult::Block { reason } => reason,
            NativeHookResult::ModifyInput { .. } => {
                "provider admission hook returned an unsupported input modification".to_owned()
            }
            NativeHookResult::InjectContext { .. } => {
                "provider admission hook returned unsupported context injection".to_owned()
            }
        };
        Err(anyhow::Error::new(ProviderAdmissionDenied {
            kind: kind.to_owned(),
            request_id: request_id.to_owned(),
            reason,
        }))
    }

    fn managed_gateway_receipt_event(
        receipt: maestro_ai::ManagedGatewayReceipt,
        experiment_eligible: bool,
    ) -> FromAgent {
        FromAgent::ManagedGatewayReceipt {
            request_id: receipt.request_id,
            record_id: receipt.record_id,
            lineage_id: receipt.lineage_id,
            record_status: receipt.record_status,
            provider_tools_sha256: if experiment_eligible {
                receipt.provider_tools_sha256
            } else {
                None
            },
            provider_tool_count: if experiment_eligible {
                receipt.provider_tool_count
            } else {
                None
            },
            // Auxiliary compaction uses its own instructions. Keep its cost
            // receipt, but never label it as exposure to the turn's treatment.
            provider_prompt_sha256: if experiment_eligible {
                receipt.provider_prompt_sha256
            } else {
                None
            },
        }
    }

    async fn resolve_managed_request_lineage(
        &mut self,
        explicit_lineage: Option<String>,
    ) -> Result<Option<String>> {
        let Some(client) = self.client.as_ref() else {
            return Ok(explicit_lineage);
        };
        if !client.is_managed_gateway() {
            return Ok(explicit_lineage);
        }
        let Some((organization_id, workspace_id)) =
            client
                .managed_gateway_scope()
                .map(|(organization_id, workspace_id)| {
                    (organization_id.to_owned(), workspace_id.to_owned())
                })
        else {
            bail!("managed gateway request requires complete organization/workspace scope");
        };

        if let Some(lineage) = explicit_lineage {
            return Ok(Some(lineage));
        }

        let thread_id = self
            .hooks
            .hook_session_id()
            .await
            .and_then(|value| {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_owned())
            })
            .context("managed gateway request requires an active session context")?;
        let next_turn = self
            .next_managed_turn_id
            .checked_add(1)
            .context("managed gateway turn sequence exhausted")?;
        self.next_managed_turn_id = next_turn;
        Ok(Some(managed_turn_lineage_id(
            &organization_id,
            &workspace_id,
            &thread_id,
            &self.managed_run_id,
            &format!("turn-{next_turn}"),
        )))
    }

    fn reject_managed_request(&self, error: anyhow::Error) {
        let _ = self.event_tx.send(FromAgent::Error {
            message: format!("Managed request rejected: {error:#}"),
            fatal: false,
            terminal: true,
            retryable: false,
        });
    }

    fn messages_mut(&mut self) -> &mut Vec<Message> {
        Arc::make_mut(&mut self.messages)
    }

    fn set_goal_tools_visible(&mut self, visible: bool) {
        if self.goal_tools_visible == visible {
            return;
        }
        self.goal_tools_visible = visible;
        self.model_tool_cache = None;
        self.refresh_runtime_audit();
    }

    fn set_context_tool_excluded(&mut self, name: &str, excluded: bool) {
        let name = name.to_ascii_lowercase();
        if self.model_route.uses_app_server() || !self.tools.contains_key(&name) {
            let _ = self.event_tx.send(FromAgent::Status {
                message: "Context tool selection requires a registered tool on the native provider route.".into()
            });
            return;
        }
        {
            let mut audit = self
                .runtime_audit
                .write()
                .unwrap_or_else(|p| p.into_inner());
            if excluded {
                audit.excluded_context_tools.insert(name.clone());
            } else {
                audit.excluded_context_tools.remove(&name);
            }
        }
        self.model_tool_cache = None;
        self.refresh_runtime_audit();
        let _ = self.event_tx.send(FromAgent::Status {
            message: format!(
                "{} {name} {} this session's request schemas.",
                if excluded { "Excluded" } else { "Included" },
                if excluded { "from" } else { "in" }
            ),
        });
    }

    fn refresh_runtime_audit(&self) {
        self.refresh_runtime_audit_with_prompt(self.config.system_prompt.clone());
    }

    fn refresh_runtime_audit_with_prompt(&self, system_prompt: Option<String>) {
        let previous = self
            .runtime_audit
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let excluded_context_tools = previous.excluded_context_tools;
        let snapshot = RuntimeAuditSnapshot {
            request_cache: previous.request_cache,
            cache_reuse: None,
            request_context: None,
            excluded_context_tools: excluded_context_tools.clone(),
            prompt_revision: self.runtime_prompt_revision,
            system_prompt,
            tools: effective_tool_definitions(
                &self.tools,
                &self.active_tool_names,
                &self.external_tools,
                self.goal_tools_visible,
                self.include_ide_tools,
            )
            .into_iter()
            .filter(|definition| {
                !excluded_context_tools.contains(&definition.tool.name.to_ascii_lowercase())
            })
            .collect(),
            max_output_tokens: self.config.max_tokens,
            context_window: self
                .config
                .context_window
                .or_else(|| self.tool_executor.model_context_window(&self.config.model)),
        };
        *self
            .runtime_audit
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
    }

    fn replace_governed_tools(
        &mut self,
        allowed_tools: &HashSet<String>,
        external_tool_definitions: Vec<ToolDefinition>,
    ) {
        let mut tools = self
            .tool_executor
            .tool_definitions()
            .into_iter()
            .filter(|definition| allowed_tools.contains(&definition.tool.name.to_ascii_lowercase()))
            .map(|definition| {
                (
                    definition.tool.name.to_ascii_lowercase(),
                    definition.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let external_tools = external_tool_definitions
            .iter()
            .map(|definition| definition.tool.name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        for definition in external_tool_definitions {
            tools.insert(definition.tool.name.to_ascii_lowercase(), definition);
        }
        self.active_tool_names = initial_active_tool_names(
            self.tool_profile,
            &tools,
            &external_tools,
            Some(allowed_tools),
            self.config.external_tool_schema_policy,
        );
        self.explicitly_allowed_tools = allowed_tools.clone();
        self.tools = tools;
        self.external_tools = external_tools;
        self.model_tool_cache = None;
        self.refresh_runtime_audit();
    }

    fn emit_conversation_snapshot(&mut self) {
        self.compact_codex_history_for_boundary();
        let mut processed_queue_ids = self
            .processed_prompt_queue_ids
            .iter()
            .copied()
            .collect::<Vec<_>>();
        processed_queue_ids.sort_unstable();
        if let Some(snapshot) = conversation_snapshot_event_with_queue_ids(
            &self.messages,
            processed_queue_ids,
            self.tool_executor.semantic_conversation_protocol(),
        ) {
            let _ = self.event_tx.send(snapshot);
            self.processed_prompt_queue_ids.clear();
        }
    }

    fn set_active_request_cancel_token(&mut self, token: Option<CancellationToken>) {
        let mut active = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.set_request(token.clone());
        self.cancel_token = token;
    }

    fn set_active_tool_cancel_token(
        &self,
        token: Option<CancellationToken>,
        terminal_drain_required: bool,
    ) {
        let mut active = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.set_tool(token, terminal_drain_required);
    }

    fn set_active_approval_cancel_token(&self, token: Option<CancellationToken>) {
        let mut active = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(token) = token.as_ref() {
            if active.operation_interrupted {
                token.cancel();
            }
        }
        active.approval = token;
    }

    fn set_tool_batch_active(&self, is_active: bool) {
        let mut active = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.tool_batch_active = is_active;
        if !is_active {
            active.terminal_drain_required = false;
        }
    }

    fn take_active_operation_interruption(&self) -> bool {
        let mut active = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut active.operation_interrupted)
    }

    fn finish_tool_batch(&self) -> bool {
        self.active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .finish_tool_batch()
    }

    const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024; // 10MB
    const MAX_TEXT_ATTACHMENT_CHARS: usize = 100_000;

    /// Guarantee every assistant tool call in history has a matching tool result.
    ///
    /// A cancelled turn can leave an assistant `ToolUse` block without a
    /// `ToolResult` (the turn aborted after the assistant message was recorded
    /// but before its results were appended). Providers reject such histories
    /// (OpenAI answers 400), which wedges the session: every subsequent prompt
    /// fails the same way. Harvest any results that arrived late on the
    /// tool-response channel and repair the history before giving up on a
    /// turn and, defensively, before each API call.
    fn repair_orphaned_tool_calls(&mut self) {
        let messages = Arc::make_mut(&mut self.messages);
        repair_orphaned_tool_calls(
            messages,
            &mut self.tool_response_coordinator,
            self.tool_executor.managed_policy_metadata(),
        );
    }

    fn reset_tool_response_state(&mut self) {
        self.tool_response_coordinator.reset();
    }

    fn reject_pending_tool_responses_on_cancel(&mut self) {
        self.tool_response_coordinator.reject_buffered_on_cancel();
    }

    fn drain_leading_pending_messages(
        &mut self,
        kind: PromptKind,
        mode: QueueMode,
    ) -> Vec<PendingMessage> {
        let max_count = match mode {
            QueueMode::All => usize::MAX,
            QueueMode::One => 1,
        };
        self.pending_messages
            .drain_leading_kind_and_lineage(kind, max_count)
    }

    fn dequeue_next_turn_messages(&mut self, allow_follow_ups: bool) -> Vec<PendingMessage> {
        let steering = self.drain_leading_pending_messages(PromptKind::Steer, self.steering_mode);
        if !steering.is_empty() {
            return steering;
        }
        if !allow_follow_ups {
            return Vec::new();
        }
        self.drain_leading_pending_messages(PromptKind::FollowUp, self.follow_up_mode)
    }

    fn announce_next_turn_messages(&self, pending: &[PendingMessage]) {
        let Some(first) = pending.first() else {
            return;
        };
        let remaining = self.pending_messages.len();
        let label = first.kind.label();
        let message = if pending.len() == 1 {
            if remaining > 0 {
                format!(
                    "Processing queued {label} #{} ({} remaining)...",
                    first.id, remaining
                )
            } else {
                format!("Processing queued {label} #{}...", first.id)
            }
        } else if remaining > 0 {
            format!(
                "Processing {} queued {} message(s) ({} remaining)...",
                pending.len(),
                label,
                remaining
            )
        } else {
            format!(
                "Processing {} queued {} message(s)...",
                pending.len(),
                label
            )
        };
        let _ = self.event_tx.send(FromAgent::Status { message });
    }

    fn stop_reason_label(reason: crate::ai::StopReason) -> &'static str {
        match reason {
            crate::ai::StopReason::EndTurn => "end_turn",
            crate::ai::StopReason::MaxTokens => "max_tokens",
            crate::ai::StopReason::StopSequence => "stop_sequence",
            crate::ai::StopReason::ToolUse => "tool_use",
        }
    }

    /// Describe the batch's tool results for the reminder engine.
    fn tool_outcomes_for_batch(&self, results: &[ContentBlock]) -> Vec<ReminderToolOutcome> {
        tool_outcomes_for_batch(&self.tool_executor, self.messages.last(), results)
    }

    /// Refuse a whole tool batch because the turn's step budget is spent.
    ///
    /// Returns the refused tool names in call order.
    ///
    /// Each refused call gets a model-visible `ToolResult` in the same shape
    /// an executed call produces, which keeps every `tool_use` block paired
    /// with a `tool_result` and states the refusal instead of dropping it.
    /// Dropping a call would violate the provider transcript's tool-use/result
    /// pairing and leave its state ambiguous, allowing a later response to
    /// describe unexecuted work as queued or complete. An explicit refusal
    /// result preserves the pairing and makes non-execution authoritative.
    ///
    /// No `ToolCall` event is emitted for a refused call. That event is a
    /// request to execute: callers that own execution (`print_mode`, the
    /// headless server) act on it, and they must not run a call this turn
    /// just refused.
    fn refuse_tool_batch_over_step_budget(
        &mut self,
        calls: Vec<(String, String, Value, Option<String>)>,
        max_steps: usize,
    ) -> Vec<String> {
        let mut tool_results: Vec<ContentBlock> = Vec::new();
        let mut refused_tools: Vec<String> = Vec::new();
        for (call_id, tool_name, _args, _parse_error) in calls {
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: call_id,
                content: format!(
                    "not_executed: this turn reached its step budget of {max_steps} model \
                     responses, so `{tool_name}` was refused and did not run. Tell the user \
                     what is still outstanding; do not report this call as done, queued, or \
                     running."
                ),
                is_error: Some(true),
            });
            refused_tools.push(tool_name);
        }
        self.messages_mut().push(Message {
            role: Role::User,
            content: MessageContent::Blocks(tool_results),
        });
        self.tool_executor.report_diagnostic(format!(
            "[agent] turn step budget exhausted after {max_steps} model responses; refused {} tool call(s): {}",
            refused_tools.len(),
            refused_tools.join(", "),
        ));
        refused_tools
    }

    fn retire_process_budget(&mut self, system_prompt: String) -> Result<()> {
        if self.busy || !self.pending_messages.is_empty() || !self.deferred_commands.is_empty() {
            anyhow::bail!("cannot retire process budget during an active or queued turn");
        }
        self.process_budget = None;
        self.config.system_prompt = Some(system_prompt);
        self.system_prompt_revision = self.system_prompt_revision.saturating_add(1);
        self.runtime_prompt_revision = self.runtime_prompt_revision.saturating_add(1);
        self.refresh_runtime_audit();
        Ok(())
    }

    fn apply_process_budget(
        &mut self,
        limits: super::process_budget::ProcessBudgetLimits,
        checkpoint: Option<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>>,
    ) -> Result<Arc<std::sync::Mutex<super::process_budget::ProcessBudgetState>>> {
        if self.model_route.uses_app_server() {
            anyhow::bail!("process budgets require managed native inference");
        }
        let state = self.process_budget.clone().or(checkpoint);
        let state = match state {
            Some(state) => {
                state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                    .install(&limits)
                    .map_err(anyhow::Error::msg)?;
                state
            }
            None => Arc::new(std::sync::Mutex::new(
                super::process_budget::ProcessBudgetState::new(limits)
                    .map_err(anyhow::Error::msg)?,
            )),
        };
        self.process_budget = Some(Arc::clone(&state));
        Ok(state)
    }

    fn refuse_tool_batch(
        &mut self,
        calls: Vec<(String, String, Value, Option<String>)>,
        reason: &str,
    ) {
        let results = calls
            .into_iter()
            .map(|(id, tool, _, _)| ContentBlock::ToolResult {
                tool_use_id: id,
                content: format!(
                    "not_executed: {tool} was refused because {reason}; it did not run."
                ),
                is_error: Some(true),
            })
            .collect::<Vec<_>>();
        if !results.is_empty() {
            self.messages_mut().push(Message {
                role: Role::User,
                content: MessageContent::Blocks(results),
            });
        }
    }
}

fn append_codex_tool_use(
    messages: &mut Vec<Message>,
    call_id: &str,
    tool_name: &str,
    input: Value,
) {
    messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: call_id.to_owned(),
            name: tool_name.to_owned(),
            input,
            gemini_context: None,
        }]),
    });
}

fn append_codex_tool_result(
    messages: &mut Vec<Message>,
    call_id: &str,
    content: String,
    is_error: bool,
) {
    messages.push(Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: call_id.to_owned(),
            content,
            is_error: Some(is_error),
        }]),
    });
}

#[cfg(test)]
fn conversation_snapshot_event(messages: &[Message]) -> Option<FromAgent> {
    conversation_snapshot_event_with_queue_ids(
        messages,
        Vec::new(),
        "evalops.maestro.semantic-conversation.v1",
    )
}

fn conversation_snapshot_event_with_queue_ids(
    messages: &[Message],
    processed_queue_ids: Vec<u64>,
    protocol_version: &str,
) -> Option<FromAgent> {
    let serialized = serde_json::to_value(sanitize_semantic_conversation(messages)).ok()?;
    let messages = serde_json::from_value(redact_semantic_snapshot_json(
        crate::agent::credential_store::redact_credentials_in_json(&serialized),
    ))
    .ok()?;
    Some(FromAgent::ConversationSnapshot {
        protocol_version: protocol_version.to_owned(),
        messages,
        processed_queue_ids,
    })
}

/// Private semantic checkpoints may retain provider-visible structure, but not
/// hidden reasoning or arbitrary tool output. Keep the tool IDs so restored
/// histories preserve the call/result relationship while replacing the output
/// body with a bounded marker.
fn sanitize_semantic_conversation(messages: &[Message]) -> Vec<Message> {
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
                            content: "[tool result omitted from checkpoint]".to_string(),
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

fn redact_semantic_snapshot_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(redact_semantic_snapshot_json)
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
                            redact_semantic_snapshot_json(value)
                        },
                    )
                })
                .collect(),
        ),
        value => value,
    }
}

/// Extract only typed successful results; proposals and prose are not evidence.
fn successful_file_operation(
    call_id: &str,
    execution: &super::protocol::ExecutionReceipt,
) -> Option<super::compaction::ContinuationFileOperation> {
    use super::compaction::{ContinuationFileOperation, ContinuationFileOperationKind as Kind};
    if execution.call_id != call_id
        || execution.status != super::protocol::ExecutionStatus::Succeeded
    {
        return None;
    }
    let super::protocol::ToolReceiptDetails::BuiltIn(details) = &execution.details else {
        return None;
    };
    let (path, kind) = match details {
        crate::ToolDetails::Read(details) => (&details.path, Kind::Read),
        crate::ToolDetails::Write(details) => (&details.path, Kind::Write),
        crate::ToolDetails::Edit(details) => (&details.path, Kind::Edit),
        _ => return None,
    };
    if path.is_empty() {
        return None;
    }
    Some(ContinuationFileOperation {
        tool_call_id: call_id.to_owned(),
        path: path.clone(),
        kind,
    })
}

#[cfg(test)]
#[path = "native/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "native/request_perf_tests.rs"]
mod request_perf_tests;
