//! TUI composition of the transport-neutral native runtime host.
//!
//! The runtime owns the actor and its turn state machine.  This module keeps
//! the concrete registry, hook system, action firewall, model catalog, and
//! authentication callbacks in the TUI crate and exposes them through the
//! native-specific runtime adapter.  There is deliberately one
//! `ToolExecutor` and one `IntegratedHookSystem` for the whole actor.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use maestro_runtime::agent::{
    FromAgent, InlineToolApprovalContext, NativeCodexAuth, NativeCodingCompletion,
    NativeContextEffect, NativeExecutionHost, NativeExecutionHostHandle, NativeFirewallVerdict,
    NativeHookEvent, NativeHookResult, NativeHostFuture, NativeModelCapabilities, NativeModelRoute,
    NativeReadOnlyToolCall, NativeResolvedClient, NativeToolAnnotations,
    NativeToolExecutionOptions, NativeToolOperationAdmission, SteerSignal, ToolDefinition,
    ToolExecution, WorkflowStateSnapshot,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hooks::{
    HookEventType, HookResult, IntegratedHookSystem, context::render_hook_context,
    context::render_hook_context_error,
};
use crate::safety::{ActionFirewall, FirewallContext, FirewallVerdict};
use crate::tools::{BatchConfig, BatchExecutor, BatchToolCall, ToolExecutor};

type ModelResolver = dyn Fn(&str, bool) -> Result<NativeResolvedClient, String> + Send + Sync;

fn tool_replay_policy(
    annotations: Option<&NativeToolAnnotations>,
) -> maestro_runtime_contracts::ToolReplayPolicy {
    annotations
        .filter(|annotations| {
            annotations.read_only_hint == Some(true)
                && annotations.destructive_hint != Some(true)
                && annotations.idempotent_hint == Some(true)
        })
        .map_or(maestro_runtime_contracts::ToolReplayPolicy::Never, |_| {
            maestro_runtime_contracts::ToolReplayPolicy::Safe
        })
}

fn lexical_resource_path(cwd: &Path, input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let input = Path::new(input);
    let path = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match normalized.file_name() {
                Some(name) if name != ".." => {
                    normalized.pop();
                }
                _ if !path.is_absolute() => normalized.push(".."),
                _ => {}
            },
            Component::Normal(part) => normalized.push(part),
        }
    }
    let resource_key = normalized.to_string_lossy().into_owned();
    (!resource_key.is_empty()).then_some(resource_key)
}

fn local_file_context_effect(cwd: &Path, name: &str, args: &Value) -> Option<NativeContextEffect> {
    let observes = match name.to_ascii_lowercase().as_str() {
        "read" => true,
        "write" | "edit" => false,
        _ => return None,
    };
    let raw_path = args
        .get("path")
        .or_else(|| args.get("file_path"))?
        .as_str()?;
    let resource_key = lexical_resource_path(cwd, raw_path)?;
    if observes {
        Some(NativeContextEffect::Observe { resource_key })
    } else {
        Some(NativeContextEffect::Mutate { resource_key })
    }
}

/// Concrete TUI owner of the native runtime execution boundary.
pub struct LocalNativeExecutionHost {
    experiment_scope:
        Option<Arc<std::sync::RwLock<Option<crate::telemetry::TelemetryIdentityScope>>>>,
    pinned_model_capabilities: Option<(String, NativeModelCapabilities)>,
    executor: Arc<ToolExecutor>,
    hooks: Arc<tokio::sync::Mutex<IntegratedHookSystem>>,
    resolve_model: Arc<ModelResolver>,
    model_route: Arc<dyn Fn(&str) -> NativeModelRoute + Send + Sync + 'static>,
}

impl std::fmt::Debug for LocalNativeExecutionHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalNativeExecutionHost(..)")
    }
}

impl LocalNativeExecutionHost {
    #[cfg(test)]
    pub fn compose(
        executor: Arc<ToolExecutor>,
        hooks: IntegratedHookSystem,
        resolve_model: impl Fn(&str, bool) -> Result<NativeResolvedClient, String>
        + Send
        + Sync
        + 'static,
        model_route: impl Fn(&str) -> NativeModelRoute + Send + Sync + 'static,
        pinned_model_capabilities: Option<(String, NativeModelCapabilities)>,
    ) -> NativeExecutionHostHandle {
        Self::compose_with_experiments(
            executor,
            hooks,
            resolve_model,
            model_route,
            pinned_model_capabilities,
            None,
        )
    }

    pub fn compose_with_experiments(
        executor: Arc<ToolExecutor>,
        hooks: IntegratedHookSystem,
        resolve_model: impl Fn(&str, bool) -> Result<NativeResolvedClient, String>
        + Send
        + Sync
        + 'static,
        model_route: impl Fn(&str) -> NativeModelRoute + Send + Sync + 'static,
        pinned_model_capabilities: Option<(String, NativeModelCapabilities)>,
        experiment_scope: Option<
            Arc<std::sync::RwLock<Option<crate::telemetry::TelemetryIdentityScope>>>,
        >,
    ) -> NativeExecutionHostHandle {
        NativeExecutionHostHandle::new(Arc::new(Self {
            experiment_scope,
            pinned_model_capabilities,
            executor,
            hooks: Arc::new(tokio::sync::Mutex::new(hooks)),
            resolve_model: Arc::new(resolve_model),
            model_route: Arc::new(model_route),
        }))
    }

    fn with_hooks<'a, T, F>(&'a self, f: F) -> NativeHostFuture<'a, T>
    where
        F: FnOnce(&mut IntegratedHookSystem) -> T + Send + 'a,
        T: Send + 'a,
    {
        // Hook state is shared with execute_tool, which holds this mutex while
        // the receipt-aware executor runs nested direct hooks. Waiting here
        // preserves the single hook stream instead of dropping an event when
        // that execution is in flight.
        Box::pin(async move {
            let mut hooks = self.hooks.lock().await;
            f(&mut hooks)
        })
    }

    fn hook_result(result: HookResult) -> NativeHookResult {
        match result {
            HookResult::Continue => NativeHookResult::Continue,
            HookResult::Block { reason } => NativeHookResult::Block { reason },
            HookResult::ModifyInput { new_input } => NativeHookResult::ModifyInput { new_input },
            HookResult::InjectContext { context } => NativeHookResult::InjectContext { context },
        }
    }

    fn hook_event(event: NativeHookEvent) -> HookEventType {
        match event {
            NativeHookEvent::PreToolUse => HookEventType::PreToolUse,
            NativeHookEvent::PostToolUse => HookEventType::PostToolUse,
            NativeHookEvent::EvalGate => HookEventType::EvalGate,
            NativeHookEvent::UserPromptSubmit => HookEventType::UserPromptSubmit,
            NativeHookEvent::PreMessage => HookEventType::PreMessage,
            NativeHookEvent::PostMessage => HookEventType::PostMessage,
            NativeHookEvent::OnError => HookEventType::OnError,
            NativeHookEvent::StopFailure => HookEventType::StopFailure,
            NativeHookEvent::PermissionRequest => HookEventType::PermissionRequest,
            NativeHookEvent::SessionStart => HookEventType::SessionStart,
            NativeHookEvent::SessionEnd => HookEventType::SessionEnd,
            NativeHookEvent::Overflow => HookEventType::Overflow,
        }
    }

    fn firewall_result(verdict: FirewallVerdict) -> NativeFirewallVerdict {
        match verdict {
            FirewallVerdict::Allow => NativeFirewallVerdict::Allow,
            FirewallVerdict::RequireApproval { reason } => {
                NativeFirewallVerdict::RequireApproval { reason }
            }
            FirewallVerdict::Block { reason } => NativeFirewallVerdict::Block { reason },
        }
    }

    fn mcp_annotations(
        annotations: Option<&NativeToolAnnotations>,
    ) -> Option<crate::mcp::McpToolAnnotations> {
        annotations.map(|annotations| crate::mcp::McpToolAnnotations {
            read_only_hint: annotations.read_only_hint,
            destructive_hint: annotations.destructive_hint,
            idempotent_hint: annotations.idempotent_hint,
            open_world_hint: annotations.open_world_hint,
        })
    }
}

impl NativeExecutionHost for LocalNativeExecutionHost {
    fn experiment_assignment(
        &self,
        model: &str,
    ) -> Option<maestro_runtime_contracts::experiments::ExperimentAssignment> {
        let scope = self.experiment_scope.as_ref()?.read().ok()?.clone()?;
        crate::experiments::enroll(&scope, model)
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.executor.tool_definitions().cloned().collect()
    }

    fn tool_context_effect(&self, name: &str, args: &Value) -> Option<NativeContextEffect> {
        local_file_context_effect(Path::new(self.executor.cwd()), name, args)
    }

    fn has_native_tool(&self, name: &str) -> bool {
        self.executor.has_tool(name)
    }

    fn is_reserved_tool(&self, name: &str) -> bool {
        crate::tools::orb_delegation::is_reserved_orb_tool(name)
    }

    fn goal_tools_visible(&self) -> bool {
        crate::goal::GoalStore::load_default().tools_visible()
    }

    fn include_ide_tools(&self) -> bool {
        std::env::var("MAESTRO_INCLUDE_IDE_TOOLS")
            .ok()
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
    }

    fn missing_required(&self, name: &str, args: &Value) -> Vec<String> {
        self.executor.missing_required(name, args)
    }

    fn has_code_authority(&self) -> bool {
        self.executor.has_code_authority()
    }

    fn requires_sandbox_bypass_approval(&self, name: &str, args: &Value) -> bool {
        self.executor.requires_sandbox_bypass_approval(name, args)
    }

    fn mcp_permission_allows(&self, name: &str) -> bool {
        self.executor.mcp_permission_allows(name)
    }

    fn requires_approval(&self, name: &str, args: &Value) -> bool {
        self.executor.requires_approval(name, args)
    }

    fn is_mcp_tool(&self, name: &str) -> bool {
        crate::mcp::McpClient::is_mcp_tool(name)
    }

    fn tool_annotations(&self, name: &str) -> Option<NativeToolAnnotations> {
        self.executor
            .tool_annotations(name)
            .map(|annotations| NativeToolAnnotations {
                read_only_hint: annotations.read_only_hint,
                destructive_hint: annotations.destructive_hint,
                idempotent_hint: annotations.idempotent_hint,
                open_world_hint: annotations.open_world_hint,
            })
    }

    fn tool_operation_admission(&self, name: &str, _args: &Value) -> NativeToolOperationAdmission {
        NativeToolOperationAdmission {
            replay_policy: tool_replay_policy(self.tool_annotations(name).as_ref()),
            idempotency_key: None,
        }
    }

    fn ensure_mcp_annotations(&self) -> NativeHostFuture<'_, Result<(), String>> {
        Box::pin(async move { self.executor.ensure_mcp_annotations().await })
    }

    fn inline_tool_approval_context(&self, name: &str) -> Option<InlineToolApprovalContext> {
        let tool = self.executor.get_inline_tool(name)?;
        let (shell, shell_arg) = self.executor.inline_tool_effective_shell();
        Some(InlineToolApprovalContext {
            command: tool.definition.command.clone(),
            source_path: tool.source_path.display().to_string(),
            source_label: tool.source.label().to_owned(),
            cwd: self.executor.inline_tool_effective_cwd(tool),
            environment: self.executor.inline_tool_effective_env(tool),
            shell,
            shell_arg: shell_arg.to_owned(),
        })
    }

    fn is_explicit_inline_read_only_tool(&self, name: &str) -> bool {
        self.executor.inline_tools().any(|tool| {
            tool.definition.name.eq_ignore_ascii_case(name)
                && tool.definition.annotations.read_only
                && !tool.definition.annotations.destructive
        })
    }

    fn credential_generation(&self) -> u64 {
        self.executor.credential_generation()
    }

    fn file_read_verdict(&self, path: &str) -> NativeFirewallVerdict {
        Self::firewall_result(ActionFirewall::new(self.executor.cwd()).check_file_read(path))
    }

    fn video_mime(&self, path: &Path) -> Option<(String, u64)> {
        crate::video::detect_video_mime(path)
            .map(|mime| (mime.to_owned(), crate::video::MAX_VIDEO_BYTES))
    }

    fn extract_video_frames<'a>(
        &'a self,
        path: &'a Path,
    ) -> NativeHostFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            crate::video::extract_frames(path)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn managed_policy_metadata(&self) -> Option<maestro_runtime::ManagedPolicyMetadata> {
        crate::safety::managed_policy_metadata()
    }

    fn firewall_verdict(
        &self,
        name: &str,
        args: &Value,
        workflow_state: &WorkflowStateSnapshot,
        annotations: Option<&NativeToolAnnotations>,
        external: bool,
    ) -> NativeFirewallVerdict {
        if external {
            return NativeFirewallVerdict::Allow;
        }
        let firewall = ActionFirewall::new(self.executor.cwd());
        Self::firewall_result(firewall.check_tool_with_context(FirewallContext {
            tool_name: name,
            args,
            workflow_state: Some(workflow_state),
            annotations: Self::mcp_annotations(annotations).as_ref(),
        }))
    }

    fn execute_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        event_tx: Option<&'a mpsc::UnboundedSender<FromAgent>>,
        call_id: &'a str,
        options: NativeToolExecutionOptions<'a>,
    ) -> NativeHostFuture<'a, ToolExecution> {
        let executor = Arc::clone(&self.executor);
        let hooks = Arc::clone(&self.hooks);
        let name = name.to_owned();
        let args = args.clone();
        let call_id = call_id.to_owned();
        let event_tx = event_tx.cloned();
        let cancel = options.cancel;
        let approved_inline_env = options.approved_inline_env.cloned();
        let receipt_policy = self.managed_policy_metadata();
        Box::pin(async move {
            let mut hook_guard = hooks.lock().await;
            let execution = executor
                .execute_with_receipt_cancellable_inline_env(
                    &name,
                    &args,
                    event_tx.as_ref(),
                    &call_id,
                    crate::tools::ToolExecutionOptions {
                        cancel,
                        approved_inline_env: approved_inline_env.as_ref(),
                        hooks: Some(&mut *hook_guard),
                    },
                )
                .await;
            execution.with_managed_policy(receipt_policy)
        })
    }

    fn execute_read_only_wave<'a>(
        &'a self,
        calls: &'a [NativeReadOnlyToolCall],
        event_tx: &'a mpsc::UnboundedSender<FromAgent>,
        cancel: Option<CancellationToken>,
    ) -> NativeHostFuture<'a, HashMap<String, ToolExecution>> {
        let calls = calls
            .iter()
            .map(|call| {
                BatchToolCall::new(
                    call.call_id.clone(),
                    call.tool_name.clone(),
                    call.args.clone(),
                )
            })
            .collect::<Vec<_>>();
        let executor = Arc::clone(&self.executor);
        let event_tx = event_tx.clone();
        let generation = self.executor.credential_generation();
        let config = native_read_only_batch_config();
        let receipt_policy = self.managed_policy_metadata();
        Box::pin(async move {
            let batch = BatchExecutor::from_shared_executor(executor, config);
            let results = match cancel {
                Some(cancel) => {
                    batch
                        .execute_with_cancel_at_generation(
                            calls,
                            Some(event_tx),
                            cancel,
                            generation,
                        )
                        .await
                }
                None => {
                    batch
                        .execute_at_generation(calls, Some(event_tx), generation)
                        .await
                }
            };
            results
                .into_iter()
                .map(|result| {
                    let execution = result.execution.unwrap_or_else(|| {
                        ToolExecution::from_legacy(
                            &result.call_id,
                            &result.tool_name,
                            maestro_runtime::agent::ExecutionSource::Native,
                            result.result,
                        )
                    });
                    let execution = execution.with_managed_policy(receipt_policy.clone());
                    (result.call_id, execution)
                })
                .collect()
        })
    }

    fn clear_cache(&self) {
        self.executor.clear_cache();
    }

    fn set_steer_signal(&self, signal: Arc<SteerSignal>) {
        self.executor.set_steer_signal(signal);
    }

    fn reset_coding_turn(&self) {
        self.executor.reset_coding_turn();
    }

    fn set_subagent_parent_scope(&self, scope: String) {
        self.executor.set_subagent_parent_scope(scope);
    }

    fn set_subagent_parent_model(&self, model: String, thinking: String) {
        let thinking = maestro_runtime::agent::ThinkingLevel::parse(&thinking).unwrap_or_default();
        self.executor
            .set_subagent_parent_model(crate::model_dynamics::ModelChoice { model, thinking });
    }

    fn set_subagent_parent_requests(&self, requests: Vec<String>) {
        self.executor.set_subagent_parent_requests(requests);
    }

    fn coding_completion(&self) -> Result<Option<NativeCodingCompletion>, String> {
        self.executor.coding_completion().map(|completion| {
            completion.map(|(_, submission, child_records)| NativeCodingCompletion {
                submission,
                child_records,
            })
        })
    }

    fn shutdown_background_processes(&self) -> NativeHostFuture<'_, ()> {
        Box::pin(async move { self.executor.shutdown_background_processes().await })
    }

    fn hook_pre_tool_use<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_pre_tool_use(name, call_id, args))
        })
    }

    fn hook_post_tool_use<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        output: &'a str,
        is_error: bool,
        duration_ms: u64,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_post_tool_use(
                name,
                call_id,
                args,
                output,
                is_error,
                duration_ms,
            ))
        })
    }

    fn hook_eval_gate<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        output: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_eval_gate(name, call_id, args, output))
        })
    }

    fn hook_user_prompt_submit<'a>(
        &'a self,
        prompt: &'a str,
        attachment_count: u32,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_user_prompt_submit(prompt, attachment_count))
        })
    }

    fn hook_pre_message<'a>(
        &'a self,
        message: &'a str,
        attachments: &'a [String],
        model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_pre_message(message, attachments, model))
        })
    }

    fn hook_post_message<'a>(
        &'a self,
        response: &'a str,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        stop_reason: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_post_message(
                response,
                input_tokens,
                output_tokens,
                duration_ms,
                stop_reason,
            ))
        })
    }

    fn hook_on_error<'a>(
        &'a self,
        error: &'a str,
        error_kind: &'a str,
        context: Option<&'a str>,
        recoverable: bool,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_on_error(error, error_kind, context, recoverable))
        })
    }

    fn hook_stop_failure<'a>(
        &'a self,
        error: &'a str,
        error_details: Option<&'a str>,
        last_assistant_message: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_stop_failure(
                error,
                error_details,
                last_assistant_message,
            ))
        })
    }

    fn hook_permission_request<'a>(
        &'a self,
        name: &'a str,
        call_id: &'a str,
        args: &'a Value,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| {
            Self::hook_result(hooks.execute_permission_request(name, call_id, args, reason))
        })
    }

    fn hook_handle_overflow(&self) -> NativeHostFuture<'_, bool> {
        self.with_hooks(|hooks| hooks.handle_overflow())
    }

    fn hook_checkpoint_transcript_before_response(&self) -> NativeHostFuture<'_, ()> {
        self.with_hooks(|hooks| hooks.checkpoint_transcript_before_response())
    }

    fn hook_record_tool_operation<'a>(
        &'a self,
        record: &'a maestro_runtime_contracts::ToolOperationRecord,
    ) -> NativeHostFuture<'a, Result<(), String>> {
        self.with_hooks(move |hooks| hooks.record_tool_operation(record))
    }

    fn hook_load_tool_operations(
        &self,
    ) -> NativeHostFuture<'_, Result<Vec<maestro_runtime_contracts::ToolOperationRecord>, String>>
    {
        self.with_hooks(|hooks| hooks.load_tool_operations())
    }

    fn hook_session_id(&self) -> NativeHostFuture<'_, Option<String>> {
        self.with_hooks(|hooks| hooks.session_id().map(str::to_owned))
    }

    fn hook_set_session_context(
        &self,
        session_id: Option<String>,
        transcript_path: Option<String>,
    ) -> NativeHostFuture<'_, ()> {
        self.with_hooks(move |hooks| hooks.set_session_context(session_id, transcript_path))
    }

    fn hook_on_session_start<'a>(
        &'a self,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| Self::hook_result(hooks.on_session_start(reason)))
    }

    fn hook_on_session_end<'a>(
        &'a self,
        reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        self.with_hooks(move |hooks| Self::hook_result(hooks.on_session_end(reason)))
    }

    fn hook_increment_turn(&self) -> NativeHostFuture<'_, ()> {
        self.with_hooks(IntegratedHookSystem::increment_turn)
    }

    fn hook_set_model<'a>(&'a self, model: &'a str) -> NativeHostFuture<'a, ()> {
        let model = model.to_owned();
        self.with_hooks(move |hooks| hooks.set_model(&model))
    }

    fn hook_set_log_file(&self, path: Option<String>) -> NativeHostFuture<'_, ()> {
        self.with_hooks(move |hooks| hooks.set_log_file(path))
    }

    fn render_hook_context(&self, event: NativeHookEvent, context: &str) -> Result<String, String> {
        render_hook_context(Self::hook_event(event), context)
            .map_err(|error| render_hook_context_error(&error))
    }

    fn model_allowed(&self, model_id: &str) -> Option<String> {
        crate::safety::check_model_allowed(model_id)
    }

    fn resolve_model(&self, model_id: &str) -> Result<NativeResolvedClient, String> {
        (self.resolve_model)(model_id, false)
    }

    fn resolve_model_for_automatic_transition(
        &self,
        model_id: &str,
    ) -> Result<NativeResolvedClient, String> {
        (self.resolve_model)(model_id, true)
    }

    fn default_max_output_tokens(&self, model: &str) -> u32 {
        crate::model_catalog::default_max_output_tokens(model)
    }

    fn is_local_model(&self, model: &str) -> bool {
        crate::local_models::find_discovered_model(model).is_some()
    }

    fn model_context_window(&self, model: &str) -> Option<u64> {
        crate::model_catalog::find_model(model)
            .map(|model| model.capabilities.context_tokens as u64)
    }

    fn model_capabilities(&self, model: &str) -> NativeModelCapabilities {
        self.pinned_model_capabilities
            .as_ref()
            .filter(|(bound_model, _)| bound_model == model)
            .map_or_else(|| catalog_model_capabilities(model), |(_, caps)| *caps)
    }

    fn validate_model_transition(&self, from: &str, to: &str) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        if self.model_route(from).uses_app_server() {
            return Err("Use /model to change a Codex session model".to_owned());
        }
        let old = crate::model_catalog::find_model(from)
            .ok_or_else(|| "Current model capabilities are unavailable".to_owned())?;
        let new = crate::model_catalog::find_model(to)
            .ok_or_else(|| "Target model capabilities are unavailable".to_owned())?;
        if old.capabilities.protocol != new.capabilities.protocol
            || new.capabilities.context_tokens < old.capabilities.context_tokens
            || (old.capabilities.vision && !new.capabilities.vision)
            || (old.capabilities.tools && !new.capabilities.tools)
        {
            return Err(
                "This model change needs an explicit context transition; use /model".to_owned(),
            );
        }
        Ok(())
    }

    fn normalize_thinking(
        &self,
        model: &str,
        requested: maestro_runtime::agent::ThinkingLevel,
    ) -> maestro_runtime::agent::ThinkingLevel {
        crate::model_dynamics::normalize_thinking(model, requested)
    }

    fn boost_choice(
        &self,
        current: &maestro_runtime::agent::ModelChoice,
        config: &maestro_runtime::agent::ModelDynamicsConfig,
    ) -> Option<maestro_runtime::agent::ModelChoice> {
        crate::model_dynamics::boost_choice(current, config)
    }

    fn codex_auth_context(&self) -> Result<NativeCodexAuth, String> {
        let requested_profile =
            crate::service_connections::selected_delegated_profile_from_env("openai-codex")
                .map_err(|error| format!("Codex managed connection selection failed: {error:#}"))?;
        let identity = crate::codex_identity::resolve_codex_identity(
            requested_profile.as_deref(),
            Path::new(self.executor.cwd()),
        )
        .map_err(|error| format!("Codex identity selection failed: {error:#}"))?;
        let state_root = crate::path_utils::maestro_home_dir()
            .ok_or_else(|| "Maestro home is unavailable for Codex thread bindings".to_owned())?;
        Ok(NativeCodexAuth {
            profile_name: identity.profile_name.clone(),
            child_env: identity.child_env(),
            auth_path: identity.auth_path(),
            state_root,
        })
    }

    fn codex_auth_is_usable(&self, path: &Path) -> bool {
        crate::codex_identity::inspect_codex_auth(path)
            .state
            .is_usable()
    }

    fn report_diagnostic(&self, message: String) {
        crate::headless::report_diagnostic_nonblocking(message);
    }

    fn clamp_tool_output(
        &self,
        content: &str,
        tool_name: &str,
        spill_dir: Option<&Path>,
    ) -> String {
        self.project_tool_output(content, tool_name, spill_dir)
            .content
    }

    fn project_tool_output(
        &self,
        content: &str,
        tool_name: &str,
        spill_dir: Option<&Path>,
    ) -> maestro_runtime::agent::native_host::NativeToolOutput {
        let payload = crate::tool_output::clamp_for_model(content, tool_name, spill_dir);
        let saved_path = match &payload {
            crate::tool_output::ModelToolPayload::Spilled { path, .. } => Some(path.clone()),
            crate::tool_output::ModelToolPayload::Inline(_) => None,
        };
        maestro_runtime::agent::native_host::NativeToolOutput {
            content: payload.into_model_text(),
            saved_path,
        }
    }

    fn model_tool_spill_dir(&self, cwd: &str, session_id: &str) -> std::path::PathBuf {
        crate::tool_output::model_tool_spill_dir(cwd, session_id)
    }

    fn open_todo_count(&self, output: &str) -> Option<usize> {
        Some(crate::tools::todo::open_todo_count_from_output(output))
    }

    fn semantic_conversation_protocol(&self) -> &str {
        crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
    }

    fn model_route(&self, model_id: &str) -> NativeModelRoute {
        (self.model_route)(model_id)
    }
}

fn native_read_only_batch_config() -> BatchConfig {
    BatchConfig::default()
        .with_concurrency(
            std::env::var("MAESTRO_NATIVE_READ_ONLY_TOOL_CONCURRENCY")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map_or(8, |value| value.clamp(1, 16)),
        )
        .continue_on_error(true)
        .emit_events(true)
}

/// Catalog facts shared by execution and provider-prompt receipts.
pub(crate) fn catalog_model_capabilities(model: &str) -> NativeModelCapabilities {
    let catalog_model = crate::model_catalog::find_model(model).or_else(|| {
        let (namespace, underlying) = model.split_once('/')?;
        if namespace.eq_ignore_ascii_case("evalops")
            || namespace.eq_ignore_ascii_case("maestro-managed")
        {
            // Remove only the managed namespace. Keep provider qualification
            // so a custom endpoint cannot inherit another provider's facts.
            crate::model_catalog::find_model(underlying)
        } else {
            None
        }
    });
    let Some(model) = catalog_model else {
        return NativeModelCapabilities::default();
    };
    NativeModelCapabilities {
        vision: Some(model.capabilities.vision),
        tool_calling: Some(model.capabilities.tools),
        reasoning: Some(model.capabilities.reasoning),
        context_tokens: Some(u64::from(model.capabilities.context_tokens))
            .filter(|tokens| *tokens > 0),
        output_tokens: model
            .capabilities
            .output_tokens
            .filter(|tokens| *tokens > 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{
        EvalGateHook, EvalGateInput, HookResult, PostToolUseHook, PostToolUseInput, SessionEndHook,
        SessionEndInput, SessionStartHook, SessionStartInput,
    };
    use std::sync::Mutex;

    #[test]
    fn replay_requires_explicit_read_only_and_idempotent_annotations() {
        let safe = NativeToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: None,
        };
        assert_eq!(
            tool_replay_policy(Some(&safe)),
            maestro_runtime_contracts::ToolReplayPolicy::Safe
        );

        for annotations in [
            None,
            Some(NativeToolAnnotations {
                read_only_hint: None,
                ..safe.clone()
            }),
            Some(NativeToolAnnotations {
                idempotent_hint: None,
                ..safe.clone()
            }),
            Some(NativeToolAnnotations {
                destructive_hint: Some(true),
                ..safe.clone()
            }),
        ] {
            assert_eq!(
                tool_replay_policy(annotations.as_ref()),
                maestro_runtime_contracts::ToolReplayPolicy::Never
            );
        }
    }

    #[derive(Default)]
    struct RecordingHook {
        events: Mutex<Vec<String>>,
    }

    impl RecordingHook {
        fn events(&self) -> Vec<String> {
            self.events.lock().expect("recording hook lock").clone()
        }

        fn record(&self, event: String) {
            self.events.lock().expect("recording hook lock").push(event);
        }
    }

    impl PostToolUseHook for RecordingHook {
        fn on_post_tool_use(&self, input: &PostToolUseInput) -> HookResult {
            self.record(format!(
                "post:{}:{}",
                input.session_id.as_deref().unwrap_or("<none>"),
                input.tool_call_id
            ));
            HookResult::Continue
        }
    }

    impl EvalGateHook for RecordingHook {
        fn on_eval_gate(&self, input: &EvalGateInput) -> HookResult {
            self.record(format!(
                "eval:{}:{}",
                input.session_id.as_deref().unwrap_or("<none>"),
                input.tool_call_id
            ));
            HookResult::Continue
        }
    }

    impl SessionStartHook for RecordingHook {
        fn on_session_start(&self, input: &SessionStartInput) -> HookResult {
            self.record(format!(
                "start:{}:{}",
                input.session_id.as_deref().unwrap_or("<none>"),
                input.source
            ));
            HookResult::Continue
        }
    }

    impl SessionEndHook for RecordingHook {
        fn on_session_end(&self, input: &SessionEndInput) -> HookResult {
            self.record(format!(
                "end:{}:{}",
                input.session_id.as_deref().unwrap_or("<none>"),
                input.reason
            ));
            HookResult::Continue
        }
    }

    fn test_host() -> (Arc<LocalNativeExecutionHost>, Arc<RecordingHook>) {
        let recording = Arc::new(RecordingHook::default());
        let mut hooks = IntegratedHookSystem::new("/tmp");
        hooks.registry.register_post_tool_use(recording.clone());
        hooks.registry.register_eval_gate(recording.clone());
        hooks.registry.register_session_start(recording.clone());
        hooks.registry.register_session_end(recording.clone());

        let resolve_model: Arc<ModelResolver> =
            Arc::new(|_, _| Err("test host has no model resolver".to_owned()));
        let model_route: Arc<dyn Fn(&str) -> NativeModelRoute + Send + Sync + 'static> =
            Arc::new(|_| NativeModelRoute::DirectProvider);

        (
            Arc::new(LocalNativeExecutionHost {
                experiment_scope: None,
                pinned_model_capabilities: None,
                executor: Arc::new(ToolExecutor::new("/tmp")),
                hooks: Arc::new(tokio::sync::Mutex::new(hooks)),
                resolve_model,
                model_route,
            }),
            recording,
        )
    }

    #[test]
    fn local_context_effect_normalizes_equivalent_file_paths() {
        let cwd = Path::new("/workspace/project");

        assert_eq!(
            local_file_context_effect(
                cwd,
                "read",
                &serde_json::json!({"path":"./src/../src/lib.rs"}),
            ),
            Some(NativeContextEffect::Observe {
                resource_key: "/workspace/project/src/lib.rs".to_owned(),
            })
        );
        assert_eq!(
            local_file_context_effect(cwd, "EDIT", &serde_json::json!({"path":"src/lib.rs"}),),
            Some(NativeContextEffect::Mutate {
                resource_key: "/workspace/project/src/lib.rs".to_owned(),
            })
        );
    }

    #[test]
    fn local_context_effect_accepts_file_path_for_writes() {
        assert_eq!(
            local_file_context_effect(
                Path::new("/workspace"),
                "write",
                &serde_json::json!({"file_path":"config.toml"}),
            ),
            Some(NativeContextEffect::Mutate {
                resource_key: "/workspace/config.toml".to_owned(),
            })
        );
    }

    #[test]
    fn local_context_effect_rejects_empty_or_opaque_tools() {
        assert_eq!(
            local_file_context_effect(
                Path::new("/workspace"),
                "read",
                &serde_json::json!({"path":"  "}),
            ),
            None
        );
        assert_eq!(
            local_file_context_effect(
                Path::new("/workspace"),
                "bash",
                &serde_json::json!({"path":"src/lib.rs"}),
            ),
            None
        );
    }

    #[test]
    fn bound_prompt_capabilities_survive_a_different_catalog_snapshot() {
        let (mut host, _) = test_host();
        let model = "openai/gpt-4o";
        let mut bound = catalog_model_capabilities(model);
        bound.context_tokens = Some(12345);
        assert_ne!(bound, catalog_model_capabilities(model));
        Arc::get_mut(&mut host).unwrap().pinned_model_capabilities =
            Some((model.to_owned(), bound));
        let receipt_prompt = crate::agent::provider_system_prompt("fixture", model, bound);
        let provider_prompt = maestro_runtime::agent::runtime_system_prompt(
            Some("fixture"),
            None,
            model,
            host.model_capabilities(model),
        )
        .unwrap();
        assert_eq!(receipt_prompt, provider_prompt);
        assert_eq!(
            host.model_capabilities("unknown/model"),
            NativeModelCapabilities::default()
        );
    }

    #[test]
    fn model_capabilities_come_from_catalog_and_unknown_models_stay_unknown() {
        let (host, _) = test_host();
        let known = host.model_capabilities("openai/gpt-4o");
        assert_eq!(known.vision, Some(true));
        assert_eq!(known.tool_calling, Some(true));
        assert_eq!(known.reasoning, Some(false));
        assert!(known.context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(known.output_tokens.is_some_and(|tokens| tokens > 0));
        for model in [
            "evalops/gpt-4o",
            "maestro-managed/openai/gpt-4o",
            "EVALOPS/openai/gpt-4o",
        ] {
            assert_eq!(host.model_capabilities(model), known, "{model}");
        }
        for model in [
            "unknown-provider/unknown-model",
            "unknown-provider/gpt-4o",
            "evalops/unknown-model",
        ] {
            assert_eq!(
                host.model_capabilities(model),
                NativeModelCapabilities::default(),
                "{model}"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_hook_calls_wait_for_execution_lock_and_run_once_with_session_attribution() {
        let (host, recording) = test_host();
        host.hook_set_session_context(Some("session-42".to_owned()), None)
            .await;
        assert_eq!(host.hook_session_id().await, Some("session-42".to_owned()));

        // `execute_tool` holds this same guard while the existing executor
        // runs. Holding it here makes the regression deterministic without
        // depending on a shell or a host-specific tool implementation.
        let execution_guard = host.hooks.lock().await;
        let post_task = tokio::spawn({
            let host = Arc::clone(&host);
            async move {
                let args = serde_json::json!({"command": "pwd"});
                host.hook_post_tool_use("bash", "call-post", &args, "ok", false, 7)
                    .await
            }
        });
        let eval_task = tokio::spawn({
            let host = Arc::clone(&host);
            async move {
                let args = serde_json::json!({"command": "pwd"});
                host.hook_eval_gate("bash", "call-eval", &args, "ok").await
            }
        });

        tokio::task::yield_now().await;
        assert!(
            !post_task.is_finished(),
            "post hook bypassed the execution lock"
        );
        assert!(
            !eval_task.is_finished(),
            "eval hook bypassed the execution lock"
        );

        drop(execution_guard);
        assert!(matches!(
            post_task.await.expect("post task"),
            NativeHookResult::Continue
        ));
        assert!(matches!(
            eval_task.await.expect("eval task"),
            NativeHookResult::Continue
        ));

        assert!(matches!(
            host.hook_on_session_start("resume").await,
            NativeHookResult::Continue
        ));
        assert!(matches!(
            host.hook_on_session_end("shutdown").await,
            NativeHookResult::Continue
        ));

        let events = recording.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("post:"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("eval:"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("start:"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("end:"))
                .count(),
            1
        );
        assert!(events.contains(&"post:session-42:call-post".to_owned()));
        assert!(events.contains(&"eval:session-42:call-eval".to_owned()));
        assert!(events.contains(&"start:session-42:resume".to_owned()));
        assert!(events.contains(&"end:session-42:shutdown".to_owned()));
    }
}
