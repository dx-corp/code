//! Native Maestro turn runtime.
//!
//! The actor and turn algorithm live in this crate.  Application crates only
//! compose a resolved provider client, session context, and the private native
//! execution adapter, then project the resulting [`FromAgent`] events into
//! their own transport or UI.

pub mod codex_app_server_turns;
mod codex_selective_summary;
pub mod compaction;
pub mod credential_store;
pub mod denial_memory;
pub mod extensions;
mod managed_authorization;
pub mod message_queue;
pub mod model_dynamics;
mod native;
pub use managed_authorization::ManagedAuthorizationCoordinator;
pub mod native_host;
pub mod process_budget;
pub mod protocol;
pub mod reminders;
pub mod retry;
pub mod safety;
pub mod selective_summary;
pub mod session_scope;
pub mod steer_signal;
pub mod text_loop;
pub mod turn_budget;
pub mod workflow_state;

pub use maestro_context::RequestContextUsage;
pub use maestro_context::TokenCounter;
pub use maestro_context::token_counting;
pub use maestro_context::token_estimation;

pub use codex_app_server_turns::{
    CodexAppServerTurnResult, CodexAppServerTurnSession, DynamicToolSpec, TurnWaitEvent,
    approval_decision, codex_thread_model_id, dynamic_tools_from_native,
    model_should_use_app_server_turns, parse_tool_call_params, tool_call_error_result,
    tool_call_success_result,
};
pub use compaction::{CompactionConfig, CompactionResult, ContextCompactor, CutPoint};
pub use credential_store::{CredentialStats, CredentialStore, CredentialType, CredentialVault};
pub use denial_memory::{DenialMemory, MAX_DENIAL_TARGET_CHARS};
pub use extensions::{
    AgentExtension, BatchEndContext, DoomLoopExtension, ExtensionRegistry, ExtensionStats,
    ExtensionVerdict, ToolCallContext as ExtensionToolCallContext, ToolResultContext,
    ToolResultPayload, TurnEndContext, TurnStartContext,
};
pub use message_queue::{
    MAX_PENDING_MESSAGES, MessageQueue, PendingMessage, PromptKind, QueuePlacement, QueueStats,
};
pub use model_dynamics::{
    BoostStatus, ModelChoice, ModelDynamicsConfig, TaskDifficulty, ThinkingLevel,
};
pub use native::{
    ExternalToolSchemaPolicy, MaxTokensSource, NativeAgent, NativeAgentConfig,
    REQUEST_CONTEXT_SAFETY_TOKENS, RuntimeAuditSnapshot, ToolResponseConsumption,
    ToolResponseMessage,
};
pub use native::{managed_turn_lineage_id, runtime_system_prompt};
pub use native_host::{
    ApprovalMode, NativeCodexAuth, NativeCodingCompletion, NativeContextEffect,
    NativeExecutionHost, NativeExecutionHostHandle, NativeFirewallVerdict, NativeHookEvent,
    NativeHookResult, NativeHostFuture, NativeModelCapabilities, NativeModelRoute,
    NativeReadOnlyToolCall, NativeResolvedClient, NativeToolAnnotations,
    NativeToolExecutionOptions, NativeToolOperationAdmission, QueueMode, ToolDefinition,
};
pub use protocol::{
    DenialReason, ExecutionPhase, ExecutionReceipt, ExecutionSource, ExecutionStatus, FromAgent,
    InlineToolApprovalContext, ManagedInferenceAuthorization, ManagedPolicyMetadata, ToAgent,
    TokenUsage, ToolError, ToolExecution, ToolOutcome, ToolOutput, ToolReceiptDetails, ToolResult,
    UNTRUSTED_CONTENT_POLICY, ensure_untrusted_content_policy,
};
pub use reminders::{REMINDER_CLOSE, REMINDER_OPEN, Reminder, ReminderContext, ReminderEngine};
pub use retry::{ErrorKind, RetryConfig, RetryDecision, RetryPolicy};
pub use safety::{
    SafetyConfig, SafetyController, SafetyVerdict, is_retryable_error, stable_stringify,
};
pub use selective_summary::{
    RangeSelection, SelectiveSummaryOutcome, SelectiveSummaryPreview, SelectiveSummaryRequest,
    SelectiveSummaryResult, SummaryTurn,
};
pub use session_scope::{ParentScopeId, SessionId, parent_scope_for_session};
pub use steer_signal::SteerSignal;
pub use text_loop::{LoopKind, TextLoopDetector, loop_reminder_message};
pub use turn_budget::{DEFAULT_MAX_TURN_STEPS, TurnOutcome, TurnStepBudget};
pub use workflow_state::{
    ToolEgress, ToolTag, WorkflowStateSnapshot, WorkflowStateTracker, apply_workflow_state_hooks,
    has_tool_tags, is_human_facing_tool, is_workflow_tracked_tool, looks_like_egress,
};

#[cfg(feature = "test-support")]
pub use native::{
    codex_native_effect_denial_for_test, deferred_firewall_verdict_for_test,
    deferred_policy_rejection_event_for_test, invalidate_cache_after_serial_tool_for_test,
    rerun_deferred_pre_tool_use_for_test,
};
