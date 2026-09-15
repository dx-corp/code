//! The concrete host boundary used by the native actor.
//!
//! The native loop owns turn orchestration, approvals, cancellation, retries,
//! and result/receipt ordering.  Tool dispatch and integrated hooks remain in
//! the composing host because their implementation closure includes the local
//! registry, MCP, sandbox, mailbox, coding-task, and subagent stacks.  This
//! module is the deliberately small, native-specific seam between those two
//! owners.
//!
//! This is not a general tool port.  The methods below mirror the existing
//! `ToolExecutor`/`IntegratedHookSystem` operations used by the native loop;
//! they do not introduce a second invocation or receipt model.  The TUI host
//! adapts the concrete executor and hook system to this seam, and the runtime
//! calls each operation exactly once.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::protocol::{FromAgent, InlineToolApprovalContext, ManagedPolicyMetadata, ToolExecution};
use super::safety::WorkflowStateSnapshot;
use super::steer_signal::SteerSignal;
use maestro_ai::Tool;
use maestro_ai::UnifiedClient;

/// Approval policy consumed by the native loop.  The composing TUI maps its
/// existing state selector to this runtime value at construction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    Yolo,
    #[default]
    Selective,
    Safe,
}

/// Queue drain policy for prompts received while a turn is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    #[default]
    All,
    One,
}

/// Boxed future used by asynchronous operations in the host seam.
pub type NativeHostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Catalog-reported model support, supplied by the composing host without
/// coupling the runtime to a catalog implementation. None means unknown, not
/// unsupported. These facts do not grant tool access or model-change authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct NativeModelCapabilities {
    pub vision: Option<bool>,
    pub tool_calling: Option<bool>,
    pub reasoning: Option<bool>,
    pub context_tokens: Option<u64>,
    pub output_tokens: Option<u32>,
}

/// Provider transport selected by the composing host after authentication and
/// model policy have been resolved.  The runtime only needs to know whether a
/// turn is served by the direct provider client or by the existing Codex
/// app-server transport; it never resolves credentials or routes itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeModelRoute {
    DirectProvider,
    CodexAppServer { model_id: String },
}

impl NativeModelRoute {
    #[must_use]
    pub fn uses_app_server(&self) -> bool {
        matches!(self, Self::CodexAppServer { .. })
    }
}

/// All provider/auth work is completed by the host before the native actor is
/// spawned.  An app-server route intentionally carries no HTTP client because
/// Codex owns that authenticated transport; direct routes carry the already
/// resolved `UnifiedClient`.
pub struct NativeResolvedClient {
    pub client: Option<UnifiedClient>,
    pub provider_name: String,
    pub model_route: NativeModelRoute,
}

/// Authenticated Codex app-server launch context resolved by the composing
/// host.  The runtime carries the opaque child environment and persistence
/// roots to the existing transport, but never reads credential files or
/// selects a profile itself.
#[derive(Debug, Clone)]
pub struct NativeCodexAuth {
    pub profile_name: String,
    pub child_env: HashMap<String, String>,
    pub auth_path: PathBuf,
    pub state_root: PathBuf,
}

/// Tool metadata sent to the provider and used by the loop's model-facing
/// allowlist.  The concrete registry stays with the host.
#[derive(Clone)]
pub struct ToolDefinition {
    pub tool: Tool,
    pub requires_approval: bool,
}

/// The annotations the concrete MCP/inline implementation resolved for a
/// tool.  The runtime only needs these safety hints for the native read-only
/// classifier and action-firewall call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// Model projection and optional file created by the existing session output owner.
pub struct NativeToolOutput {
    pub content: String,
    pub saved_path: Option<PathBuf>,
}

/// Result of the host-owned action firewall.  The runtime owns how this result
/// participates in the approval state machine; the host owns the policy that
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeFirewallVerdict {
    Allow,
    RequireApproval { reason: String },
    Block { reason: String },
}

impl NativeFirewallVerdict {
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Block { .. })
    }

    #[must_use]
    pub fn requires_approval(&self) -> bool {
        matches!(self, Self::RequireApproval { .. })
    }
}

/// Hook event names used by the runtime's context-rendering helper.  The TUI
/// adapter maps these to its existing `HookEventType` without exposing that
/// implementation type to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeHookEvent {
    PreToolUse,
    PostToolUse,
    EvalGate,
    UserPromptSubmit,
    PreMessage,
    PostMessage,
    OnError,
    StopFailure,
    PermissionRequest,
    SessionStart,
    SessionEnd,
    Overflow,
}

/// Result returned by one host hook invocation.
#[derive(Debug, Clone, Default)]
pub enum NativeHookResult {
    #[default]
    Continue,
    Block {
        reason: String,
    },
    ModifyInput {
        new_input: Value,
    },
    InjectContext {
        context: String,
    },
}

/// A read-only call passed to the host's existing `BatchExecutor` path.
#[derive(Debug, Clone)]
pub struct NativeReadOnlyToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub args: Value,
}

/// Options for the existing cancellable serial executor path.
pub struct NativeToolExecutionOptions<'a> {
    pub cancel: CancellationToken,
    pub approved_inline_env: Option<&'a HashMap<String, String>>,
}

/// The one native-specific execution boundary.  Implement this around the
/// existing concrete `ToolExecutor` and `IntegratedHookSystem` in the host
/// crate.  In particular, `execute_tool` must call the current receipt-aware
/// executor with its mutable hook state; it must not invoke a second dispatcher.
pub trait NativeExecutionHost: Send + Sync {
    /// Local consent owner supplies a durably enrolled assignment. Hosted/default hosts abstain.
    fn experiment_assignment(
        &self,
        _model: &str,
    ) -> Option<maestro_runtime_contracts::experiments::ExperimentAssignment> {
        None
    }

    // Immutable registry/metadata surface.
    fn tool_definitions(&self) -> Vec<ToolDefinition>;
    fn has_native_tool(&self, name: &str) -> bool;
    fn is_reserved_tool(&self, name: &str) -> bool;
    fn goal_tools_visible(&self) -> bool;
    fn include_ide_tools(&self) -> bool;
    fn missing_required(&self, name: &str, args: &Value) -> Vec<String>;
    fn has_code_authority(&self) -> bool;
    fn requires_sandbox_bypass_approval(&self, name: &str, args: &Value) -> bool;
    fn mcp_permission_allows(&self, name: &str) -> bool;
    fn requires_approval(&self, name: &str, args: &Value) -> bool;
    fn is_mcp_tool(&self, name: &str) -> bool;
    fn tool_annotations(&self, name: &str) -> Option<NativeToolAnnotations>;
    fn ensure_mcp_annotations<'a>(&'a self) -> NativeHostFuture<'a, Result<(), String>>;
    fn inline_tool_approval_context(&self, name: &str) -> Option<InlineToolApprovalContext>;
    fn is_explicit_inline_read_only_tool(&self, name: &str) -> bool;
    fn credential_generation(&self) -> u64;
    fn file_read_verdict(&self, path: &str) -> NativeFirewallVerdict;
    fn video_mime(&self, path: &Path) -> Option<(String, u64)>;
    fn extract_video_frames<'a>(
        &'a self,
        path: &'a Path,
    ) -> NativeHostFuture<'a, Result<Vec<String>, String>>;

    /// Return the verified managed-policy identity owned by this host.
    ///
    /// The transport-neutral runtime must never read policy files or process
    /// environment directly: a composing host is the authority for policy and
    /// tenant scope. Hosts without managed policy return `None`.
    fn managed_policy_metadata(&self) -> Option<ManagedPolicyMetadata> {
        None
    }

    // Policy and execution.  The returned `ToolExecution` is the existing
    // receipt-bearing value; the event sender is the same loop-owned channel.
    fn firewall_verdict(
        &self,
        name: &str,
        args: &Value,
        workflow_state: &WorkflowStateSnapshot,
        annotations: Option<&NativeToolAnnotations>,
        external: bool,
    ) -> NativeFirewallVerdict;
    fn execute_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        event_tx: Option<&'a mpsc::UnboundedSender<FromAgent>>,
        call_id: &'a str,
        options: NativeToolExecutionOptions<'a>,
    ) -> NativeHostFuture<'a, ToolExecution>;
    fn execute_read_only_wave<'a>(
        &'a self,
        calls: &'a [NativeReadOnlyToolCall],
        event_tx: &'a mpsc::UnboundedSender<FromAgent>,
        cancel: Option<CancellationToken>,
    ) -> NativeHostFuture<'a, HashMap<String, ToolExecution>>;
    fn clear_cache(&self);
    fn set_steer_signal(&self, signal: Arc<SteerSignal>);
    fn reset_coding_turn(&self);
    fn set_subagent_parent_scope(&self, scope: String);
    fn set_subagent_parent_model(&self, model: String, thinking: String);
    fn set_subagent_parent_requests(&self, requests: Vec<String>);
    fn coding_completion(&self) -> Result<Option<NativeCodingCompletion>, String>;
    fn shutdown_background_processes<'a>(&'a self) -> NativeHostFuture<'a, ()>;

    // Integrated hook lifecycle. The futures wait for the host's serialized
    // hook state, preserving ordering, timeout, and Session History behavior
    // when an executor already owns the mutex.
    fn hook_pre_tool_use<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_post_tool_use<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        output: &'a str,
        is_error: bool,
        duration_ms: u64,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_eval_gate<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        output: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_user_prompt_submit<'a>(
        &'a self,
        prompt: &'a str,
        attachment_count: u32,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_pre_message<'a>(
        &'a self,
        message: &'a str,
        attachments: &'a [String],
        model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    /// Admit one logical direct-provider request before opening its stream.
    ///
    /// The default keeps local/TUI hosts unchanged. Managed hosts may use the
    /// stable request identity to fence a provider call against their worker
    /// lease and event ledger; every lower-level transport retry belongs to
    /// the already-admitted stream and must not call this hook again.
    fn hook_pre_provider_request<'a>(
        &'a self,
        _kind: &'a str,
        _request_id: &'a str,
        _model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { NativeHookResult::Continue })
    }
    fn hook_post_message<'a>(
        &'a self,
        response: &'a str,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        stop_reason: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_on_error<'a>(
        &'a self,
        error: &'a str,
        error_kind: &'a str,
        context: Option<&'a str>,
        recoverable: bool,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_stop_failure<'a>(
        &'a self,
        error: &'a str,
        error_details: Option<&'a str>,
        last_assistant_message: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_permission_request<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_handle_overflow<'a>(&'a self) -> NativeHostFuture<'a, bool>;
    fn hook_checkpoint_transcript_before_response<'a>(&'a self) -> NativeHostFuture<'a, ()>;
    fn hook_session_id<'a>(&'a self) -> NativeHostFuture<'a, Option<String>>;
    fn hook_set_session_context<'a>(
        &'a self,
        session_id: Option<String>,
        transcript_path: Option<String>,
    ) -> NativeHostFuture<'a, ()>;
    fn hook_on_session_start<'a>(
        &'a self,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_on_session_end<'a>(&'a self, reason: &'a str)
    -> NativeHostFuture<'a, NativeHookResult>;
    fn hook_increment_turn<'a>(&'a self) -> NativeHostFuture<'a, ()>;
    fn hook_set_model<'a>(&'a self, model: &'a str) -> NativeHostFuture<'a, ()>;
    fn hook_set_log_file<'a>(&'a self, path: Option<String>) -> NativeHostFuture<'a, ()>;
    fn render_hook_context(&self, event: NativeHookEvent, context: &str) -> Result<String, String>;

    // Host-resolved support callbacks.  These keep auth/config/catalog and
    // output persistence out of the loop crate while preserving their exact
    // current behavior.
    fn model_allowed(&self, model_id: &str) -> Option<String>;
    fn resolve_model(&self, model_id: &str) -> Result<NativeResolvedClient, String>;
    /// Resolve a model for automatic task-scoped routing.  Hosts may apply
    /// the active tenant scope check here while keeping explicit `/model`
    /// changes on the existing resolver path.
    fn resolve_model_for_automatic_transition(
        &self,
        model_id: &str,
    ) -> Result<NativeResolvedClient, String> {
        self.resolve_model(model_id)
    }
    fn default_max_output_tokens(&self, model: &str) -> u32;
    fn is_local_model(&self, model: &str) -> bool;
    fn model_context_window(&self, model: &str) -> Option<u64>;
    /// Report only known model metadata. Hosts without a richer catalog retain
    /// their existing context-window fact and leave other support unknown.
    fn model_capabilities(&self, model: &str) -> NativeModelCapabilities {
        NativeModelCapabilities {
            context_tokens: self
                .model_context_window(model)
                .filter(|tokens| *tokens > 0),
            ..NativeModelCapabilities::default()
        }
    }

    fn validate_model_transition(&self, from: &str, to: &str) -> Result<(), String>;
    fn boost_choice(
        &self,
        current: &super::model_dynamics::ModelChoice,
        config: &super::model_dynamics::ModelDynamicsConfig,
    ) -> Option<super::model_dynamics::ModelChoice>;
    fn normalize_thinking(
        &self,
        model: &str,
        requested: super::model_dynamics::ThinkingLevel,
    ) -> super::model_dynamics::ThinkingLevel;
    fn codex_auth_context(&self) -> Result<NativeCodexAuth, String>;
    fn codex_auth_is_usable(&self, path: &Path) -> bool;
    fn report_diagnostic(&self, message: String);
    fn clamp_tool_output(
        &self,
        content: &str,
        tool_name: &str,
        spill_dir: Option<&std::path::Path>,
    ) -> String;
    fn project_tool_output(
        &self,
        content: &str,
        tool_name: &str,
        spill_dir: Option<&Path>,
    ) -> NativeToolOutput {
        NativeToolOutput {
            content: self.clamp_tool_output(content, tool_name, spill_dir),
            saved_path: None,
        }
    }
    fn model_tool_spill_dir(&self, cwd: &str, session_id: &str) -> PathBuf;
    fn open_todo_count(&self, output: &str) -> Option<usize>;
    fn semantic_conversation_protocol(&self) -> &str;

    /// Resolve the transport route from the host's already-admitted model
    /// catalog/auth state.  This is a pure callback during a model switch; it
    /// must not mint a new grant or widen the active authority.
    fn model_route(&self, model_id: &str) -> NativeModelRoute;
}

/// Shared handle kept by the runner.  The wrapper exposes the native-loop
/// method names while keeping the concrete host implementation private to the
/// composing crate.
#[derive(Clone)]
pub struct NativeExecutionHostHandle(Arc<dyn NativeExecutionHost>);

impl std::fmt::Debug for NativeExecutionHostHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NativeExecutionHostHandle(..)")
    }
}

impl NativeExecutionHostHandle {
    pub fn experiment_assignment(
        &self,
        model: &str,
    ) -> Option<maestro_runtime_contracts::experiments::ExperimentAssignment> {
        self.0.experiment_assignment(model)
    }

    #[must_use]
    pub fn new(host: Arc<dyn NativeExecutionHost>) -> Self {
        Self(host)
    }

    #[must_use]
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.0.tool_definitions()
    }

    #[must_use]
    pub fn has_native_tool(&self, name: &str) -> bool {
        self.0.has_native_tool(name)
    }

    #[must_use]
    pub fn goal_tools_visible(&self) -> bool {
        self.0.goal_tools_visible()
    }

    #[must_use]
    pub fn include_ide_tools(&self) -> bool {
        self.0.include_ide_tools()
    }

    #[must_use]
    pub fn is_reserved_tool(&self, name: &str) -> bool {
        self.0.is_reserved_tool(name)
    }

    #[must_use]
    pub fn missing_required(&self, name: &str, args: &Value) -> Vec<String> {
        self.0.missing_required(name, args)
    }

    #[must_use]
    pub fn has_code_authority(&self) -> bool {
        self.0.has_code_authority()
    }

    #[must_use]
    pub fn requires_sandbox_bypass_approval(&self, name: &str, args: &Value) -> bool {
        self.0.requires_sandbox_bypass_approval(name, args)
    }

    #[must_use]
    pub fn mcp_permission_allows(&self, name: &str) -> bool {
        self.0.mcp_permission_allows(name)
    }

    #[must_use]
    pub fn requires_approval(&self, name: &str, args: &Value) -> bool {
        self.0.requires_approval(name, args)
    }

    #[must_use]
    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.0.is_mcp_tool(name)
    }

    #[must_use]
    pub fn tool_annotations(&self, name: &str) -> Option<NativeToolAnnotations> {
        self.0.tool_annotations(name)
    }

    pub fn ensure_mcp_annotations<'a>(&'a self) -> NativeHostFuture<'a, Result<(), String>> {
        self.0.ensure_mcp_annotations()
    }

    #[must_use]
    pub fn inline_tool_approval_context(&self, name: &str) -> Option<InlineToolApprovalContext> {
        self.0.inline_tool_approval_context(name)
    }

    #[must_use]
    pub fn is_explicit_inline_read_only_tool(&self, name: &str) -> bool {
        self.0.is_explicit_inline_read_only_tool(name)
    }

    #[must_use]
    pub fn credential_generation(&self) -> u64 {
        self.0.credential_generation()
    }

    #[must_use]
    pub fn file_read_verdict(&self, path: &str) -> NativeFirewallVerdict {
        self.0.file_read_verdict(path)
    }

    #[must_use]
    pub fn video_mime(&self, path: &Path) -> Option<(String, u64)> {
        self.0.video_mime(path)
    }

    pub fn extract_video_frames<'a>(
        &'a self,
        path: &'a Path,
    ) -> NativeHostFuture<'a, Result<Vec<String>, String>> {
        self.0.extract_video_frames(path)
    }

    #[must_use]
    pub fn firewall_verdict(
        &self,
        name: &str,
        args: &Value,
        workflow_state: &WorkflowStateSnapshot,
        annotations: Option<&NativeToolAnnotations>,
        external: bool,
    ) -> NativeFirewallVerdict {
        self.0
            .firewall_verdict(name, args, workflow_state, annotations, external)
    }

    pub fn execute_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        event_tx: Option<&'a mpsc::UnboundedSender<FromAgent>>,
        call_id: &'a str,
        options: NativeToolExecutionOptions<'a>,
    ) -> NativeHostFuture<'a, ToolExecution> {
        self.0.execute_tool(name, args, event_tx, call_id, options)
    }

    /// Enrich a host-produced execution with the host's verified policy
    /// identity while preserving an already-attached receipt policy.
    #[must_use]
    pub fn with_managed_policy(&self, execution: ToolExecution) -> ToolExecution {
        execution.with_managed_policy(self.0.managed_policy_metadata())
    }

    #[must_use]
    pub fn managed_policy_metadata(&self) -> Option<ManagedPolicyMetadata> {
        self.0.managed_policy_metadata()
    }

    pub fn execute_read_only_wave<'a>(
        &'a self,
        calls: &'a [NativeReadOnlyToolCall],
        event_tx: &'a mpsc::UnboundedSender<FromAgent>,
        cancel: Option<CancellationToken>,
    ) -> NativeHostFuture<'a, HashMap<String, ToolExecution>> {
        self.0.execute_read_only_wave(calls, event_tx, cancel)
    }

    pub fn clear_cache(&self) {
        self.0.clear_cache();
    }

    pub fn set_steer_signal(&self, signal: Arc<SteerSignal>) {
        self.0.set_steer_signal(signal);
    }

    pub fn reset_coding_turn(&self) {
        self.0.reset_coding_turn();
    }

    pub fn set_subagent_parent_scope(&self, scope: String) {
        self.0.set_subagent_parent_scope(scope);
    }

    pub fn set_subagent_parent_model(&self, model: String, thinking: String) {
        self.0.set_subagent_parent_model(model, thinking);
    }

    pub fn set_subagent_parent_requests(&self, requests: Vec<String>) {
        self.0.set_subagent_parent_requests(requests);
    }

    pub fn coding_completion(&self) -> Result<Option<NativeCodingCompletion>, String> {
        self.0.coding_completion()
    }

    pub fn shutdown_background_processes<'a>(&'a self) -> NativeHostFuture<'a, ()> {
        self.0.shutdown_background_processes()
    }

    pub fn hook_pre_tool_use<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_pre_tool_use(name, call_id, args)
    }

    pub fn hook_post_tool_use<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        output: &'a str,
        is_error: bool,
        duration_ms: u64,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0
            .hook_post_tool_use(name, call_id, args, output, is_error, duration_ms)
    }

    pub fn hook_eval_gate<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        output: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_eval_gate(name, call_id, args, output)
    }

    pub fn hook_user_prompt_submit<'a>(
        &'a self,
        prompt: &'a str,
        attachment_count: u32,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_user_prompt_submit(prompt, attachment_count)
    }

    pub fn hook_pre_message<'a>(
        &'a self,
        message: &'a str,
        attachments: &'a [String],
        model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_pre_message(message, attachments, model)
    }

    pub fn hook_pre_provider_request<'a>(
        &'a self,
        kind: &'a str,
        request_id: &'a str,
        model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_pre_provider_request(kind, request_id, model)
    }

    pub fn hook_post_message<'a>(
        &'a self,
        response: &'a str,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        stop_reason: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_post_message(
            response,
            input_tokens,
            output_tokens,
            duration_ms,
            stop_reason,
        )
    }

    pub fn hook_on_error<'a>(
        &'a self,
        error: &'a str,
        error_kind: &'a str,
        context: Option<&'a str>,
        recoverable: bool,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0
            .hook_on_error(error, error_kind, context, recoverable)
    }

    pub fn hook_stop_failure<'a>(
        &'a self,
        error: &'a str,
        error_details: Option<&'a str>,
        last_assistant_message: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0
            .hook_stop_failure(error, error_details, last_assistant_message)
    }

    pub fn hook_permission_request<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_permission_request(name, call_id, args, reason)
    }

    #[must_use]
    pub fn hook_handle_overflow<'a>(&'a self) -> NativeHostFuture<'a, bool> {
        self.0.hook_handle_overflow()
    }

    pub fn hook_checkpoint_transcript_before_response<'a>(&'a self) -> NativeHostFuture<'a, ()> {
        self.0.hook_checkpoint_transcript_before_response()
    }

    #[must_use]
    pub fn hook_session_id<'a>(&'a self) -> NativeHostFuture<'a, Option<String>> {
        self.0.hook_session_id()
    }

    pub fn hook_set_session_context<'a>(
        &'a self,
        session_id: Option<String>,
        transcript_path: Option<String>,
    ) -> NativeHostFuture<'a, ()> {
        self.0.hook_set_session_context(session_id, transcript_path)
    }

    pub fn hook_on_session_start<'a>(
        &'a self,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_on_session_start(reason)
    }

    pub fn hook_on_session_end<'a>(
        &'a self,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.0.hook_on_session_end(reason)
    }

    pub fn hook_increment_turn<'a>(&'a self) -> NativeHostFuture<'a, ()> {
        self.0.hook_increment_turn()
    }

    pub fn hook_set_model<'a>(&'a self, model: &'a str) -> NativeHostFuture<'a, ()> {
        self.0.hook_set_model(model)
    }

    pub fn hook_set_log_file<'a>(&'a self, path: Option<String>) -> NativeHostFuture<'a, ()> {
        self.0.hook_set_log_file(path)
    }

    pub fn render_hook_context(
        &self,
        event: NativeHookEvent,
        context: &str,
    ) -> Result<String, String> {
        self.0.render_hook_context(event, context)
    }

    #[must_use]
    pub fn model_allowed(&self, model_id: &str) -> Option<String> {
        self.0.model_allowed(model_id)
    }

    pub fn resolve_model(&self, model_id: &str) -> Result<NativeResolvedClient, String> {
        self.0.resolve_model(model_id)
    }

    pub fn resolve_model_for_automatic_transition(
        &self,
        model_id: &str,
    ) -> Result<NativeResolvedClient, String> {
        self.0.resolve_model_for_automatic_transition(model_id)
    }

    #[must_use]
    pub fn default_max_output_tokens(&self, model: &str) -> u32 {
        self.0.default_max_output_tokens(model)
    }

    #[must_use]
    pub fn model_capabilities(&self, model: &str) -> NativeModelCapabilities {
        self.0.model_capabilities(model)
    }

    #[must_use]
    pub fn model_context_window(&self, model: &str) -> Option<u64> {
        self.0.model_context_window(model)
    }

    #[must_use]
    pub fn is_local_model(&self, model: &str) -> bool {
        self.0.is_local_model(model)
    }

    pub fn validate_model_transition(&self, from: &str, to: &str) -> Result<(), String> {
        self.0.validate_model_transition(from, to)
    }

    #[must_use]
    pub fn boost_choice(
        &self,
        current: &super::model_dynamics::ModelChoice,
        config: &super::model_dynamics::ModelDynamicsConfig,
    ) -> Option<super::model_dynamics::ModelChoice> {
        self.0.boost_choice(current, config)
    }

    #[must_use]
    pub fn normalize_thinking(
        &self,
        model: &str,
        requested: super::model_dynamics::ThinkingLevel,
    ) -> super::model_dynamics::ThinkingLevel {
        self.0.normalize_thinking(model, requested)
    }

    pub fn codex_auth_context(&self) -> Result<NativeCodexAuth, String> {
        self.0.codex_auth_context()
    }

    #[must_use]
    pub fn codex_auth_is_usable(&self, path: &Path) -> bool {
        self.0.codex_auth_is_usable(path)
    }

    pub fn report_diagnostic(&self, message: String) {
        self.0.report_diagnostic(message);
    }

    #[must_use]
    pub fn clamp_tool_output(
        &self,
        content: &str,
        tool_name: &str,
        spill_dir: Option<&std::path::Path>,
    ) -> NativeToolOutput {
        self.0.project_tool_output(content, tool_name, spill_dir)
    }

    #[must_use]
    pub fn model_tool_spill_dir(&self, cwd: &str, session_id: &str) -> PathBuf {
        self.0.model_tool_spill_dir(cwd, session_id)
    }

    #[must_use]
    pub fn open_todo_count(&self, output: &str) -> Option<usize> {
        self.0.open_todo_count(output)
    }

    #[must_use]
    pub fn semantic_conversation_protocol(&self) -> &str {
        self.0.semantic_conversation_protocol()
    }

    #[must_use]
    pub fn model_route(&self, model_id: &str) -> NativeModelRoute {
        self.0.model_route(model_id)
    }
}

/// Existing coding-task result projected into the runtime without exposing the
/// local coding/subagent implementation.
#[derive(Debug, Clone)]
pub struct NativeCodingCompletion {
    pub submission: maestro_coding_acceptance::CodingCompletionSubmission,
    pub child_records: Vec<maestro_coding_acceptance::CodingAcceptanceChildRecord>,
}
