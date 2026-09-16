#[test]
fn prompt_experiment_excludes_auxiliary_compaction_receipts() {
    for eligible in [false, true] {
        let receipt = maestro_ai::ManagedGatewayReceipt {
            request_id: "request".into(),
            record_id: "record".into(),
            lineage_id: "lineage".into(),
            record_status: "planned".into(),
            provider_prompt_sha256: Some("sha256:verified".into()),
            provider_tools_sha256: Some("sha256:tools".into()),
            provider_tool_count: Some(3),
        };
        let FromAgent::ManagedGatewayReceipt {
            record_id,
            provider_prompt_sha256,
            provider_tools_sha256,
            provider_tool_count,
            ..
        } = NativeAgentRunner::managed_gateway_receipt_event(receipt, eligible)
        else {
            panic!("gateway receipt must be preserved")
        };
        assert_eq!(record_id, "record");
        assert_eq!(
            provider_tools_sha256.as_deref(),
            eligible.then_some("sha256:tools")
        );
        assert_eq!(provider_tool_count, eligible.then_some(3));
        assert_eq!(
            provider_prompt_sha256.as_deref(),
            eligible.then_some("sha256:verified")
        );
    }
}

use super::super::native_host::{
    NativeCodexAuth, NativeExecutionHost, NativeExecutionHostHandle, NativeFirewallVerdict,
    NativeHookEvent, NativeHookResult, NativeHostFuture, NativeModelRoute, NativeReadOnlyToolCall,
    NativeResolvedClient, NativeToolAnnotations, NativeToolExecutionOptions,
    NativeToolOperationAdmission, ToolDefinition,
};
use super::super::protocol::InlineToolApprovalContext;
use super::super::{
    BoostStatus, ModelChoice, ModelDynamicsConfig, NativeCodingCompletion, SteerSignal,
    ThinkingLevel, WorkflowStateSnapshot,
};
use super::*;
use std::fmt::Write as _;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn empty_runtime_audit() -> Arc<RwLock<RuntimeAuditSnapshot>> {
    Arc::new(RwLock::new(RuntimeAuditSnapshot {
        request_cache: None,
        cache_reuse: None,
        request_context: None,
        excluded_context_tools: HashSet::new(),
        prompt_revision: 0,
        system_prompt: None,
        tools: Vec::new(),
        max_output_tokens: 16_384,
        context_window: Some(128_000),
    }))
}

/// A deterministic host for runtime-owned tests.
///
/// The runtime crate deliberately has no dependency on the TUI registry or
/// hook implementation.  Tests that exercise the actor therefore compose the
/// same public host boundary that a real application uses.  This host keeps
/// tool behavior small and deterministic; concrete filesystem/process/MCP
/// behavior remains covered by the TUI host tests.
#[derive(Clone)]
pub(super) struct RuntimeTestHost {
    cwd: Arc<std::path::PathBuf>,
    client: Arc<UnifiedClient>,
    session_id: Arc<Mutex<Option<String>>>,
    provider_admission_blocked: Arc<AtomicBool>,
    experiment: Arc<Mutex<Option<maestro_runtime_contracts::experiments::ExperimentAssignment>>>,
    block_provider_after_tool: bool,
    post_tool_context: Option<String>,
    checkpoint_barrier: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify, AtomicBool)>>,
    completed_tool_executions: Arc<AtomicUsize>,
    tool_operation_records: Option<Arc<Mutex<Vec<maestro_runtime_contracts::ToolOperationRecord>>>>,
    replay_safe_tools: HashSet<String>,
    tool_definitions: Arc<Vec<ToolDefinition>>,
    reserved_tools: HashSet<String>,
    mcp_permission_tools: HashSet<String>,
    code_authority: bool,
    sandbox_policy: bool,
    max_output_tokens: u32,
    context_window: u64,
    model_capabilities: HashMap<String, NativeModelCapabilities>,
}

impl RuntimeTestHost {
    pub(super) fn new(cwd: impl Into<std::path::PathBuf>, client: UnifiedClient) -> Self {
        let tool_definitions = [
            ("bash", "Run a deterministic shell fixture"),
            ("read", "Read a deterministic fixture file"),
            ("write", "Write a deterministic fixture file"),
            ("edit", "Edit a deterministic fixture file"),
            ("glob", "Find deterministic fixture files"),
            ("grep", "Search deterministic fixture files"),
            ("todo", "Update the deterministic todo fixture"),
            ("update_goal", "Update the deterministic goal fixture"),
            ("tool_search", "Search the deterministic tool catalog"),
            (
                "recall_output",
                "Recall an earlier deterministic tool result",
            ),
            ("explore", "Explore the deterministic workspace"),
        ]
        .into_iter()
        .map(|(name, description)| ToolDefinition {
            tool: Tool::new(name, description).with_schema(serde_json::json!({
                "type": "object",
                "additionalProperties": true
            })),
            requires_approval: false,
        })
        .collect();
        Self {
            cwd: Arc::new(cwd.into()),
            client: Arc::new(client),
            session_id: Arc::new(Mutex::new(None)),
            provider_admission_blocked: Arc::new(AtomicBool::new(false)),
            experiment: Arc::new(Mutex::new(None)),
            block_provider_after_tool: false,
            post_tool_context: None,
            checkpoint_barrier: None,
            completed_tool_executions: Arc::new(AtomicUsize::new(0)),
            tool_operation_records: None,
            replay_safe_tools: HashSet::new(),
            tool_definitions: Arc::new(tool_definitions),
            reserved_tools: HashSet::new(),
            mcp_permission_tools: HashSet::new(),
            code_authority: true,
            sandbox_policy: false,
            max_output_tokens: 16_384,
            context_window: 128_000,
            model_capabilities: HashMap::new(),
        }
    }

    fn with_code_authority(mut self, enabled: bool) -> Self {
        self.code_authority = enabled;
        self
    }

    fn with_sandbox_policy(mut self, enabled: bool) -> Self {
        self.sandbox_policy = enabled;
        self
    }

    fn with_model_limits(mut self, max_output_tokens: u32, context_window: u64) -> Self {
        self.max_output_tokens = max_output_tokens;
        self.context_window = context_window;
        self
    }

    fn with_provider_admission_blocked(self, blocked: bool) -> Self {
        self.provider_admission_blocked
            .store(blocked, Ordering::SeqCst);
        self
    }

    fn with_provider_admission_blocked_after_tool(mut self) -> Self {
        self.block_provider_after_tool = true;
        self
    }

    fn with_tool_operation_journal(
        mut self,
        records: Arc<Mutex<Vec<maestro_runtime_contracts::ToolOperationRecord>>>,
    ) -> Self {
        self.tool_operation_records = Some(records);
        self
    }

    fn with_replay_safe_tool(mut self, tool_name: &str) -> Self {
        self.replay_safe_tools
            .insert(tool_name.to_ascii_lowercase());
        self
    }

    fn assert_effect_pending(&self, call_id: &str) {
        let Some(records) = &self.tool_operation_records else {
            return;
        };
        let records = records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            records.iter().rev().any(|record| {
                record.call_id == call_id
                    && record.phase == maestro_runtime_contracts::ToolOperationPhase::EffectPending
            }),
            "effect_pending must be durable before executing {call_id}"
        );
    }

    fn execution(&self, call_id: &str, name: &str, args: &Value) -> ToolExecution {
        let output = match name.to_ascii_lowercase().as_str() {
            "read" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let path = self.cwd.join(path);
                std::fs::read_to_string(path).unwrap_or_else(|_| "fixture read result".to_owned())
            }
            "write" | "edit" => args
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("fixture write result")
                .to_owned(),
            "bash" => {
                let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                if command.trim().is_empty() || command.contains("pwd") {
                    self.cwd.display().to_string()
                } else if let Some(value) = command
                    .strip_prefix("printf ")
                    .or_else(|| command.strip_prefix("echo "))
                {
                    value.trim_matches([' ', '\'', '"']).to_owned()
                } else {
                    format!("fixture bash result: {command}")
                }
            }
            "update_goal" => args.get("status").and_then(Value::as_str).map_or_else(
                || serde_json::json!({"goal": {"status": "active"}}).to_string(),
                |status| serde_json::json!({"goal": {"status": status}}).to_string(),
            ),
            "todo" => serde_json::json!({"open": 0}).to_string(),
            _ => "fixture tool result".to_owned(),
        };
        let execution = ToolExecution::from_legacy(
            call_id,
            name,
            ExecutionSource::Native,
            ToolResult::success(output),
        );
        self.completed_tool_executions
            .fetch_add(1, Ordering::SeqCst);
        execution
    }

    fn hook_result() -> NativeHookResult {
        NativeHookResult::Continue
    }
}

impl NativeExecutionHost for RuntimeTestHost {
    fn experiment_assignment(
        &self,
        _model: &str,
    ) -> Option<maestro_runtime_contracts::experiments::ExperimentAssignment> {
        self.experiment.lock().unwrap().clone()
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_definitions.as_ref().clone()
    }

    fn has_native_tool(&self, name: &str) -> bool {
        self.tool_definitions
            .iter()
            .any(|definition| definition.tool.name.eq_ignore_ascii_case(name))
    }

    fn is_reserved_tool(&self, name: &str) -> bool {
        self.reserved_tools.contains(&name.to_ascii_lowercase())
    }

    fn goal_tools_visible(&self) -> bool {
        true
    }

    fn include_ide_tools(&self) -> bool {
        false
    }

    fn missing_required(&self, name: &str, args: &Value) -> Vec<String> {
        let required = match name.to_ascii_lowercase().as_str() {
            "bash" => "command",
            "read" => "path",
            _ => return Vec::new(),
        };
        args.get(required)
            .is_some_and(Value::is_string)
            .then(Vec::new)
            .unwrap_or_else(|| vec![required.to_owned()])
    }

    fn has_code_authority(&self) -> bool {
        self.code_authority
    }

    fn requires_sandbox_bypass_approval(&self, _name: &str, args: &Value) -> bool {
        self.sandbox_policy
            && args
                .get("bypass_sandbox")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }

    fn mcp_permission_allows(&self, name: &str) -> bool {
        self.mcp_permission_tools
            .contains(&name.to_ascii_lowercase())
    }

    fn requires_approval(&self, name: &str, args: &Value) -> bool {
        match name.to_ascii_lowercase().as_str() {
            "bash" => args
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    [
                        "rm ",
                        "-delete",
                        "git branch -d",
                        "git branch -D",
                        "git remote set-url",
                        "| tee ",
                        "sed -i",
                    ]
                    .iter()
                    .any(|needle| command.contains(needle))
                }),
            "write" | "edit" | "todo" => true,
            _ => false,
        }
    }

    fn is_mcp_tool(&self, name: &str) -> bool {
        self.mcp_permission_tools
            .contains(&name.to_ascii_lowercase())
    }

    fn tool_annotations(&self, _name: &str) -> Option<NativeToolAnnotations> {
        None
    }

    fn ensure_mcp_annotations<'a>(&'a self) -> NativeHostFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn inline_tool_approval_context(&self, _name: &str) -> Option<InlineToolApprovalContext> {
        None
    }

    fn is_explicit_inline_read_only_tool(&self, _name: &str) -> bool {
        false
    }

    fn credential_generation(&self) -> u64 {
        0
    }

    fn file_read_verdict(&self, _path: &str) -> NativeFirewallVerdict {
        NativeFirewallVerdict::Allow
    }

    fn video_mime(&self, _path: &Path) -> Option<(String, u64)> {
        None
    }

    fn extract_video_frames<'a>(
        &'a self,
        _path: &'a Path,
    ) -> NativeHostFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async { Err("video fixtures are owned by the TUI host".to_owned()) })
    }

    fn firewall_verdict(
        &self,
        _name: &str,
        _args: &Value,
        _workflow_state: &WorkflowStateSnapshot,
        _annotations: Option<&NativeToolAnnotations>,
        _external: bool,
    ) -> NativeFirewallVerdict {
        NativeFirewallVerdict::Allow
    }

    fn execute_tool<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        _event_tx: Option<&'a mpsc::UnboundedSender<FromAgent>>,
        call_id: &'a str,
        _options: NativeToolExecutionOptions<'a>,
    ) -> NativeHostFuture<'a, ToolExecution> {
        Box::pin(async move {
            self.assert_effect_pending(call_id);
            let execution = self.execution(call_id, name, args);
            if self.block_provider_after_tool {
                self.provider_admission_blocked
                    .store(true, Ordering::SeqCst);
            }
            execution
        })
    }

    fn execute_read_only_wave<'a>(
        &'a self,
        calls: &'a [NativeReadOnlyToolCall],
        _event_tx: &'a mpsc::UnboundedSender<FromAgent>,
        _cancel: Option<CancellationToken>,
    ) -> NativeHostFuture<'a, HashMap<String, ToolExecution>> {
        Box::pin(async move {
            let executions = calls
                .iter()
                .map(|call| {
                    self.assert_effect_pending(&call.call_id);
                    (
                        call.call_id.clone(),
                        self.execution(&call.call_id, &call.tool_name, &call.args),
                    )
                })
                .collect();
            if self.block_provider_after_tool {
                self.provider_admission_blocked
                    .store(true, Ordering::SeqCst);
            }
            executions
        })
    }

    fn clear_cache(&self) {}

    fn set_steer_signal(&self, _signal: Arc<SteerSignal>) {}

    fn reset_coding_turn(&self) {}

    fn set_subagent_parent_scope(&self, _scope: String) {}

    fn set_subagent_parent_model(&self, _model: String, _thinking: String) {}

    fn set_subagent_parent_requests(&self, _requests: Vec<String>) {}

    fn coding_completion(&self) -> Result<Option<NativeCodingCompletion>, String> {
        Ok(None)
    }

    fn shutdown_background_processes<'a>(&'a self) -> NativeHostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn hook_pre_tool_use<'a>(
        &'a self,
        _name: &'a str,
        _call_id: &'a str,
        _args: &'a Value,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_post_tool_use<'a>(
        &'a self,
        _name: &'a str,
        _call_id: &'a str,
        _args: &'a Value,
        _output: &'a str,
        _is_error: bool,
        _duration_ms: u64,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async {
            self.post_tool_context
                .as_ref()
                .map_or_else(Self::hook_result, |context| {
                    NativeHookResult::InjectContext {
                        context: context.clone(),
                    }
                })
        })
    }

    fn hook_eval_gate<'a>(
        &'a self,
        _name: &'a str,
        _call_id: &'a str,
        _args: &'a Value,
        _output: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_user_prompt_submit<'a>(
        &'a self,
        _prompt: &'a str,
        _attachment_count: u32,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_pre_message<'a>(
        &'a self,
        _message: &'a str,
        _attachments: &'a [String],
        _model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_pre_provider_request<'a>(
        &'a self,
        _kind: &'a str,
        _request_id: &'a str,
        _model: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        let blocked = self.provider_admission_blocked.load(Ordering::SeqCst);
        Box::pin(async move {
            if blocked {
                NativeHookResult::Block {
                    reason: "fixture admission denied after lease timeout".to_owned(),
                }
            } else {
                NativeHookResult::Continue
            }
        })
    }

    fn hook_post_message<'a>(
        &'a self,
        _response: &'a str,
        _input_tokens: u64,
        _output_tokens: u64,
        _duration_ms: u64,
        _stop_reason: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async {
            if let Some(barrier) = &self.checkpoint_barrier {
                if !barrier.2.swap(true, Ordering::SeqCst) {
                    barrier.0.notify_one();
                    barrier.1.notified().await;
                }
            }
            Self::hook_result()
        })
    }

    fn hook_on_error<'a>(
        &'a self,
        _error: &'a str,
        _error_kind: &'a str,
        _context: Option<&'a str>,
        _recoverable: bool,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_stop_failure<'a>(
        &'a self,
        _error: &'a str,
        _error_details: Option<&'a str>,
        _last_assistant_message: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_permission_request<'a>(
        &'a self,
        _name: &'a str,
        _call_id: &'a str,
        _args: &'a Value,
        _reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_handle_overflow<'a>(&'a self) -> NativeHostFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn hook_checkpoint_transcript_before_response<'a>(&'a self) -> NativeHostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn hook_session_id<'a>(&'a self) -> NativeHostFuture<'a, Option<String>> {
        Box::pin(async move {
            self.session_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
    }

    fn hook_set_session_context<'a>(
        &'a self,
        session_id: Option<String>,
        _transcript_path: Option<String>,
    ) -> NativeHostFuture<'a, ()> {
        Box::pin(async move {
            *self
                .session_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = session_id;
        })
    }

    fn tool_operation_admission(&self, name: &str, _args: &Value) -> NativeToolOperationAdmission {
        if self.replay_safe_tools.contains(&name.to_ascii_lowercase()) {
            NativeToolOperationAdmission {
                replay_policy: maestro_runtime_contracts::ToolReplayPolicy::Safe,
                idempotency_key: Some(format!("runtime-test:{name}")),
            }
        } else {
            NativeToolOperationAdmission::default()
        }
    }

    fn hook_record_tool_operation<'a>(
        &'a self,
        record: &'a maestro_runtime_contracts::ToolOperationRecord,
    ) -> NativeHostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let Some(records) = &self.tool_operation_records {
                records
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(record.clone());
            }
            Ok(())
        })
    }

    fn hook_load_tool_operations<'a>(
        &'a self,
    ) -> NativeHostFuture<'a, Result<Vec<maestro_runtime_contracts::ToolOperationRecord>, String>>
    {
        Box::pin(async move {
            let Some(records) = &self.tool_operation_records else {
                return Ok(Vec::new());
            };
            let records = records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let mut ledger = maestro_runtime_contracts::ToolOperationLedger::default();
            for record in records {
                ledger.apply(record).map_err(|error| error.to_string())?;
            }
            Ok(ledger.latest_records().cloned().collect())
        })
    }

    fn hook_on_session_start<'a>(
        &'a self,
        _reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_on_session_end<'a>(
        &'a self,
        _reason: &'a str,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
    }

    fn hook_increment_turn<'a>(&'a self) -> NativeHostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn hook_set_model<'a>(&'a self, _model: &'a str) -> NativeHostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn hook_set_log_file<'a>(&'a self, _path: Option<String>) -> NativeHostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn render_hook_context(
        &self,
        _event: NativeHookEvent,
        context: &str,
    ) -> Result<String, String> {
        Ok(context.to_owned())
    }

    fn model_allowed(&self, _model_id: &str) -> Option<String> {
        None
    }

    fn resolve_model(&self, _model_id: &str) -> Result<NativeResolvedClient, String> {
        Ok(NativeResolvedClient {
            client: Some(self.client.as_ref().clone()),
            provider_name: self.client.provider_name().to_owned(),
            model_route: NativeModelRoute::DirectProvider,
        })
    }

    fn default_max_output_tokens(&self, _model: &str) -> u32 {
        self.max_output_tokens
    }

    fn is_local_model(&self, model: &str) -> bool {
        model.starts_with("llamacpp/") || model.starts_with("local/")
    }

    fn model_capabilities(&self, model: &str) -> NativeModelCapabilities {
        self.model_capabilities
            .get(model)
            .copied()
            .unwrap_or_default()
    }

    fn model_context_window(&self, _model: &str) -> Option<u64> {
        Some(self.context_window)
    }

    fn validate_model_transition(&self, _from: &str, _to: &str) -> Result<(), String> {
        Ok(())
    }

    fn boost_choice(
        &self,
        current: &ModelChoice,
        config: &ModelDynamicsConfig,
    ) -> Option<ModelChoice> {
        Some(config.boost.clone().unwrap_or_else(|| ModelChoice {
            model: current.model.clone(),
            thinking: ThinkingLevel::High,
        }))
    }

    fn normalize_thinking(&self, _model: &str, requested: ThinkingLevel) -> ThinkingLevel {
        requested
    }

    fn codex_auth_context(&self) -> Result<NativeCodexAuth, String> {
        Err("Codex process fixtures belong to the TUI host".to_owned())
    }

    fn codex_auth_is_usable(&self, _path: &Path) -> bool {
        std::fs::read(_path)
            .map(|contents| !contents.is_empty())
            .unwrap_or(false)
    }

    fn report_diagnostic(&self, _message: String) {}

    fn clamp_tool_output(
        &self,
        content: &str,
        _tool_name: &str,
        _spill_dir: Option<&Path>,
    ) -> String {
        content.to_owned()
    }

    fn project_tool_output(
        &self,
        content: &str,
        _tool: &str,
        spill_dir: Option<&Path>,
    ) -> super::super::native_host::NativeToolOutput {
        let mut output = super::super::native_host::NativeToolOutput {
            content: content.to_owned(),
            saved_path: None,
        };
        if self.post_tool_context.is_some() && content.len() > 40_000 {
            let dir = spill_dir.expect("fixture must have a session-owned spill directory");
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join("full-output.txt");
            std::fs::write(&path, content).unwrap();
            output.content = format!(
                "{}\n[Truncated. Full output: {}. Read with offset and limit.]",
                &content[..1000],
                path.display()
            );
            output.saved_path = Some(path);
        }
        output
    }

    fn model_tool_spill_dir(&self, cwd: &str, session_id: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(cwd)
            .join(".maestro")
            .join(session_id)
    }

    fn open_todo_count(&self, output: &str) -> Option<usize> {
        serde_json::from_str::<Value>(output)
            .ok()
            .and_then(|value| value.get("open").and_then(Value::as_u64))
            .map(|value| value as usize)
    }

    fn semantic_conversation_protocol(&self) -> &str {
        "maestro.semantic-conversation.v1"
    }

    fn model_route(&self, _model_id: &str) -> NativeModelRoute {
        NativeModelRoute::DirectProvider
    }
}
pub(super) fn new_runtime_test_agent(
    config: NativeAgentConfig,
    client: UnifiedClient,
) -> Result<(super::NativeAgent, mpsc::UnboundedReceiver<FromAgent>)> {
    let host = RuntimeTestHost::new(config.cwd.clone(), client.clone())
        .with_code_authority(config.approval_mode != ApprovalMode::Selective);
    new_runtime_test_agent_with_host(config, host)
}

fn new_runtime_test_agent_with_host(
    config: NativeAgentConfig,
    host: RuntimeTestHost,
) -> Result<(super::NativeAgent, mpsc::UnboundedReceiver<FromAgent>)> {
    let client = host.client.as_ref().clone();
    let host = NativeExecutionHostHandle::new(Arc::new(host));

    let resolved = NativeResolvedClient {
        provider_name: client.provider_name().to_owned(),
        client: Some(client),
        model_route: NativeModelRoute::DirectProvider,
    };
    super::NativeAgent::start_with_resolved_client(
        config,
        host,
        Vec::new(),
        CredentialVault::new(),
        None,
        resolved,
    )
}

fn scripted_tool_turn(call_id: &str, tool_name: &str, input: Value) -> crate::ai::ScriptedResponse {
    crate::ai::ScriptedResponse {
        blocks: vec![crate::ai::ScriptedBlock::ToolUse {
            id: call_id.to_owned(),
            name: tool_name.to_owned(),
            input,
        }],
        stop_reason: crate::ai::StopReason::ToolUse,
        error: None,
    }
}

fn staged_tool_operation(
    call_id: &str,
    replay_policy: maestro_runtime_contracts::ToolReplayPolicy,
    outcome: Option<maestro_runtime_contracts::ToolOperationOutcome>,
) -> Vec<maestro_runtime_contracts::ToolOperationRecord> {
    let planned = maestro_runtime_contracts::ToolOperationRecord::planned(
        call_id,
        "read",
        serde_json::json!({"path": "recovery-fixture.txt"}),
        (replay_policy == maestro_runtime_contracts::ToolReplayPolicy::Safe)
            .then(|| format!("runtime-test:{call_id}")),
        replay_policy,
        1,
    )
    .expect("planned operation");
    let pending = planned
        .clone()
        .effect_pending(2)
        .expect("pending operation");
    let mut records = vec![planned, pending.clone()];
    if let Some(outcome) = outcome {
        records.push(pending.outcome_ready(outcome, 3).expect("ready operation"));
    }
    records
}

async fn wait_for_recovered_tool_result(
    events: &mut mpsc::UnboundedReceiver<FromAgent>,
    call_id: &str,
) -> (String, bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ConversationSnapshot { messages, .. }) => {
                    for message in messages {
                        let MessageContent::Blocks(blocks) = message.content else {
                            continue;
                        };
                        for block in blocks {
                            let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } = block
                            else {
                                continue;
                            };
                            if tool_use_id == call_id {
                                return (content, is_error.unwrap_or(false));
                            }
                        }
                    }
                }
                Some(FromAgent::Error {
                    message,
                    fatal: true,
                    ..
                }) => panic!("recovery failed: {message}"),
                Some(_) => {}
                None => panic!("agent event channel closed before recovery snapshot"),
            }
        }
    })
    .await
    .expect("recovery snapshot timeout")
}

async fn wait_for_turn_completed(events: &mut mpsc::UnboundedReceiver<FromAgent>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::TurnCompleted { .. }) => return,
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                })
                | Some(FromAgent::ProviderError { message, .. }) => {
                    panic!("runtime turn failed: {message}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before turn completion"),
            }
        }
    })
    .await
    .expect("runtime turn timeout");
}

#[tokio::test]
async fn native_tool_effect_is_staged_before_execution_and_completed_after_projection() {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = Arc::new(Mutex::new(Vec::new()));
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/durable-operation",
        vec![
            scripted_tool_turn(
                "call-durable-read",
                "read",
                serde_json::json!({"path": "fixture.txt"}),
            ),
            crate::ai::ScriptedResponse::text("done"),
        ],
    );
    let config = NativeAgentConfig {
        model: "runtime-test/durable-operation".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let host = RuntimeTestHost::new(config.cwd.clone(), UnifiedClient::Scripted(scripted))
        .with_tool_operation_journal(Arc::clone(&journal));
    let executions = Arc::clone(&host.completed_tool_executions);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).expect("agent");

    agent
        .prompt("read the fixture".to_owned(), Vec::new())
        .await
        .expect("prompt");
    wait_for_turn_completed(&mut events).await;
    agent.shutdown().await;

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let records = journal.lock().unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.call_id == "call-durable-read")
            .map(|record| record.phase)
            .collect::<Vec<_>>(),
        vec![
            maestro_runtime_contracts::ToolOperationPhase::Planned,
            maestro_runtime_contracts::ToolOperationPhase::EffectPending,
            maestro_runtime_contracts::ToolOperationPhase::OutcomeReady,
            maestro_runtime_contracts::ToolOperationPhase::Completed,
        ]
    );
    assert!(
        records
            .iter()
            .find(|record| {
                record.call_id == "call-durable-read"
                    && record.phase == maestro_runtime_contracts::ToolOperationPhase::OutcomeReady
            })
            .and_then(|record| record.outcome.as_ref())
            .and_then(|outcome| outcome.receipt.as_ref())
            .is_some(),
        "the staged outcome must retain its governed execution receipt"
    );
}

#[tokio::test]
async fn outcome_ready_recovery_materializes_without_reexecution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = Arc::new(Mutex::new(staged_tool_operation(
        "call-ready",
        maestro_runtime_contracts::ToolReplayPolicy::Never,
        Some(maestro_runtime_contracts::ToolOperationOutcome::new(
            "persisted result",
            false,
            None,
        )),
    )));
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/outcome-ready",
        vec![crate::ai::ScriptedResponse::text("barrier")],
    );
    let config = NativeAgentConfig {
        model: "runtime-test/outcome-ready".to_owned(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let host = RuntimeTestHost::new(config.cwd.clone(), UnifiedClient::Scripted(scripted))
        .with_tool_operation_journal(Arc::clone(&journal));
    let executions = Arc::clone(&host.completed_tool_executions);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).expect("agent");

    agent
        .set_session_context(Some("ready-session".into()), "restore", false)
        .expect("session context");
    let (content, is_error) = wait_for_recovered_tool_result(&mut events, "call-ready").await;
    assert_eq!(content, "[tool result omitted from checkpoint]");
    assert!(!is_error);
    agent
        .prompt("barrier".to_owned(), Vec::new())
        .await
        .expect("barrier prompt");
    wait_for_turn_completed(&mut events).await;
    agent.shutdown().await;

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let records = journal.lock().unwrap();
    assert_eq!(
        records.last().map(|record| record.phase),
        Some(maestro_runtime_contracts::ToolOperationPhase::Completed)
    );
    assert_eq!(
        records
            .last()
            .and_then(|record| record.outcome.as_ref())
            .map(|outcome| outcome.content.as_str()),
        Some("persisted result")
    );
}

#[tokio::test]
async fn safe_pending_recovery_replays_once_from_admitted_arguments() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("recovery-fixture.txt"),
        "replayed fixture",
    )
    .expect("fixture");
    let journal = Arc::new(Mutex::new(staged_tool_operation(
        "call-safe-pending",
        maestro_runtime_contracts::ToolReplayPolicy::Safe,
        None,
    )));
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/safe-recovery",
        vec![crate::ai::ScriptedResponse::text("barrier")],
    );
    let config = NativeAgentConfig {
        model: "runtime-test/safe-recovery".to_owned(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let host = RuntimeTestHost::new(config.cwd.clone(), UnifiedClient::Scripted(scripted))
        .with_tool_operation_journal(Arc::clone(&journal))
        .with_replay_safe_tool("read");
    let executions = Arc::clone(&host.completed_tool_executions);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).expect("agent");

    agent
        .set_session_context(Some("safe-session".into()), "restore", false)
        .expect("session context");
    let (content, is_error) =
        wait_for_recovered_tool_result(&mut events, "call-safe-pending").await;
    assert_eq!(content, "[tool result omitted from checkpoint]");
    assert!(!is_error);
    agent
        .set_session_context(Some("safe-session".into()), "restore-again", false)
        .expect("repeat session context");
    agent
        .prompt("barrier".to_owned(), Vec::new())
        .await
        .expect("barrier prompt");
    wait_for_turn_completed(&mut events).await;
    agent.shutdown().await;

    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "a completed replay must be idempotent across repeated restores"
    );
    let records = journal.lock().unwrap();
    assert_eq!(
        records.last().map(|record| record.phase),
        Some(maestro_runtime_contracts::ToolOperationPhase::Completed)
    );
    assert_eq!(
        records
            .last()
            .and_then(|record| record.outcome.as_ref())
            .map(|outcome| outcome.content.as_str()),
        Some("replayed fixture")
    );
}

#[tokio::test]
async fn never_pending_recovery_records_unknown_outcome_without_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let journal = Arc::new(Mutex::new(staged_tool_operation(
        "call-never-pending",
        maestro_runtime_contracts::ToolReplayPolicy::Never,
        None,
    )));
    let config = NativeAgentConfig {
        model: "runtime-test/never-recovery".to_owned(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let host = RuntimeTestHost::new(
        config.cwd.clone(),
        UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
            "runtime-test/never-recovery",
            Vec::new(),
        )),
    )
    .with_tool_operation_journal(Arc::clone(&journal));
    let executions = Arc::clone(&host.completed_tool_executions);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).expect("agent");

    agent
        .set_session_context(Some("never-session".into()), "restore", false)
        .expect("session context");
    let (content, is_error) =
        wait_for_recovered_tool_result(&mut events, "call-never-pending").await;
    agent.shutdown().await;

    assert!(is_error);
    assert_eq!(content, "[tool result omitted from checkpoint]");
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let records = journal.lock().unwrap();
    assert_eq!(
        records.last().map(|record| record.phase),
        Some(maestro_runtime_contracts::ToolOperationPhase::Completed)
    );
    assert!(
        records
            .last()
            .and_then(|record| record.outcome.as_ref())
            .is_some_and(|outcome| {
                outcome.is_error
                    && outcome.receipt.is_none()
                    && outcome.content.contains("Unknown outcome")
            })
    );
}

#[tokio::test]
async fn blocked_provider_admission_never_opens_or_retries_a_direct_request() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/admission",
        vec![crate::ai::ScriptedResponse::text("must not be consumed")],
    );
    let client = UnifiedClient::Scripted(scripted.clone());
    let config = NativeAgentConfig {
        model: "runtime-test/admission".to_owned(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let host =
        RuntimeTestHost::new(config.cwd.clone(), client).with_provider_admission_blocked(true);
    let (agent, mut events) =
        new_runtime_test_agent_with_host(config, host).expect("blocked-admission test agent");

    agent
        .prompt("this provider call must be denied".to_owned(), Vec::new())
        .await
        .expect("prompt queued");
    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("a blocked provider request must not complete")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before admission denial"),
            }
        }
    })
    .await
    .expect("admission denial event");
    assert!(terminal.contains("provider admission denied"), "{terminal}");
    assert_eq!(
        scripted.remaining(),
        1,
        "denial must happen before the provider stream and outer retry loop"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn provider_admission_blocks_the_next_round_after_tool_completion() {
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/round-admission",
        vec![
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "call-before-admission-block".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({"path": "Cargo.toml"}),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::text("must remain queued after admission denial"),
        ],
    );
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let config = NativeAgentConfig {
        model: "scripted/round-admission".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let host = RuntimeTestHost::new(
        config.cwd.clone(),
        UnifiedClient::Scripted(scripted.clone()),
    )
    .with_provider_admission_blocked_after_tool();
    let completed_tool_executions = Arc::clone(&host.completed_tool_executions);
    let (agent, mut events) =
        new_runtime_test_agent_with_host(config, host).expect("round-admission test agent");

    agent
        .prompt("run the first tool round".to_owned(), Vec::new())
        .await
        .expect("prompt queued");
    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("the second provider round must be denied")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before admission denial"),
            }
        }
    })
    .await
    .expect("second-round admission denial");

    assert_eq!(
        completed_tool_executions.load(Ordering::SeqCst),
        1,
        "one actual host tool execution must complete before the gate flips: {terminal}"
    );
    assert!(terminal.contains("provider admission denied"), "{terminal}");
    assert_eq!(
        scripted.remaining(),
        1,
        "the denied second round must not consume or retry the next provider response"
    );
    agent.shutdown().await;
}

#[test]
fn provider_request_ids_are_stable_and_change_with_identity_inputs() {
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::text("same logical request"),
    }];
    let first = provider_request_id("primary", "model-a", &messages).expect("request id");
    assert_eq!(
        first,
        provider_request_id("primary", "model-a", &messages).expect("same request id")
    );
    assert_ne!(
        first,
        provider_request_id("side_question", "model-a", &messages).expect("kind request id")
    );
    assert_ne!(
        first,
        provider_request_id("primary", "model-b", &messages).expect("model request id")
    );
    let changed_messages = vec![Message {
        role: Role::User,
        content: MessageContent::text("different logical request"),
    }];
    assert_ne!(
        first,
        provider_request_id("primary", "model-a", &changed_messages).expect("history request id")
    );
}

fn runtime_test_host_handle() -> NativeExecutionHostHandle {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/fixture",
        vec![crate::ai::ScriptedResponse::text("fixture response")],
    ));
    NativeExecutionHostHandle::new(Arc::new(RuntimeTestHost::new(".", client)))
}

fn runtime_policy_host_handle() -> NativeExecutionHostHandle {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/fixture",
        vec![crate::ai::ScriptedResponse::text("fixture response")],
    ));
    NativeExecutionHostHandle::new(Arc::new(
        RuntimeTestHost::new(".", client).with_code_authority(false),
    ))
}

fn runtime_sandbox_host_handle() -> NativeExecutionHostHandle {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/fixture",
        vec![crate::ai::ScriptedResponse::text("fixture response")],
    ));
    NativeExecutionHostHandle::new(Arc::new(
        RuntimeTestHost::new(".", client).with_sandbox_policy(true),
    ))
}

fn runtime_catalog_host_handle(
    max_output_tokens: u32,
    context_window: u64,
) -> NativeExecutionHostHandle {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/fixture",
        vec![crate::ai::ScriptedResponse::text("fixture response")],
    ));
    NativeExecutionHostHandle::new(Arc::new(
        RuntimeTestHost::new(".", client).with_model_limits(max_output_tokens, context_window),
    ))
}

/// Compatibility wrapper for the old inline tests.  The production runtime
/// constructors that guessed a concrete TUI host intentionally fail closed;
/// this wrapper supplies the deterministic host above while keeping the test
/// call sites focused on the actor behavior they exercise.
pub(super) struct NativeAgent(super::NativeAgent);

impl std::ops::Deref for NativeAgent {
    type Target = super::NativeAgent;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl NativeAgent {
    fn with_client(
        config: NativeAgentConfig,
        client: UnifiedClient,
        external_tool_definitions: Vec<ToolDefinition>,
        allowed_tools: Option<&HashSet<String>>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<FromAgent>)> {
        if external_tool_definitions.is_empty() && allowed_tools.is_none() {
            return new_runtime_test_agent(config, client)
                .map(|(agent, events)| (Self(agent), events));
        }
        let host = RuntimeTestHost::new(config.cwd.clone(), client.clone())
            .with_code_authority(config.approval_mode != ApprovalMode::Selective);
        let host = NativeExecutionHostHandle::new(Arc::new(host));
        let resolved = NativeResolvedClient {
            provider_name: client.provider_name().to_owned(),
            client: Some(client),
            model_route: NativeModelRoute::DirectProvider,
        };
        super::NativeAgent::start_with_resolved_client(
            config,
            host,
            external_tool_definitions,
            CredentialVault::new(),
            allowed_tools,
            resolved,
        )
        .map(|(agent, events)| (Self(agent), events))
    }

    fn new_with_test_client(
        config: NativeAgentConfig,
        client: UnifiedClient,
    ) -> Result<(Self, mpsc::UnboundedReceiver<FromAgent>)> {
        Self::with_client(config, client, Vec::new(), None)
    }

    pub(super) fn new_with_external_tools(
        config: NativeAgentConfig,
        external_tool_definitions: Vec<ToolDefinition>,
        allowed_tools: Option<&HashSet<String>>,
        client: UnifiedClient,
    ) -> Result<(Self, mpsc::UnboundedReceiver<FromAgent>)> {
        Self::with_client(config, client, external_tool_definitions, allowed_tools)
    }

    pub(super) async fn shutdown(self) {
        self.0.shutdown().await;
    }
}

pub(super) fn external_tool_definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        tool: Tool::new(name, "Caller-owned test tool").with_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false
        })),
        requires_approval: true,
    }
}

#[test]
fn external_tool_names_reject_duplicates_and_reserved_names() {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/tool-validation",
        Vec::new(),
    ));
    let mut host = RuntimeTestHost::new(".", client);
    host.reserved_tools.insert("runtime_reserved".to_owned());
    let host = NativeExecutionHostHandle::new(Arc::new(host));

    let duplicate = validate_tools_with_host(
        &host,
        None,
        &[
            external_tool_definition("caller_tool"),
            external_tool_definition("CALLER_TOOL"),
        ],
    )
    .expect_err("case-insensitive duplicate external names must be rejected");
    assert!(duplicate.to_string().contains("multiple owners"));

    let host_collision = validate_tools_with_host(&host, None, &[external_tool_definition("BASH")])
        .expect_err("case-insensitive host tool collision must be rejected");
    assert!(
        host_collision
            .to_string()
            .contains("host, MCP, or reserved tool")
    );

    let reserved =
        validate_tools_with_host(&host, None, &[external_tool_definition("RUNTIME_RESERVED")])
            .expect_err("reserved external name must be rejected");
    assert!(reserved.to_string().contains("host, MCP, or reserved tool"));
}

#[test]
fn ungoverned_external_tool_cannot_claim_dynamic_mcp_tool_with_remembered_grant() {
    let workspace = tempfile::tempdir().expect("workspace");
    let name = "mcp__project__apply";
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/mcp-collision",
        Vec::new(),
    ));
    let config = NativeAgentConfig {
        model: "runtime-test/mcp-collision".to_owned(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let mut host = RuntimeTestHost::new(config.cwd.clone(), client.clone());
    host.mcp_permission_tools.insert(name.to_owned());
    assert!(
        host.is_mcp_tool(name) && host.mcp_permission_allows(name),
        "fixture must model a dynamic MCP tool with a remembered grant"
    );
    let executions = Arc::clone(&host.completed_tool_executions);
    let result = super::NativeAgent::start_with_resolved_client(
        config,
        NativeExecutionHostHandle::new(Arc::new(host)),
        vec![external_tool_definition("MCP__PROJECT__APPLY")],
        CredentialVault::new(),
        None,
        NativeResolvedClient {
            provider_name: client.provider_name().to_owned(),
            client: Some(client),
            model_route: NativeModelRoute::DirectProvider,
        },
    );

    let error = match result {
        Ok(_) => panic!("external tool must not overwrite a host MCP tool"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("host, MCP, or reserved tool"));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "rejected external tools must never reach host dispatch"
    );
}

#[test]
fn closed_tool_response_is_typed_and_not_retried_as_a_deterministic_failure() {
    let error = closed_tool_response_failure("call_closed");
    let failure = error
        .downcast_ref::<ProviderStreamFailure>()
        .expect("closed approval channel must preserve its typed failure");
    assert_eq!(failure.kind, ProviderStreamErrorKind::TransientProtocol);
    assert!(failure.message.contains("call_closed"));

    let mut retry_policy = super::super::retry::RetryPolicy::default();
    let error_kind = super::super::retry::ErrorKind::classify(&format!("{error:#}"));
    assert!(matches!(
        retry_policy.should_retry(error_kind),
        super::super::retry::RetryDecision::GiveUp { .. }
    ));
}

#[test]
fn exhausted_provider_stream_does_not_consume_the_outer_request_retry_budget() {
    let mut retry_policy = super::super::retry::RetryPolicy::default();
    let transient = super::super::retry::ErrorKind::Transient;

    assert!(matches!(
        request_retry_decision(
            &mut retry_policy,
            transient,
            RequestFailureOwner::ProviderStream,
        ),
        super::super::retry::RetryDecision::GiveUp { reason }
            if reason.contains("stream retry policy")
    ));

    assert!(matches!(
        request_retry_decision(&mut retry_policy, transient, RequestFailureOwner::Request,),
        super::super::retry::RetryDecision::Retry { attempt: 1, .. }
    ));
}

#[tokio::test]
async fn managed_gateway_retry_open_failures_emit_one_terminal_without_a_fourth_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock managed gateway");
    let address = listener.local_addr().expect("mock gateway address");
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for attempt in 1..=3 {
            let (mut stream, _) = listener.accept().await.expect("gateway request");
            let _ = read_scripted_provider_request(&mut stream).await;
            server_requests.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                let body = r#"{"error":{"type":"server_error","message":"operation timed out"}}"#;
                let wire = format!(
                    "HTTP/1.1 504 Gateway Timeout\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream
                    .write_all(wire.as_bytes())
                    .await
                    .expect("gateway timeout response");
            }
            // Later attempts model the production response-open failure:
            // the gateway accepted the request but closed before headers.
        }
    });

    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "evalops/openai/gpt-5.6-terra".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::from_model_with_env(
        "evalops/openai/gpt-5.6-terra",
        &HashMap::from([
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
                "delegated-token".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_BASE_URL".to_string(),
                format!("http://{address}/v1"),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".to_string(), "org-test".to_string()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".to_string(),
                "workspace-test".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_PROVIDER".to_string(),
                "openrouter".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_ENVIRONMENT".to_string(),
                "production".to_string(),
            ),
        ]),
    )
    .expect("managed gateway client");
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("hosted agent");
    agent
        .set_session_context(Some("managed-retry-session".to_owned()), "test", false)
        .expect("session context");

    agent
        .prompt("return a terminal outcome".to_owned(), vec![])
        .await
        .expect("hosted prompt");
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ProviderError { kind, message }) => break (kind, message),
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("failed gateway turn must not complete successfully")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before provider terminal"),
            }
        }
    })
    .await
    .expect("provider terminal timeout");
    agent.shutdown().await;
    server.await.expect("mock gateway server");

    assert_eq!(terminal.0, ProviderStreamErrorKind::TransientProtocol);
    assert!(terminal.1.contains("gateway response") || terminal.1.contains("request"));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "the managed stream budget is exactly three total requests; the native outer loop must not start a fourth"
    );
}

#[tokio::test]
async fn native_agent_projects_managed_gateway_receipt_without_signed_payload() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock managed gateway");
    let address = listener.local_addr().expect("mock gateway address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("managed request");
        let request = read_scripted_provider_request(&mut stream).await;
        let body = chat_sse_response("resp_managed", "ok", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Request-ID: request-native\r\nX-EvalOps-Record-ID: record-native\r\nX-EvalOps-Lineage-ID: lineage-native\r\nX-EvalOps-Record-Status: planned\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream
            .write_all(wire.as_bytes())
            .await
            .expect("managed response");
        request
    });

    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "evalops/openai/gpt-5.6-terra".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::from_model_with_env(
        "evalops/openai/gpt-5.6-terra",
        &HashMap::from([
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
                "delegated-token".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_BASE_URL".to_string(),
                format!("http://{address}/v1"),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".to_string(), "org-test".to_string()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".to_string(),
                "workspace-test".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_PROVIDER".to_string(),
                "openrouter".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_ENVIRONMENT".to_string(),
                "production".to_string(),
            ),
        ]),
    )
    .expect("managed gateway client");
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("hosted agent");
    let authorization = serde_json::json!({
        "claims": {
            "endpoint": "chat.completions",
            "lineage_id": "lineage-native",
            "session_id": "session-native",
            "thread_id": "thread-native",
            "run_id": "run-native",
            "turn_id": "turn-native",
            "model": "openai/gpt-5.6-terra",
            "providerCandidates": [{
                "provider": "openrouter",
                "environment": "production",
                "credentialName": "default",
                "teamId": "",
                "model": "openai/gpt-5.6-terra"
            }],
            "routing": "ordered",
            "output_token_budget": {
                "value": 4096,
                "origin": "route_policy",
                "origin_reference": "maestro-native-test"
            }
        },
        "signature": "signature-marker"
    })
    .to_string();

    agent
        .prompt_with_kind_and_managed_context(
            "managed prompt".to_string(),
            Vec::new(),
            PromptKind::Prompt,
            None,
            Some("lineage-native".to_string()),
            Some(crate::agent::ManagedInferenceAuthorization::new(
                authorization,
            )),
        )
        .await
        .expect("managed prompt");

    let mut saw_receipt = false;
    let mut saw_provider_content = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(receipt @ FromAgent::ManagedGatewayReceipt { .. }) => {
                    assert!(!saw_provider_content);
                    let serialized =
                        serde_json::to_string(&receipt).expect("serialize native managed receipt");
                    assert!(!serialized.contains("managed_inference_authorization"));
                    assert!(!serialized.contains("signature-marker"));
                    assert!(!serialized.contains("managed prompt"));
                    assert!(!serialized.contains("provider_ref"));
                    assert!(!serialized.contains("raw_body"));
                    let FromAgent::ManagedGatewayReceipt {
                        request_id,
                        record_id,
                        lineage_id,
                        record_status,
                        ..
                    } = receipt
                    else {
                        unreachable!("matched managed receipt")
                    };
                    assert_eq!(request_id, "request-native");
                    assert_eq!(record_id, "record-native");
                    assert_eq!(lineage_id, "lineage-native");
                    assert_eq!(record_status, "planned");
                    saw_receipt = true;
                }
                Some(FromAgent::ResponseChunk { .. }) => {
                    assert!(saw_receipt, "receipt must precede provider content");
                    saw_provider_content = true;
                }
                Some(FromAgent::TurnCompleted { .. }) => break,
                Some(FromAgent::ProviderError { kind, .. }) => {
                    panic!("managed request unexpectedly failed with {kind:?}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before turn completion"),
            }
        }
    })
    .await
    .expect("managed turn timeout");
    agent.shutdown().await;

    assert!(saw_receipt);
    assert!(saw_provider_content);
    let request = server.await.expect("mock gateway server");
    assert!(
        request
            .get("managed_inference_authorization")
            .and_then(|value| value.pointer("/claims/lineage_id"))
            .and_then(serde_json::Value::as_str)
            == Some("lineage-native"),
        "native client must forward the opaque authorization"
    );
    assert!(
        request
            .get("managed_inference_authorization")
            .and_then(|value| value.get("signature"))
            .and_then(serde_json::Value::as_str)
            == Some("signature-marker"),
        "native client must preserve the signed authorization"
    );
    assert_eq!(
        request.get("managed_inference_context"),
        Some(&serde_json::json!({
            "session_id": "session-native",
            "thread_id": "thread-native",
            "run_id": "run-native",
            "turn_id": "turn-native"
        })),
        "native client must bind the request to the signed turn context"
    );
    assert_eq!(
        request.get("provider_candidates"),
        Some(&serde_json::json!([{
            "model": "openai/gpt-5.6-terra",
            "provider_ref": {
                "provider": "openrouter",
                "environment": "production",
                "credential_name": "default",
                "team_id": ""
            }
        }])),
        "native client must project the signed provider candidates"
    );
    assert!(
        request.get("provider_ref").is_none(),
        "the unsigned fallback provider must not accompany a signed route"
    );
}

#[tokio::test]
async fn managed_tool_continuation_uses_fresh_invocation_authority() {
    assert_managed_invocation_renewal(false).await;
}

#[tokio::test]
async fn managed_transport_retry_and_tool_continuation_use_fresh_authority() {
    assert_managed_invocation_renewal(true).await;
}

async fn assert_managed_invocation_renewal(fail_first_open: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        let mut consumed = HashSet::new();
        let mut successful_invocations = 0;
        while let Ok((mut stream, _)) = listener.accept().await {
            let request = read_scripted_provider_request(&mut stream).await;
            let authorization =
                request["managed_inference_authorization"]["claims"]["authorization_id"]
                    .as_str()
                    .expect("invocation authority")
                    .to_owned();
            let first_attempt = consumed.is_empty();
            let fresh = consumed.insert(authorization);
            captured.lock().unwrap().push(request);
            let (status, content_type, body) = if fresh && first_attempt && fail_first_open {
                ("503 Service Unavailable", "application/json", serde_json::json!({"error": {
                    "code": "provider_unavailable", "message": "injected failure after admission"
                }}).to_string())
            } else if fresh {
                let first = successful_invocations == 0;
                successful_invocations += 1;
                (
                    "200 OK",
                    "text/event-stream",
                    chat_sse_response("managed-round", if first { "" } else { "done" }, first),
                )
            } else {
                (
                    "409 Conflict",
                    "application/json",
                    serde_json::json!({"error": {
                        "code": "managed_authorization_replay",
                        "message": "managed inference authorization has already been consumed"
                    }})
                    .to_string(),
                )
            };
            let wire = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nX-Request-ID: request-round\r\nX-EvalOps-Record-ID: record-round\r\nX-EvalOps-Lineage-ID: lineage-round\r\nX-EvalOps-Record-Status: planned\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            if stream.write_all(wire.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "evalops/openai/gpt-5.6-terra".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::from_model_with_env(
        &config.model,
        &HashMap::from([
            ("MAESTRO_EVALOPS_ACCESS_TOKEN".into(), "test-token".into()),
            (
                "MAESTRO_EVALOPS_BASE_URL".into(),
                format!("http://{address}/v1"),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".into(), "org-test".into()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".into(),
                "workspace-test".into(),
            ),
            ("MAESTRO_EVALOPS_PROVIDER".into(), "openrouter".into()),
            ("MAESTRO_EVALOPS_ENVIRONMENT".into(), "production".into()),
        ]),
    )
    .unwrap();
    let host = RuntimeTestHost::new(config.cwd.clone(), client);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    let authorization = serde_json::json!({
        "claims": {
            "authorization_id": "initial-invocation",
            "endpoint": "chat.completions",
            "lineage_id": "lineage-round",
            "session_id": "session-round", "thread_id": "thread-round",
            "run_id": "run-round", "turn_id": "turn-round",
            "model": "openai/gpt-5.6-terra",
            "providerCandidates": [{"provider": "openrouter", "environment": "production",
                "credentialName": "default", "teamId": "", "model": "openai/gpt-5.6-terra"}],
            "routing": "ordered",
            "output_token_budget": {"value": 4096, "origin": "route_policy",
                "origin_reference": "managed-round-test"}
        },
        "signature": "test-signature"
    });
    agent
        .prompt_with_kind_and_managed_context(
            "read then answer".into(),
            Vec::new(),
            PromptKind::Prompt,
            None,
            Some("lineage-round".into()),
            Some(ManagedInferenceAuthorization::new(
                authorization.to_string(),
            )),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ManagedAuthorizationRequest { request_id }) => {
                    let mut renewed = authorization.clone();
                    renewed["claims"]["authorization_id"] = request_id.clone().into();
                    agent
                        .managed_authorization_coordinator()
                        .respond(
                            &request_id,
                            ManagedInferenceAuthorization::new(renewed.to_string()),
                        )
                        .unwrap();
                }
                Some(FromAgent::TurnCompleted { .. }) => break Ok(()),
                Some(FromAgent::ProviderError { message, .. }) => break Err(message),
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break Err(message),
                Some(_) => {}
                None => break Err("agent closed before completing the continuation".into()),
            }
        }
    })
    .await;
    agent.shutdown().await;
    server.abort();
    let _ = server.await;
    assert!(result.is_ok(), "managed continuation timed out");
    assert_eq!(
        result.unwrap(),
        Ok(()),
        "a tool continuation needs fresh authority"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        if fail_first_open { 3 } else { 2 }
    );
}

#[tokio::test]
async fn auth_refresh_waiter_resumes_only_after_usable_credentials_change() {
    let root = tempfile::tempdir().expect("auth root");
    let auth_path = root.path().join("auth.json");
    let writer_path = auth_path.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::fs::write(
            writer_path,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"refreshed"}"#,
        )
        .await
        .expect("write refreshed auth");
    });
    let host = runtime_test_host_handle();
    let resumed = wait_for_codex_auth_refresh(
        &host,
        &auth_path,
        &CancellationToken::new(),
        &CancellationToken::new(),
        Duration::from_secs(1),
    )
    .await;
    writer.await.unwrap();
    assert!(resumed);
}

#[test]
fn codex_completion_usage_maps_nested_app_server_payload() {
    let usage = codex_token_usage_from_completion(&json!({
        "event": {
            "turn": {
                "usage": {
                    "inputTokens": 123,
                    "outputTokens": 45,
                    "inputTokensDetails": {"cachedTokens": 7},
                    "cacheWriteTokens": 2,
                    "cost": 0.125
                }
            }
        }
    }))
    .expect("nested Codex usage");

    assert_eq!(usage.input_tokens, 123);
    assert_eq!(usage.output_tokens, 45);
    assert_eq!(usage.cache_read_tokens, 7);
    assert_eq!(usage.cache_write_tokens, 2);
    assert_eq!(usage.cost, Some(0.125));
}

#[test]
fn goal_tools_visibility_tracks_update_goal_results() {
    for (status, expected) in [
        ("active", true),
        ("paused", true),
        ("blocked", true),
        ("complete", false),
    ] {
        let execution = ToolExecution::from_legacy(
            "call-1",
            "update_goal",
            ExecutionSource::Native,
            ToolResult::success(serde_json::json!({"goal": {"status": status}}).to_string()),
        );
        assert_eq!(
            goal_tools_visible_from_execution(&execution),
            Some(expected),
            "unexpected visibility for goal status {status}"
        );
    }

    let failed = ToolExecution::from_legacy(
        "call-2",
        "update_goal",
        ExecutionSource::Native,
        ToolResult::failure("goal update failed"),
    );
    assert_eq!(goal_tools_visible_from_execution(&failed), None);

    let malformed = ToolExecution::from_legacy(
        "call-3",
        "update_goal",
        ExecutionSource::Native,
        ToolResult::success("not json"),
    );
    assert_eq!(goal_tools_visible_from_execution(&malformed), None);
}

#[test]
fn codex_wire_results_resolve_vaulted_credentials_without_mutating_input() {
    let vault = CredentialVault::new();
    let reference = vault.store(
        "child-discovered-secret",
        crate::agent::CredentialType::Secret,
    );
    let vaulted = format!("child result: {reference}");

    let response = resolve_codex_tool_result_for_wire(&vault, &vaulted);

    assert_eq!(response, "child result: child-discovered-secret");
    assert_eq!(vaulted, format!("child result: {reference}"));
}

#[tokio::test]
async fn injected_user_note_acknowledges_history_application() {
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: tempfile::tempdir()
            .expect("workspace")
            .path()
            .display()
            .to_string(),
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", "http://127.0.0.1:1/v1")
            .expect("test client"),
    );
    let (agent, _events) = NativeAgent::new_with_test_client(config, client).expect("native agent");

    let (applied, mut consumed) = agent
        .inject_user_note("Subagent child-1 completed.")
        .expect("queue user note");
    tokio::time::timeout(std::time::Duration::from_secs(1), applied)
        .await
        .expect("agent note application timeout")
        .expect("agent note application acknowledgement");
    assert!(matches!(
        consumed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    agent.shutdown().await;
}

#[tokio::test]
async fn overflowing_pending_note_reaches_provider_before_consumption_ack() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        request_tx
            .send(read_scripted_provider_request(&mut stream).await)
            .unwrap();
        release_rx.await.unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().await.unwrap();
        request_tx
            .send(read_scripted_provider_request(&mut stream).await)
            .unwrap();
        let response = chat_sse_response("note-delivery", "Done.", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream.write_all(wire.as_bytes()).await.unwrap();
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(1024),
        ..Default::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, mut events) =
        new_runtime_test_agent_with_host(config.clone(), RuntimeTestHost::new(config.cwd, client))
            .unwrap();
    let note = (0..2048)
        .map(|i| format!("unseen-note-{i} "))
        .collect::<String>();
    let (applied, mut consumed) = agent.inject_user_note(note.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), applied)
        .await
        .unwrap()
        .unwrap();
    agent
        .prompt("Acknowledge the note.".into(), vec![])
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(10), request_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"] == note.trim())
    );
    assert!(matches!(
        consumed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    agent.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if matches!(event, FromAgent::TurnInterrupted { .. }) {
                return;
            }
        }
        panic!("missing interruption");
    })
    .await
    .unwrap();
    assert!(matches!(
        consumed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    release_tx.send(()).unwrap();
    agent
        .prompt("Retry the note.".into(), vec![])
        .await
        .unwrap();
    let retry = tokio::time::timeout(Duration::from_secs(10), request_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        retry["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"] == note.trim())
    );
    tokio::time::timeout(Duration::from_secs(10), consumed)
        .await
        .unwrap()
        .unwrap();
    agent.shutdown().await;
    server.await.unwrap();
}

#[test]
fn codex_app_server_turn_includes_trailing_injected_notes() {
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: MessageContent::text("Previous response"),
        },
        Message {
            role: Role::User,
            content: MessageContent::text("Cancelled instruction."),
        },
        Message {
            role: Role::User,
            content: MessageContent::text("Subagent child-1 completed."),
        },
        Message {
            role: Role::User,
            content: MessageContent::text("Continue the task."),
        },
    ];

    assert_eq!(
        codex_app_server_user_text(
            &messages,
            &["Subagent child-1 completed.".to_string()],
            Some(3),
        ),
        "Subagent child-1 completed.\n\nContinue the task."
    );
}

pub(super) async fn read_scripted_provider_request(
    stream: &mut tokio::net::TcpStream,
) -> serde_json::Value {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "provider request closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header end");
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .find_map(|(_, value)| value.trim().parse::<usize>().ok())
        .expect("content length");
    let body_start = header_end + 4;
    while buffer.len() - body_start < content_length {
        let read = stream.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "provider request closed before body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&buffer[body_start..body_start + content_length])
        .expect("provider request json")
}

pub(super) fn chat_sse_response(id: &str, content: &str, tool_call: bool) -> String {
    let mut events = vec![serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": content}, "finish_reason": null}]
    })];
    if tool_call {
        events.push(serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-4o", "choices": [{"index": 0,
                "delta": {"tool_calls": [{"index": 0, "id": "call-native-1", "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"Cargo.toml\"}"}}]},
                "finish_reason": "tool_calls"}]
        }));
    } else {
        events.push(serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-4o", "choices": [{"index": 0,
                "delta": {}, "finish_reason": "stop"}]
        }));
    }
    let mut body = String::new();
    for event in events {
        write!(body, "data: {event}\n\n").expect("write SSE event");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

async fn scripted_single_turn_provider() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("provider accept");
        let request = read_scripted_provider_request(&mut stream).await;
        captured.lock().unwrap().push(request);
        let response = chat_sse_response("hosted-fast-final", "The hosted turn completed.", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream
            .write_all(wire.as_bytes())
            .await
            .expect("provider response");
    });
    (format!("http://{address}/v1"), requests)
}

async fn scripted_managed_single_turn_provider() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind managed provider");
    let address = listener.local_addr().expect("managed provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("managed provider accept");
        let request = read_scripted_provider_request(&mut stream).await;
        let lineage_id = request["lineage_id"]
            .as_str()
            .expect("managed request lineage")
            .to_owned();
        captured.lock().unwrap().push(request);
        let response = chat_sse_response(
            "managed-hosted-fast-final",
            "The managed turn completed.",
            false,
        );
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\nX-Request-ID: managed-request\r\nX-EvalOps-Record-ID: managed-record\r\nX-EvalOps-Lineage-ID: {lineage_id}\r\nX-EvalOps-Record-Status: completed\r\n\r\n{}",
            response.len(),
            response
        );
        stream
            .write_all(wire.as_bytes())
            .await
            .expect("managed provider response");
    });
    (format!("http://{address}/v1"), requests)
}

fn assistant_tool_use(calls: &[(&str, &str)]) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(
            calls
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    input: serde_json::json!({}),
                    gemini_context: None,
                })
                .collect(),
        ),
    }
}

fn tool_results(results: &[(&str, &str, bool)]) -> Vec<ContentBlock> {
    results
        .iter()
        .map(|(id, content, is_error)| ContentBlock::ToolResult {
            tool_use_id: (*id).to_string(),
            content: (*content).to_string(),
            is_error: is_error.then_some(true),
        })
        .collect()
}

#[test]
fn batch_outcomes_read_tool_names_from_the_assistant_tool_use_blocks() {
    let assistant = assistant_tool_use(&[("call-1", "Edit"), ("call-2", "todo")]);
    let host = runtime_test_host_handle();
    let results = tool_results(&[
        ("call-1", "no such file", true),
        ("call-2", r#"{"open":1}"#, false),
    ]);

    let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &results);
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].tool, "edit");
    assert!(!outcomes[0].success);
    assert_eq!(outcomes[0].open_todos, None);
    assert_eq!(outcomes[1].tool, "todo");
    assert!(outcomes[1].success);
    assert_eq!(outcomes[1].open_todos, Some(1));
}

#[test]
fn a_result_with_no_matching_tool_use_is_named_unknown() {
    let assistant = assistant_tool_use(&[("call-1", "edit")]);
    let results = tool_results(&[("call-orphan", "output", false)]);
    let host = runtime_test_host_handle();
    let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &results);
    assert_eq!(outcomes[0].tool, "unknown");
}

#[test]
fn three_consecutive_edit_failures_append_one_reminder_to_the_last_tool_result() {
    let assistant = assistant_tool_use(&[("call-1", "edit"), ("call-2", "read")]);
    let host = runtime_test_host_handle();
    let mut engine = ReminderEngine::new();

    for attempt in 1..=2 {
        let batch = tool_results(&[
            ("call-1", "edit failed", true),
            ("call-2", "file contents", false),
        ]);
        let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &batch);
        assert_eq!(
            engine.observe_batch(&outcomes),
            None,
            "attempt {attempt} must not fire the reminder"
        );
    }

    let mut batch = tool_results(&[
        ("call-1", "edit failed", true),
        ("call-2", "file contents", false),
    ]);
    let before = batch.len();
    let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &batch);
    let reminder = engine
        .observe_batch(&outcomes)
        .expect("the third consecutive edit failure must fire");
    assert!(append_reminder_to_last_tool_result(&mut batch, &reminder));

    assert_eq!(batch.len(), before, "no new block, and no new message");
    let ContentBlock::ToolResult { content: first, .. } = &batch[0] else {
        panic!("first block is a tool result");
    };
    assert_eq!(first, "edit failed", "only the last result is annotated");
    let ContentBlock::ToolResult { content: last, .. } = &batch[1] else {
        panic!("last block is a tool result");
    };
    assert!(last.starts_with("file contents"), "{last}");
    assert!(last.contains(crate::agent::REMINDER_OPEN), "{last}");
    assert!(
        last.contains("`edit` has failed 3 times in a row"),
        "{last}"
    );
}

#[test]
fn a_successful_edit_resets_the_consecutive_failure_run() {
    let assistant = assistant_tool_use(&[("call-1", "edit")]);
    let host = runtime_test_host_handle();
    let mut engine = ReminderEngine::new();
    let failing = tool_results(&[("call-1", "edit failed", true)]);
    let passing = tool_results(&[("call-1", "edit applied", false)]);

    for _ in 0..2 {
        let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &failing);
        assert_eq!(engine.observe_batch(&outcomes), None);
    }
    let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &passing);
    assert_eq!(engine.observe_batch(&outcomes), None);
    for _ in 0..2 {
        let outcomes = tool_outcomes_for_batch(&host, Some(&assistant), &failing);
        assert_eq!(
            engine.observe_batch(&outcomes),
            None,
            "the success reset the run, so two more failures are not three"
        );
    }
}

/// One assistant response whose text is the same line over and over.
fn chat_sse_looping_text_response(id: &str) -> String {
    let looping = "Still checking the same file.\n".repeat(40);
    let start = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": looping}, "finish_reason": null}]
    });
    let stop = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {}, "finish_reason": "stop"}]
    });
    format!("data: {start}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

/// A provider whose first looping response remains open until the client
/// cancels it. The second request is accepted only after that disconnect,
/// making request ordering observable in the regression test.
async fn scripted_looping_text_provider()
-> (String, Arc<Mutex<Vec<serde_json::Value>>>, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let first_released = Arc::new(AtomicBool::new(false));
    let released = Arc::clone(&first_released);
    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first provider request");
        let request = read_scripted_provider_request(&mut first).await;
        captured.lock().unwrap().push(request);
        let looping = "Still checking the same file.\n".repeat(40);
        let start = serde_json::json!({
            "id": "looping-0", "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-4o", "choices": [{"index": 0,
                "delta": {"role": "assistant", "content": looping},
                "finish_reason": null}]
        });
        let partial = format!("data: {start}\n\n");
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n{partial}"
        );
        first
            .write_all(wire.as_bytes())
            .await
            .expect("write first looping response");

        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), first.read(&mut byte))
            .await
            .expect("looping response must be cancelled before retry")
            .expect("observe first provider disconnect");
        assert_eq!(read, 0, "first provider connection must be released");
        released.store(true, Ordering::SeqCst);

        let (mut second, _) = listener.accept().await.expect("second provider request");
        let request = read_scripted_provider_request(&mut second).await;
        captured.lock().unwrap().push(request);
        let response = chat_sse_looping_text_response("looping-1");
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        second
            .write_all(wire.as_bytes())
            .await
            .expect("write second looping response");
    });
    (format!("http://{address}/v1"), requests, first_released)
}

#[tokio::test]
async fn repeating_model_text_is_steered_once_and_then_aborts_the_turn() {
    let (base_url, requests, first_released) = scripted_looping_text_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("scripted client"),
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("looping agent");

    agent
        .prompt("Summarize the repository.".to_owned(), vec![])
        .await
        .expect("looping prompt");

    let message = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("a turn whose text keeps looping must not complete")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before the loop terminal"),
            }
        }
    })
    .await
    .expect("text loop terminal timeout");
    agent.shutdown().await;

    assert!(message.contains("assistant_text_loop"), "{message}");
    assert!(message.contains("multi_line"), "{message}");
    assert!(
        first_released.load(Ordering::SeqCst),
        "the abandoned HTTP/SSE producer must stop before the retry starts"
    );

    let captured = requests.lock().unwrap();
    assert_eq!(
        captured.len(),
        2,
        "the runner must retry exactly once after the first detected loop"
    );
    let retried = serde_json::to_string(&captured[1]).expect("retry request json");
    assert!(
        retried.contains("runtime_note"),
        "the retry must carry the steering reminder: {retried}"
    );
    let first = serde_json::to_string(&captured[0]).expect("first request json");
    assert!(
        !first.contains("runtime_note"),
        "the first request must not carry a reminder"
    );
}

/// One assistant response that asks to `read` a path unique to `index`.
///
/// Unique paths matter: the doom-loop detector in `crate::agent::safety`
/// blocks three *identical* consecutive calls, so a scripted loop that
/// repeated one path would be stopped by that detector instead of by the
/// step budget under test.
fn chat_sse_indexed_tool_response(index: usize) -> String {
    let start = serde_json::json!({
        "id": format!("loop-{index}"), "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
    });
    let tool = serde_json::json!({
        "id": format!("loop-{index}"), "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"tool_calls": [{"index": 0, "id": format!("call-loop-{index}"),
                "type": "function", "function": {"name": "read",
                    "arguments": format!("{{\"path\":\"loop-{index}.md\"}}")}}]},
            "finish_reason": "tool_calls"}]
    });
    format!("data: {start}\n\ndata: {tool}\n\ndata: [DONE]\n\n")
}

/// A provider that never stops asking for tools, and counts how many
/// requests the runner made before it gave up.
async fn scripted_tool_loop_provider() -> (String, Arc<AtomicUsize>) {
    scripted_tool_loop_provider_with_repetition(false).await
}

async fn scripted_tool_loop_provider_with_repetition(repeat: bool) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = read_scripted_provider_request(&mut stream).await;
            let index = counted.fetch_add(1, Ordering::SeqCst);
            let response = chat_sse_indexed_tool_response(index);
            let response = if repeat {
                response.replace(&format!("loop-{index}.md"), "same.md")
            } else {
                response
            };
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            if stream.write_all(wire.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    (format!("http://{address}/v1"), requests)
}

#[tokio::test]
async fn process_budget_refusal_deduplicates_tool_calls_in_checkpoint() {
    let workspace = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_scripted_provider_request(&mut stream).await;
        let calls = (0..2)
            .map(|index| {
                serde_json::json!({
                    "index":index, "id":"duplicate-call", "type":"function",
                    "function":{"name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}
                })
            })
            .collect::<Vec<_>>();
        let tool = serde_json::json!({
            "id":"duplicate-response","object":"chat.completion.chunk","created":0,"model":"gpt-4o",
            "choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]
        });
        let usage = serde_json::json!({
            "id":"duplicate-response","object":"chat.completion.chunk","created":0,"model":"gpt-4o",
            "choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3,"total_tokens":11}
        });
        let body = format!("data: {tool}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream.write_all(wire.as_bytes()).await.unwrap();
    });
    let (agent, mut events) = NativeAgent::new_with_test_client(
        NativeAgentConfig {
            model: "openai/gpt-4o".into(),
            cwd: workspace.path().display().to_string(),
            ..NativeAgentConfig::default()
        },
        UnifiedClient::OpenAI(
            crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1"))
                .unwrap(),
        ),
    )
    .unwrap();
    agent
        .install_process_budget(
            super::super::process_budget::ProcessBudgetLimits {
                event_id: "duplicate-event".into(),
                max_requests: 2,
                max_total_tokens: 10,
                max_cost_micros: 10,
                cost_micros_per_token: 1,
            },
            None,
        )
        .await
        .unwrap();
    agent.prompt("read file".into(), vec![]).await.unwrap();
    let messages = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut checkpoint = None;
        loop {
            match events.recv().await {
                Some(FromAgent::ConversationSnapshot { messages, .. }) => {
                    checkpoint = Some(messages);
                }
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => {
                    assert!(message.contains("budget exhausted"), "{message}");
                    break checkpoint.expect("history precedes refusal");
                }
                Some(FromAgent::ToolStart { .. }) => panic!("refused tool executed"),
                Some(_) => {}
                None => panic!("missing process refusal"),
            }
        }
    })
    .await
    .unwrap();
    let mut uses = 0;
    let mut results = 0;
    for message in messages {
        if let MessageContent::Blocks(blocks) = message.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse { id, .. } if id == "duplicate-call" => uses += 1,
                    ContentBlock::ToolResult { tool_use_id, .. }
                        if tool_use_id == "duplicate-call" =>
                    {
                        results += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(
        (uses, results),
        (1, 1),
        "checkpoint retains exactly one matched pair"
    );
    agent.shutdown().await;
    server.await.unwrap();
}

#[test]
fn process_budget_provider_cost_preserves_exact_gateway_micros_and_real_fractions() {
    assert_eq!(
        process_provider_cost_micros(123_f64 / 1_000_000.0).unwrap(),
        123
    );
    assert_eq!(
        process_provider_cost_micros(123.25 / 1_000_000.0).unwrap(),
        124
    );
    assert_eq!(
        process_provider_cost_micros(123.000_001 / 1_000_000.0).unwrap(),
        124
    );
    for invalid in [f64::NAN, f64::INFINITY, -0.000_001] {
        assert!(process_provider_cost_micros(invalid).is_err());
    }
}

#[tokio::test]
async fn process_budget_refuses_model_tool_effects_before_replay_can_intervene() {
    assert_process_budget_refuses_model_tool_effects(0).await;
}

#[tokio::test]
async fn process_budget_counts_cached_input_before_tool_execution() {
    assert_process_budget_refuses_model_tool_effects(6).await;
}

async fn assert_process_budget_refuses_model_tool_effects(cached_tokens: u64) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_scripted_provider_request(&mut stream).await;
        let usage = serde_json::json!({"id":"budget-response", "object":"chat.completion.chunk",
            "created":0, "model":"gpt-4o", "choices":[],
            "usage":{"prompt_tokens":8,"completion_tokens":3,"total_tokens":11,
                "prompt_tokens_details":{"cached_tokens":cached_tokens}}});
        let body = chat_sse_response("budget-response", "Read the file.", true)
            .replace("data: [DONE]", &format!("data: {usage}\n\ndata: [DONE]"));
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(wire.as_bytes()).await.unwrap();
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    let limits = super::super::process_budget::ProcessBudgetLimits {
        event_id: "event-1".into(),
        max_requests: 2,
        max_total_tokens: 10,
        max_cost_micros: 20,
        cost_micros_per_token: 2,
    };
    let checkpoint = agent
        .install_process_budget(limits.clone(), None)
        .await
        .unwrap();
    agent
        .prompt("Read Cargo.toml".into(), vec![])
        .await
        .unwrap();
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::ToolStart { .. }) => {
                    panic!("over-budget response executed a tool")
                }
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("over-budget process completed")
                }
                Some(_) => {}
                None => panic!("missing process failure"),
            }
        }
    })
    .await
    .unwrap();
    assert!(message.contains("budget exhausted"), "{message}");
    assert_eq!(checkpoint.lock().unwrap().total_tokens, 11);
    let replay = agent.install_process_budget(limits, None).await.unwrap();
    assert!(Arc::ptr_eq(&checkpoint, &replay));
    assert_eq!(replay.lock().unwrap().total_tokens, 11);
    agent.shutdown().await;
    server.await.unwrap();
}

#[tokio::test]
async fn turn_loop_stops_at_the_step_budget_and_names_the_refused_tool_calls() {
    let (base_url, requests) = scripted_tool_loop_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("scripted client"),
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("looping agent");

    agent
        .prompt("Read every file you can find.".to_owned(), vec![])
        .await
        .expect("looping prompt");

    let message = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("a turn that never stops calling tools must not complete")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before the step-budget terminal"),
            }
        }
    })
    .await
    .expect("step budget terminal timeout");
    agent.shutdown().await;

    assert!(message.contains("step_budget_exhausted"), "{message}");
    assert!(message.contains("budget of 4 model responses"), "{message}");
    assert!(
        message.contains("were not executed: read"),
        "the terminal must name the refused tool calls: {message}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "the turn must stop after exactly max_turn_steps provider requests"
    );
}

#[tokio::test]
async fn native_identical_tool_guard_stops_an_unbounded_turn() {
    let (base_url, requests) = scripted_tool_loop_provider_with_repetition(true).await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        allow_unbounded_turn: true,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("scripted client"),
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("looping agent");

    agent
        .prompt("Read every file you can find.".to_owned(), vec![])
        .await
        .expect("looping prompt");

    let message = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("a turn that never stops calling tools must not complete")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before the step-budget terminal"),
            }
        }
    })
    .await
    .expect("step budget terminal timeout");
    agent.shutdown().await;

    assert!(
        message.contains("three identical tool proposals"),
        "{message}"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn request_retry_preserves_the_turn_step_budget() {
    let scripted = crate::ai::ScriptedClient::new(
        "step-budget-retry",
        vec![
            crate::ai::ScriptedResponse::stream_error("429 rate limit retry-after: 0 seconds"),
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "call-after-retry".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({ "path": "Cargo.toml" }),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::text(
                "this response must remain unused after budget exhaustion",
            ),
        ],
    );
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "scripted/step-budget-retry".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        max_turn_steps: 2,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, UnifiedClient::Scripted(scripted.clone()))
            .expect("scripted agent");

    agent
        .prompt("Read the manifest after retrying.".to_owned(), vec![])
        .await
        .expect("prompt");

    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break message,
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("a retried request must not receive a fresh step budget")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before budget terminal"),
            }
        }
    })
    .await
    .expect("budget terminal timeout");
    agent.shutdown().await;

    assert!(terminal.contains("step_budget_exhausted"), "{terminal}");
    assert_eq!(
        scripted.remaining(),
        1,
        "the provider response after the exhausted budget must not be requested"
    );
}

#[tokio::test]
async fn request_retry_preserves_denials_until_the_next_user_turn() {
    let denied_args = serde_json::json!({ "command": "printf denied" });
    let scripted = crate::ai::ScriptedClient::new(
        "denial-retry",
        vec![
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "call-denied-first".to_owned(),
                    name: "bash".to_owned(),
                    input: denied_args.clone(),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::stream_error("429 rate limit retry-after: 0 seconds"),
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "call-denied-after-retry".to_owned(),
                    name: "bash".to_owned(),
                    input: denied_args,
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::text("Finished without running the denied call."),
        ],
    );
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "scripted/denial-retry".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Safe,
        max_turn_steps: 8,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, UnifiedClient::Scripted(scripted.clone()))
            .expect("scripted agent");
    let tool_responses = agent.tool_response_sender();

    agent
        .prompt(
            "Try the command, but respect my refusal.".to_owned(),
            vec![],
        )
        .await
        .expect("prompt");

    let approval_requests = tokio::time::timeout(Duration::from_secs(10), async {
        let mut approval_requests = 0usize;
        loop {
            match events.recv().await {
                Some(FromAgent::ToolCall {
                    call_id,
                    requires_approval: true,
                    ..
                }) => {
                    approval_requests += 1;
                    tool_responses
                        .send((call_id, false, None, ExecutionSource::Native, None))
                        .expect("deny tool call");
                }
                Some(FromAgent::TurnCompleted { .. }) => break approval_requests,
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => panic!("provider turn failed: {message}"),
                Some(FromAgent::ProviderError { message, .. }) => {
                    panic!("provider turn failed: {message}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before turn completion"),
            }
        }
    })
    .await
    .expect("denial retry timeout");
    agent.shutdown().await;

    assert_eq!(
        approval_requests, 1,
        "the identical call after a provider retry must reuse the first refusal"
    );
    assert_eq!(scripted.remaining(), 0, "the scripted turn should complete");
}

#[tokio::test]
async fn context_exclusion_changes_next_request_and_its_schema_report() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_scripted_provider_request(&mut stream).await;
            captured.lock().unwrap().push(request);
            let body = chat_sse_response("context-fixture", "Done.", false);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let fixture = ToolDefinition {
        tool: Tool::new("fixture_integration", "Optional integration")
            .with_schema(serde_json::json!({"description": "large schema ".repeat(2000)})),
        requires_approval: true,
    };
    let (agent, mut events) =
        NativeAgent::new_with_external_tools(config, vec![fixture], None, client).unwrap();
    agent
        .set_session_context(Some("context-test".into()), "new", false)
        .unwrap();
    let mut counts = Vec::new();
    for index in 0..3 {
        if index == 1 {
            agent
                .set_context_tool_excluded("fixture_integration".into(), true)
                .unwrap();
        }
        if index == 2 {
            agent
                .set_context_tool_excluded("fixture_integration".into(), false)
                .unwrap();
        }
        agent.prompt("Say done.".into(), vec![]).await.unwrap();
        let mut observed_prepared_context = false;
        let mut turn_starts = 0;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await.unwrap() {
                    FromAgent::TurnCompleted { .. } => break,
                    FromAgent::TurnStarted => turn_starts += 1,
                    FromAgent::ResponseStart { .. } => {
                        assert_eq!(
                            turn_starts, 1,
                            "turn attribution must precede its responses"
                        );
                    }
                    FromAgent::RequestContextPrepared { .. } => {
                        assert!(agent.runtime_audit_snapshot().request_context.is_some());
                        observed_prepared_context = true;
                    }
                    FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                        panic!("{message}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(observed_prepared_context);
        assert_eq!(turn_starts, 1);
        let snapshot = agent.runtime_audit_snapshot();
        let topology = snapshot
            .request_cache
            .as_ref()
            .unwrap()
            .cache_topology
            .as_ref()
            .unwrap();
        assert_eq!(topology.generation, index + 1);
        if index > 0 {
            assert_eq!(
                topology.transition,
                maestro_ai::cache_topology::CacheTransition::ToolsChanged
            );
            assert_eq!(
                snapshot.cache_reuse,
                Some(maestro_context::token_counting::CacheReuse::ToolsChanged)
            );
        } else {
            assert!(snapshot.cache_reuse.is_none());
        }
        let report = snapshot.request_context.unwrap();
        counts.push(
            report
                .tools
                .iter()
                .find(|(name, _)| name == "fixture_integration")
                .map(|(_, count)| *count),
        );
    }
    agent.shutdown().await;
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    let advertised = requests
        .iter()
        .map(|request| {
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == "fixture_integration")
        })
        .collect::<Vec<_>>();
    assert_eq!(advertised, [true, false, true]);
    assert!(counts[0].unwrap() > 1000);
    assert_eq!(counts[1], None);
    assert_eq!(counts[0], counts[2]);
}

#[tokio::test]
async fn max_tokens_does_not_rewrite_input_history() {
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(128_000),
        ..Default::default()
    };
    let mut response = crate::ai::ScriptedResponse::text("Partial answer.");
    response.stop_reason = crate::ai::StopReason::MaxTokens;
    let client = crate::ai::ScriptedClient::new("output-limit", vec![response]);
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, UnifiedClient::Scripted(client)).unwrap();
    agent.replace_history_with_continuation(
        (0..20)
            .map(|index| Message {
                role: if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: MessageContent::text("retain context ".repeat(100)),
            })
            .collect(),
        None,
    );
    agent.prompt("Continue".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Compaction { .. } | FromAgent::CompactionMeasured { .. }) => {
                    panic!("an output limit must not compact input history")
                }
                Some(FromAgent::TurnCompleted { .. }) => break,
                Some(FromAgent::Error {
                    terminal: true,
                    message,
                    ..
                }) => panic!("{message}"),
                Some(_) => {}
                None => panic!("agent ended before completing the response"),
            }
        }
    })
    .await
    .expect("turn completed");
    agent.shutdown().await;
}

#[tokio::test]
async fn max_tokens_refuses_complete_json_tool_calls_without_execution() {
    let workspace = tempfile::tempdir().unwrap();
    let scripted = crate::ai::ScriptedClient::new(
        "output-limit",
        vec![
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "truncated-write".into(),
                    name: "write".into(),
                    input: serde_json::json!({"path":"result.txt","content":"valid but incomplete"}),
                }],
                stop_reason: crate::ai::StopReason::MaxTokens,
                error: None,
            },
            crate::ai::ScriptedResponse::text("The truncated call did not execute."),
        ],
    );
    let config = NativeAgentConfig {
        model: "scripted/output-limit".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..Default::default()
    };
    let host = RuntimeTestHost::new(workspace.path(), UnifiedClient::Scripted(scripted.clone()));
    let executions = Arc::clone(&host.completed_tool_executions);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    let mut refused = false;
    agent.prompt("Write a file".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message,
                    terminal: false,
                    ..
                }) => {
                    refused |= message.contains("not_executed") && message.contains("truncated");
                }
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => panic!("{message}"),
                Some(FromAgent::TurnCompleted { .. }) => break,
                Some(_) => {}
                None => panic!("agent ended before completing the response"),
            }
        }
    })
    .await
    .expect("turn completed");
    assert!(
        refused,
        "the truncated call must produce an explicit refusal"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert_eq!(
        scripted.remaining(),
        0,
        "the model receives the refusal and can continue"
    );
    assert!(!workspace.path().join("result.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn ordinary_compaction_keeps_restored_user_boundaries_in_checkpoint() {
    let workspace = tempfile::tempdir().unwrap();
    let prior = super::super::compaction::build_continuation_record(&[Message {
        role: Role::User,
        content: MessageContent::text("Do not publish. Work locally."),
    }]);
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(1024),
        ..Default::default()
    };
    let client = crate::ai::ScriptedClient::new(
        "continuation",
        vec![crate::ai::ScriptedResponse::text("Done.")],
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, UnifiedClient::Scripted(client)).unwrap();
    let history = (0..20)
        .map(|index| Message {
            role: if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: MessageContent::text(format!(
                "Turn {index}: {}",
                "retain this context ".repeat(80)
            )),
        })
        .collect();
    agent.replace_history_with_continuation(history, Some(prior));
    agent
        .prompt("Only change the CLI".into(), vec![])
        .await
        .unwrap();
    let checkpoint = tokio::time::timeout(Duration::from_secs(10), async {
        let mut compaction_measured = false;
        while let Some(event) = events.recv().await {
            if matches!(&event, FromAgent::CompactionMeasured { .. }) {
                compaction_measured = true;
            }
            if matches!(&event, FromAgent::ResponseEnd { .. }) {
                assert!(
                    compaction_measured,
                    "compaction timing must precede response end"
                );
            }
            if let FromAgent::Compaction {
                continuation: Some(record),
                ..
            } = event
            {
                assert!(
                    compaction_measured,
                    "checkpoint requires observed compaction timing"
                );
                return record;
            }
        }
        panic!("runner ended without a compaction checkpoint");
    })
    .await
    .expect("compaction should complete");
    let audit = Arc::clone(&agent.runtime_audit);
    agent.shutdown().await;
    let snapshot = audit.read().unwrap().clone();
    let topology = snapshot
        .request_cache
        .as_ref()
        .unwrap()
        .cache_topology
        .as_ref()
        .unwrap();
    assert_eq!(
        topology.generation, 2,
        "checkpoint must be installed without another model request"
    );
    assert_eq!(
        topology.transition,
        maestro_ai::cache_topology::CacheTransition::HistoryRewritten
    );

    assert_eq!(
        checkpoint.user_requests.first().map(String::as_str),
        Some("Do not publish. Work locally.")
    );
    let restored: super::super::compaction::ContinuationRecord =
        serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    assert_eq!(restored.user_requests, checkpoint.user_requests);
}

#[tokio::test]
async fn manual_summary_installs_next_generation_before_another_primary_request() {
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        ..Default::default()
    };
    let client = crate::ai::ScriptedClient::new(
        "manual-summary",
        vec![
            crate::ai::ScriptedResponse::text("Original answer."),
            crate::ai::ScriptedResponse::text("Continued summary."),
            crate::ai::ScriptedResponse::text("Unrelated session."),
        ],
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, UnifiedClient::Scripted(client)).unwrap();
    agent
        .set_session_context(Some("source".into()), "new", false)
        .unwrap();
    agent
        .prompt("Retain this constraint".into(), vec![])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if matches!(event, FromAgent::TurnCompleted { .. }) {
                return;
            }
        }
        panic!("turn did not complete");
    })
    .await
    .unwrap();
    let before = agent
        .runtime_audit_snapshot()
        .request_cache
        .unwrap()
        .cache_topology
        .unwrap();
    let preview = agent
        .start_selective_summary_preview()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    agent
        .apply_selective_summary(
            vec![Message {
                role: Role::User,
                content: MessageContent::text("Reviewed summary: retain this constraint"),
            }],
            preview.history_digest,
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    let after = agent
        .runtime_audit_snapshot()
        .request_cache
        .unwrap()
        .cache_topology
        .unwrap();
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(
        after.transition,
        maestro_ai::cache_topology::CacheTransition::HistoryRewritten
    );
    agent
        .set_compacted_session_context_with_transcript("summary-child".into(), None, false)
        .unwrap();
    for (prompt, generation, transition) in [
        (
            "Continue the summary",
            2,
            maestro_ai::cache_topology::CacheTransition::Append,
        ),
        (
            "A separate conversation",
            1,
            maestro_ai::cache_topology::CacheTransition::Initial,
        ),
    ] {
        if generation == 1 {
            agent
                .set_session_context(Some("unrelated".into()), "new", false)
                .unwrap();
        }
        agent.prompt(prompt.into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if matches!(event, FromAgent::TurnCompleted { .. }) {
                    return;
                }
            }
            panic!("turn did not complete");
        })
        .await
        .unwrap();
        let topology = agent
            .runtime_audit_snapshot()
            .request_cache
            .unwrap()
            .cache_topology
            .unwrap();
        assert_eq!(topology.generation, generation);
        assert_eq!(topology.transition, transition);
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn selective_summary_uses_only_selected_history_without_tools_and_applies_conditionally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_scripted_provider_request(&mut stream).await;
        let body = chat_sse_response("summary-fixture", "Selected facts only.", false);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        request
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        model_dynamics: ModelDynamicsConfig {
            summary_model: Some("openai/gpt-4o-mini".into()),
            ..Default::default()
        },
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, _events) =
        NativeAgent::new_with_external_tools(config, vec![], None, client).unwrap();
    let messages = vec![
        Message {
            role: Role::User,
            content: MessageContent::text("PRIVATE_UNSELECTED_PREFIX"),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::text("prefix answer"),
        },
        Message {
            role: Role::User,
            content: MessageContent::text("SELECTED_TURN_FACT"),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::text("selected answer"),
        },
    ];
    agent.replace_history_preserving_credentials(messages);
    let preview = agent
        .start_selective_summary_preview()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    let request = agent
        .start_selective_summary_with_instructions(
            super::super::RangeSelection::FromTurn(2),
            preview.history_digest.clone(),
            Some("Retain selected evidence".into()),
        )
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), request.receiver)
        .await
        .unwrap()
        .unwrap();
    let proposed = outcome.result.unwrap();
    assert_eq!(proposed.summary, "Selected facts only.");
    let unchanged = agent
        .start_selective_summary_preview()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.history_digest, preview.history_digest);
    let captured = server.await.unwrap();
    let sent = serde_json::to_string(&captured["messages"]).unwrap();
    assert!(sent.contains("SELECTED_TURN_FACT"));
    assert!(sent.contains("Retain selected evidence"));
    assert!(!sent.contains("PRIVATE_UNSELECTED_PREFIX"));
    assert!(
        captured
            .get("tools")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
    );
    assert_eq!(
        captured["model"], "gpt-4o-mini",
        "summary uses the configured model on the existing fixture connection"
    );
    assert!(
        captured["max_tokens"]
            .as_u64()
            .unwrap_or_else(|| captured["max_completion_tokens"].as_u64().unwrap())
            <= 2048
    );
    assert!(
        agent
            .apply_selective_summary(proposed.messages.clone(), "stale".into())
            .unwrap()
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(
        agent
            .start_selective_summary_preview()
            .unwrap()
            .await
            .unwrap()
            .unwrap()
            .history_digest,
        preview.history_digest
    );
    let orphan = vec![Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "missing".into(),
            content: "orphan".into(),
            is_error: Some(false),
        }]),
    }];
    assert!(
        agent
            .apply_selective_summary(orphan, preview.history_digest.clone())
            .unwrap()
            .await
            .unwrap()
            .is_err()
    );
    agent
        .apply_selective_summary(proposed.messages, preview.history_digest.clone())
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        agent
            .start_selective_summary_preview()
            .unwrap()
            .await
            .unwrap()
            .unwrap()
            .history_digest,
        preview.history_digest
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn boost_cancellation_restores_effort_before_the_next_task() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let boosted = read_scripted_provider_request(&mut first).await;
        started_tx.send(()).unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let restored = read_scripted_provider_request(&mut second).await;
        let response = chat_sse_response("after-cancel", "Done.", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        second.write_all(wire.as_bytes()).await.unwrap();
        captured_tx.send((boosted, restored)).unwrap();
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openrouter/openai/o1".into(),
        cwd: workspace.path().display().to_string(),
        thinking_enabled: true,
        thinking_budget: 10_000,
        model_dynamics: Default::default(),
        ..Default::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent.boost().unwrap();
    agent.prompt("Wait for me.".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), started_rx)
        .await
        .unwrap()
        .unwrap();
    agent.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            events.recv().await.unwrap(),
            FromAgent::TurnInterrupted { .. }
        ) {}
    })
    .await
    .unwrap();
    agent.prompt("Finish now.".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap() {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    let (boosted, restored) = captured_rx.await.unwrap();
    assert_eq!(boosted["reasoning_effort"], "high");
    assert_eq!(restored["reasoning_effort"], "medium");
}

#[tokio::test]
async fn boost_changes_wire_effort_once_and_restores_next_task() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_scripted_provider_request(&mut stream).await;
            captured.lock().unwrap().push(request);
            let response = chat_sse_response("boost-final", "Done.", false);
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openrouter/openai/o1".into(),
        cwd: workspace.path().display().to_string(),
        thinking_enabled: true,
        thinking_budget: 10_000,
        ..Default::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent.boost().unwrap();
    agent.boost().unwrap();
    let mut active = 0;
    let mut restored = 0;
    for _ in 0..2 {
        agent.prompt("Say done.".into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await.unwrap() {
                    FromAgent::BoostChanged {
                        status: BoostStatus::Active,
                        ..
                    } => active += 1,
                    FromAgent::BoostChanged {
                        status: BoostStatus::Idle,
                        thinking: Some(ThinkingLevel::Medium),
                    } => restored += 1,
                    FromAgent::TurnCompleted { .. } => break,
                    FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                        panic!("{message}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
    agent.shutdown().await;
    assert_eq!(active, 1);
    assert_eq!(restored, 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["reasoning_effort"], "high");
    assert_eq!(requests[1]["reasoning_effort"], "medium");
    assert!(
        requests[1]["messages"].as_array().unwrap().len()
            > requests[0]["messages"].as_array().unwrap().len()
    );
}

#[tokio::test]
async fn hosted_default_fast_trivial_turn_is_one_request_without_rlm_or_approval() {
    let (base_url, requests) = scripted_single_turn_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Selective,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("scripted client"),
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("hosted agent");

    agent
        .prompt("Reply with a short greeting.".to_owned(), vec![])
        .await
        .expect("hosted prompt");

    let mut assistant_text = String::new();
    let mut approval_requested = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ResponseChunk {
                    content,
                    is_thinking: false,
                    ..
                }) => assistant_text.push_str(&content),
                Some(FromAgent::ToolCall {
                    requires_approval: true,
                    ..
                }) => approval_requested = true,
                Some(FromAgent::TurnCompleted { .. }) => break,
                Some(FromAgent::Error { message, .. }) => {
                    panic!("hosted turn failed: {message}")
                }
                Some(FromAgent::ProviderError { message, .. }) => {
                    panic!("provider turn failed: {message}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before turn_completed"),
            }
        }
    })
    .await
    .expect("hosted turn timeout");
    agent.shutdown().await;

    assert!(!assistant_text.trim().is_empty());
    assert!(!approval_requested);
    let captured = requests.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "trivial turn must use one provider request"
    );
    let advertised = captured[0]["tools"]
        .as_array()
        .expect("OpenAI tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<HashSet<_>>();
    for rlm_tool in [
        "get_rlm_context",
        "set_rlm_context",
        "append_rlm_context",
        "render_rlm_context",
        "clear_rlm_context",
    ] {
        assert!(
            !advertised.contains(rlm_tool),
            "default Fast request advertised RLM tool {rlm_tool}"
        );
    }
}

#[tokio::test]
async fn managed_print_prompt_propagates_hashed_lineage_to_gateway_request() {
    let (base_url, requests) = scripted_managed_single_turn_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "evalops/openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::from_model_with_env(
        "evalops/openai/gpt-4o",
        &HashMap::from([
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN".to_owned(),
                "delegated-token".to_owned(),
            ),
            ("MAESTRO_EVALOPS_BASE_URL".to_owned(), base_url),
            ("MAESTRO_EVALOPS_ORG_ID".to_owned(), "org-test".to_owned()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".to_owned(),
                "workspace-test".to_owned(),
            ),
            ("MAESTRO_EVALOPS_PROVIDER".to_owned(), "openai".to_owned()),
            (
                "MAESTRO_EVALOPS_ENVIRONMENT".to_owned(),
                "production".to_owned(),
            ),
        ]),
    )
    .expect("managed client");
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("native agent");
    agent
        .set_session_context(Some("print-fixture".to_owned()), "start", false)
        .unwrap();
    agent
        .prompt("Reply with a short greeting.".to_owned(), vec![])
        .await
        .expect("managed prompt");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::TurnCompleted { .. }) => break,
                Some(
                    FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. },
                ) => {
                    panic!("managed turn failed: {message}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before turn completion"),
            }
        }
    })
    .await
    .expect("managed turn timeout");
    agent.shutdown().await;

    let captured = requests.lock().unwrap();
    let lineage = captured[0]["lineage_id"]
        .as_str()
        .expect("managed request lineage");
    assert!(lineage.starts_with("maestro-turn-v2:"));
}

#[tokio::test]
async fn semantic_snapshot_precedes_public_success_terminals() {
    let (base_url, _requests) = scripted_single_turn_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("scripted client"),
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("hosted agent");

    agent
        .prompt("ordering sentinel".to_owned(), vec![])
        .await
        .expect("hosted prompt");

    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut observed = Vec::new();
        let mut saw_snapshot = false;
        let mut saw_completed = false;
        while !(saw_snapshot && saw_completed) {
            match events.recv().await {
                Some(FromAgent::ConversationSnapshot { .. }) => {
                    observed.push("snapshot");
                    saw_snapshot = true;
                }
                Some(FromAgent::ResponseEnd { response_id, .. }) if response_id == "done" => {
                    observed.push("response_end");
                }
                Some(FromAgent::TurnCompleted { .. }) => {
                    observed.push("turn_completed");
                    saw_completed = true;
                }
                Some(FromAgent::Error { message, .. }) => {
                    panic!("hosted turn failed: {message}")
                }
                Some(FromAgent::ProviderError { message, .. }) => {
                    panic!("provider turn failed: {message}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before terminal sequence"),
            }
        }
        observed
    })
    .await
    .expect("hosted turn timeout");
    agent.shutdown().await;

    assert_eq!(
        observed,
        vec!["snapshot", "response_end", "turn_completed"],
        "a persistable semantic checkpoint must lead every public success terminal"
    );
}

#[test]
fn semantic_checkpoint_excludes_thinking_and_raw_tool_output_but_keeps_tool_pair_ids() {
    let checkpoint = sanitize_semantic_conversation(&[
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "hidden chain of thought".to_owned(),
                    signature: Some("signature".to_owned()),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({ "path": "src/lib.rs" }),
                    gemini_context: None,
                },
            ]),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".to_owned(),
                content: "unbounded private file content".to_owned(),
                is_error: Some(false),
            }]),
        },
    ]);

    let json = serde_json::to_string(&checkpoint).unwrap();
    assert!(!json.contains("hidden chain of thought"));
    assert!(!json.contains("unbounded private file content"));
    assert!(json.contains("call-1"));
    assert!(json.contains("[tool result omitted from checkpoint]"));
}

#[test]
fn terminal_snapshot_event_is_emitted_with_the_public_continue_terminal_shape() {
    let event = conversation_snapshot_event(&[Message {
        role: Role::User,
        content: MessageContent::text("continue from this context"),
    }])
    .expect("snapshot event");

    assert!(matches!(
        event,
        FromAgent::ConversationSnapshot { messages, .. }
            if messages.len() == 1 && messages[0].content.as_text() == Some("continue from this context")
    ));
}

#[test]
fn codex_dynamic_tool_pair_is_retained_in_ordered_terminal_snapshot() {
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: MessageContent::text("I'll inspect that first."),
    }];
    append_codex_tool_use(
        &mut messages,
        "codex-call-1",
        "read",
        serde_json::json!({ "path": "src/lib.rs" }),
    );
    append_codex_tool_result(
        &mut messages,
        "codex-call-1",
        "simulated tool failure".to_owned(),
        true,
    );

    let FromAgent::ConversationSnapshot { messages, .. } =
        conversation_snapshot_event(&messages).expect("snapshot")
    else {
        panic!("expected semantic snapshot");
    };
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0].content.as_text(),
        Some("I'll inspect that first.")
    );
    assert!(matches!(
        &messages[1].content,
        MessageContent::Blocks(blocks)
            if matches!(&blocks[0], ContentBlock::ToolUse { id, .. } if id == "codex-call-1")
    ));
    assert!(matches!(
        &messages[2].content,
        MessageContent::Blocks(blocks)
            if matches!(&blocks[0], ContentBlock::ToolResult { tool_use_id, is_error: Some(true), .. } if tool_use_id == "codex-call-1")
    ));
}

#[test]
fn codex_completion_delta_is_emitted_once_and_reaches_terminal_snapshot() {
    let (visible_completion, full_text) =
        NativeAgentRunner::reconcile_codex_completion_text("prefix ", "suffix", false);
    assert_eq!(visible_completion, "suffix");
    assert_eq!(full_text, "prefix suffix");

    let FromAgent::ConversationSnapshot { messages, .. } =
        conversation_snapshot_event(&[Message {
            role: Role::Assistant,
            content: MessageContent::Text(full_text),
        }])
        .expect("terminal snapshot")
    else {
        panic!("expected semantic snapshot");
    };
    assert_eq!(messages[0].content.as_text(), Some("prefix suffix"));
}

#[test]
fn codex_completion_reconciliation_accepts_suffix_or_authoritative_full_text() {
    assert_eq!(
        NativeAgentRunner::reconcile_codex_completion_text("prefix ", "suffix", false),
        ("suffix".to_owned(), "prefix suffix".to_owned())
    );
    assert_eq!(
        NativeAgentRunner::reconcile_codex_completion_text("prefix ", "prefix suffix", true),
        ("suffix".to_owned(), "prefix suffix".to_owned())
    );
}

#[test]
fn codex_divergent_authoritative_completion_reaches_terminal_snapshot() {
    let (visible_completion, full_text) =
        NativeAgentRunner::reconcile_codex_completion_text("partial", "full answer", true);
    assert!(visible_completion.is_empty());

    let final_text =
        NativeAgentRunner::codex_terminal_assistant_text("partial".to_owned(), full_text, true);
    let FromAgent::ConversationSnapshot { messages, .. } =
        conversation_snapshot_event(&[Message {
            role: Role::Assistant,
            content: MessageContent::Text(final_text),
        }])
        .expect("terminal snapshot")
    else {
        panic!("expected semantic snapshot");
    };
    assert_eq!(messages[0].content.as_text(), Some("full answer"));
}

#[test]
fn codex_authoritative_segments_preserve_tool_boundaries_without_duplication() {
    let (_, before) =
        NativeAgentRunner::reconcile_codex_completion_text("draft before", "before", true);
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: MessageContent::Text(before),
    }];
    append_codex_tool_use(
        &mut messages,
        "call-1",
        "read",
        serde_json::json!({ "path": "src/lib.rs" }),
    );
    append_codex_tool_result(&mut messages, "call-1", "tool output".to_owned(), false);
    let (_, after) =
        NativeAgentRunner::reconcile_codex_completion_text("draft after", "after", true);
    messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Text(after),
    });

    let FromAgent::ConversationSnapshot { messages, .. } =
        conversation_snapshot_event(&messages).expect("terminal snapshot")
    else {
        panic!("expected semantic snapshot");
    };
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].content.as_text(), Some("before"));
    assert_eq!(messages[3].content.as_text(), Some("after"));
    assert!(
        !messages[3]
            .content
            .as_text()
            .expect("final assistant text")
            .contains("before")
    );
}

#[tokio::test]
async fn retry_backoff_is_interrupted_by_request_cancellation() {
    let cancel_token = CancellationToken::new();
    let waiter_token = cancel_token.clone();
    let shutdown_token = CancellationToken::new();
    let waiter = tokio::spawn(async move {
        wait_for_retry_delay(
            std::time::Duration::from_mins(1),
            &waiter_token,
            &shutdown_token,
        )
        .await
    });

    tokio::task::yield_now().await;
    cancel_token.cancel();
    let completed_delay = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("cancellation must interrupt the retry backoff")
        .expect("retry waiter task must not panic");
    assert!(!completed_delay, "cancelled backoff must not begin a retry");
}

#[tokio::test]
async fn shutdown_awaits_terminal_outcome_beyond_the_old_drain_cutoff() {
    let request_cancel = CancellationToken::new();
    let shutdown_token = CancellationToken::new();
    let shutdown = shutdown_token.clone();

    let request = tokio::spawn(async move {
        let active_cancellation = Arc::new(Mutex::new(ActiveCancellation {
            terminal_drain_required: true,
            ..ActiveCancellation::default()
        }));
        run_request_with_cancellation(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
                Ok(())
            },
            &request_cancel,
            &shutdown_token,
            &active_cancellation,
        )
        .await
    });
    tokio::task::yield_now().await;
    shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("shutdown should await the active terminal outcome")
        .expect("request task should not panic");
    assert!(
        result.is_ok(),
        "shutdown must not replace a late successful terminal with cancellation"
    );
}

#[tokio::test]
async fn shutdown_preempts_and_drops_attachment_loading() {
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let request_cancel = CancellationToken::new();
    let shutdown_token = CancellationToken::new();
    let shutdown = shutdown_token.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_by_load = Arc::clone(&entered);
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_by_load = Arc::clone(&dropped);

    let loading = tokio::spawn(async move {
        load_until_cancelled(
            async move {
                let _drop_flag = DropFlag(dropped_by_load);
                entered_by_load.notify_one();
                std::future::pending::<Vec<ContentBlock>>().await
            },
            &request_cancel,
            &shutdown_token,
        )
        .await
    });

    entered.notified().await;
    shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), loading)
        .await
        .expect("shutdown should preempt attachment loading")
        .expect("attachment loading task should not panic");
    assert!(matches!(result, CancellableLoad::Shutdown));
    assert!(
        dropped.load(Ordering::SeqCst),
        "shutdown must drop the in-flight attachment future"
    );
}

#[test]
fn test_config_default() {
    let config = NativeAgentConfig::default();
    assert_eq!(config.model, "gpt-5.1-codex-max");
    assert_eq!(config.max_tokens, 16_384);
    assert!(!config.thinking_enabled);
    assert_eq!(config.approval_mode, ApprovalMode::Selective);
}

#[test]
fn host_catalog_refreshes_the_default_output_budget() {
    let host = runtime_catalog_host_handle(32_768, 128_000);
    let mut config = NativeAgentConfig::default();
    let mut compactor = super::super::compaction::ContextCompactor::new(
        super::super::compaction::CompactionConfig::for_model(&config.model, config.context_window),
    );

    refresh_model_budgets_with_host(&host, &mut config, &mut compactor, "fixture/catalog-model");

    assert_eq!(config.max_tokens, 32_768);
}

#[test]
fn model_switch_refreshes_output_and_compaction_budgets() {
    let mut config = NativeAgentConfig {
        model: "uncataloged/startup-model".to_owned(),
        ..NativeAgentConfig::default()
    };
    let mut compactor = super::super::compaction::ContextCompactor::new(
        super::super::compaction::CompactionConfig::for_model(&config.model, config.context_window),
    );
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text("x".repeat(400_000)),
    }];

    assert_eq!(config.max_tokens, 16_384);
    assert!(compactor.should_auto_compact(&messages));

    let host = runtime_catalog_host_handle(32_768, 1_000_000);
    refresh_model_budgets_with_host(&host, &mut config, &mut compactor, "fixture/large-context");

    assert_eq!(config.max_tokens, 32_768);
    assert!(!compactor.should_auto_compact(&messages));
}

#[test]
fn model_switch_preserves_explicit_context_window_override() {
    let mut config = NativeAgentConfig {
        model: "uncataloged/startup-model".to_owned(),
        context_window: Some(96_000),
        ..NativeAgentConfig::default()
    };
    let mut compactor = super::super::compaction::ContextCompactor::new(
        super::super::compaction::CompactionConfig::for_model(&config.model, config.context_window),
    );
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text("x".repeat(400_000)),
    }];

    let host = runtime_catalog_host_handle(32_768, 128_000);
    refresh_model_budgets_with_host(&host, &mut config, &mut compactor, "fixture/model");

    assert_eq!(config.context_window, Some(96_000));
    assert!(compactor.should_auto_compact(&messages));
}

#[test]
fn model_switch_preserves_explicit_output_token_override() {
    let mut config = NativeAgentConfig {
        model: "uncataloged/startup-model".to_owned(),
        max_tokens: 7_777,
        max_tokens_source: MaxTokensSource::Explicit,
        ..NativeAgentConfig::default()
    };
    let mut compactor = super::super::compaction::ContextCompactor::new(
        super::super::compaction::CompactionConfig::for_model(&config.model, config.context_window),
    );

    let host = runtime_catalog_host_handle(32_768, 128_000);
    refresh_model_budgets_with_host(&host, &mut config, &mut compactor, "fixture/model");

    assert_eq!(config.max_tokens, 7_777);
}

#[test]
fn model_switch_preserves_explicit_output_limit_equal_to_previous_default() {
    let mut config = NativeAgentConfig {
        model: "uncataloged/startup-model".to_owned(),
        max_tokens: 16_384,
        max_tokens_source: MaxTokensSource::Explicit,
        ..NativeAgentConfig::default()
    };
    let mut compactor = super::super::compaction::ContextCompactor::new(
        super::super::compaction::CompactionConfig::for_model(&config.model, config.context_window),
    );

    let host = runtime_catalog_host_handle(32_768, 128_000);
    refresh_model_budgets_with_host(&host, &mut config, &mut compactor, "fixture/model");

    assert_eq!(config.max_tokens, 16_384);
}

#[test]
fn set_max_tokens_marks_the_limit_explicit() {
    let mut config = NativeAgentConfig::default();
    assert_eq!(config.max_tokens_source, MaxTokensSource::Catalog);
    let max_tokens = config.max_tokens;
    let mut compactor = super::super::compaction::ContextCompactor::new(
        super::super::compaction::CompactionConfig::for_model(&config.model, config.context_window),
    );

    set_explicit_max_tokens(&mut config, max_tokens);
    let host = runtime_catalog_host_handle(32_768, 128_000);
    refresh_model_budgets_with_host(&host, &mut config, &mut compactor, "fixture/model");

    assert_eq!(config.max_tokens_source, MaxTokensSource::Explicit);
    assert_eq!(config.max_tokens, max_tokens);
}

#[tokio::test]
async fn agent_cancel_interrupts_a_blocked_runner_before_queue_processing() {
    let request_token = CancellationToken::new();
    let tool_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: Some(tool_token.clone()),
        approval: None,
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let agent = super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: active.clone(),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token: CancellationToken::new(),
        runner_handle: None,
    };

    agent.cancel_keep_queue();

    assert!(tool_token.is_cancelled());
    assert!(
        !request_token.is_cancelled(),
        "the turn selector must not race tool cleanup"
    );
    assert!(
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_interrupted,
        "the deferred suffix must observe the interruption"
    );
    {
        let mut active = active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.set_request(None);
        assert!(
            !active.operation_interrupted,
            "an interruption must not leak into a later request"
        );
    }
    assert!(matches!(
        command_rx.try_recv(),
        Ok(AgentCommand::Cancel {
            clear_pending: false
        })
    ));
}

#[tokio::test]
async fn shutdown_preempts_buffered_prompts_and_awaits_runner_exit() {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    command_tx
        .send(AgentCommand::Prompt {
            content: "queued-before-shutdown".to_string(),
            attachments: Vec::new(),
            kind: PromptKind::Prompt,
            queue_id: Some(41),
            managed_request_lineage: None,
            managed_inference_authorization: None,
        })
        .expect("queue prompt");
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let runner_exited = Arc::new(AtomicBool::new(false));
    let runner_exited_in_task = Arc::clone(&runner_exited);
    let shutdown_token = CancellationToken::new();
    let runner_shutdown_token = shutdown_token.clone();
    let runner_handle = tokio::spawn(async move {
        assert!(
            recv_command_or_shutdown(&runner_shutdown_token, &mut command_rx)
                .await
                .is_none(),
            "priority shutdown must win over a buffered prompt"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        runner_exited_in_task.store(true, Ordering::SeqCst);
    });
    let agent = super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: Arc::new(Mutex::new(ActiveCancellation::default())),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token,
        runner_handle: Some(runner_handle),
    };

    tokio::time::timeout(std::time::Duration::from_secs(2), agent.shutdown())
        .await
        .expect("shutdown lifecycle barrier timed out");
    assert!(
        runner_exited.load(Ordering::SeqCst),
        "shutdown returned before the runner exited"
    );
}

#[tokio::test]
async fn shutdown_drops_an_in_flight_side_question() {
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let shutdown = CancellationToken::new();
    let trigger = shutdown.clone();
    let started = Arc::new(AtomicBool::new(false));
    let started_in_future = Arc::clone(&started);
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_in_future = Arc::clone(&dropped);
    let side_question = async move {
        let _drop_probe = DropProbe(dropped_in_future);
        started_in_future.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
    };
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        trigger.cancel();
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        await_side_question_or_shutdown(&shutdown, side_question),
    )
    .await
    .expect("side question shutdown must not wait for provider completion");

    assert!(result.is_none());
    assert!(started.load(Ordering::SeqCst));
    assert!(
        dropped.load(Ordering::SeqCst),
        "shutdown must drop the provider stream future"
    );
}

#[test]
fn agent_cancel_interrupts_an_approval_wait_without_dropping_the_request() {
    let request_token = CancellationToken::new();
    let approval_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: None,
        approval: Some(approval_token.clone()),
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let agent = super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: active.clone(),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token: CancellationToken::new(),
        runner_handle: None,
    };

    agent.cancel_keep_queue();

    assert!(approval_token.is_cancelled());
    assert!(!request_token.is_cancelled());
    assert!(
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_interrupted
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(AgentCommand::Cancel {
            clear_pending: false
        })
    ));
}

#[tokio::test]
async fn agent_cancel_keeps_tool_batch_cleanup_alive_between_operations() {
    let request_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: None,
        approval: None,
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let agent = super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: active.clone(),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token: CancellationToken::new(),
        runner_handle: None,
    };

    agent.cancel_keep_queue();

    assert!(!request_token.is_cancelled());
    assert!(
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_interrupted
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(AgentCommand::Cancel {
            clear_pending: false
        })
    ));
}

#[test]
fn every_main_request_prompt_kind_uses_atomic_activation() {
    assert!(prompt_kind_starts_main_request(PromptKind::Prompt));
    assert!(prompt_kind_starts_main_request(PromptKind::Steer));
    assert!(prompt_kind_starts_main_request(PromptKind::FollowUp));
    assert!(!prompt_kind_starts_main_request(PromptKind::SideQuestion));
}

#[test]
fn cancellation_promotes_later_main_request_prompt_kinds() {
    assert!(should_defer_prompt_command(PromptKind::Prompt, false));
    assert!(!should_defer_prompt_command(PromptKind::Steer, false));
    assert!(!should_defer_prompt_command(PromptKind::FollowUp, false));

    assert!(should_defer_prompt_command(PromptKind::Prompt, true));
    assert!(should_defer_prompt_command(PromptKind::Steer, true));
    assert!(should_defer_prompt_command(PromptKind::FollowUp, true));
    assert!(!should_defer_prompt_command(PromptKind::SideQuestion, true));
}

#[test]
fn queued_user_message_starts_fresh_turn_scoped_safety_state() {
    let args = serde_json::json!({"command": "rm -rf /tmp/whatever"});
    let mut denials = DenialMemory::new();
    denials.record("bash", &args);
    let previous_epoch = denials.epoch();

    let mut reminders = ReminderEngine::new();
    reminders.observe_batch(&[ReminderToolOutcome {
        tool: "bash".to_string(),
        success: false,
        open_todos: None,
    }]);
    let mut step_budget = TurnStepBudget::new(4);
    step_budget.record_step();

    begin_queued_user_turn(&mut reminders, &mut denials, &mut step_budget);

    assert_eq!(denials.epoch(), previous_epoch + 1);
    assert!(!denials.was_refused("bash", &args));
    assert_eq!(reminders.context().tool_calls, 0);
    assert_eq!(step_budget.executed(), 0);
}

#[test]
fn cancellation_queued_after_receive_cancels_the_activated_request() {
    let active = Arc::new(Mutex::new(ActiveCancellation::default()));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    command_tx
        .send(AgentCommand::Cancel {
            clear_pending: true,
        })
        .expect("cancel command should queue");

    let request_token = {
        let mut activation = active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let request_token = activation.activate_request();
        if matches!(command_rx.try_recv(), Ok(AgentCommand::Cancel { .. })) {
            request_token.cancel();
        }
        request_token
    };

    assert!(
        request_token.is_cancelled(),
        "a cancel queued after direct receive must be consumed at activation"
    );
}

#[test]
fn cancellation_waiting_on_prompt_activation_lock_cancels_new_request() {
    let active = Arc::new(Mutex::new(ActiveCancellation::default()));
    let mut activation = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cancel_started = Arc::new(std::sync::Barrier::new(2));
    let active_for_cancel = Arc::clone(&active);
    let cancel_started_in_thread = Arc::clone(&cancel_started);
    let cancel_thread = std::thread::spawn(move || {
        cancel_started_in_thread.wait();
        cancel_active_operation(&active_for_cancel);
    });

    cancel_started.wait();
    let request_token = activation.activate_request();
    drop(activation);
    cancel_thread
        .join()
        .expect("cancellation thread must finish");

    assert!(
        request_token.is_cancelled(),
        "cancellation racing prompt activation must cancel the installed request token"
    );
}

#[test]
fn tool_batch_exit_consumes_a_late_interruption() {
    let request_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: None,
        approval: None,
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));

    cancel_active_operation(&active);
    assert!(!request_token.is_cancelled());
    let interrupted = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .finish_tool_batch();

    assert!(interrupted);
    let active = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(!active.tool_batch_active);
    assert!(!active.operation_interrupted);
}

#[test]
fn mutating_tool_terminal_drain_sticks_until_batch_cleanup() {
    let mut active = ActiveCancellation {
        tool_batch_active: true,
        ..ActiveCancellation::default()
    };
    active.set_tool(Some(CancellationToken::new()), true);
    active.set_tool(None, false);

    assert!(
        active.terminal_drain_required,
        "clearing the active token must not drop truthful terminal cleanup"
    );

    active.finish_tool_batch();
    assert!(
        !active.terminal_drain_required,
        "the terminal drain boundary ends with batch cleanup"
    );
}

#[test]
fn persistent_todo_requires_terminal_drain_without_approval() {
    let executor = runtime_test_host_handle();
    let args = serde_json::json!({"goal": "ship", "items": []});

    assert!(
        native_tool_requires_terminal_drain(&executor, "todo", &args),
        "todo persists its store even though it is auto-approved"
    );
}

#[test]
fn legacy_bash_mutations_require_terminal_drain() {
    let executor = runtime_test_host_handle();

    for command in [
        "find . -delete",
        "git branch -D obsolete",
        "git remote set-url origin https://example.invalid/repo.git",
        "printf x | tee target",
        "sed -i 's/a/b/' target",
    ] {
        let args = serde_json::json!({"command": command});
        assert!(
            native_tool_requires_terminal_drain(&executor, "bash", &args),
            "current effect analysis must classify legacy mutation: {command}"
        );
    }
}

#[test]
fn read_only_background_wait_does_not_require_terminal_drain() {
    let executor = runtime_test_host_handle();
    let args = serde_json::json!({
        "action": "waitForRotation",
        "taskId": "task-1",
        "timeoutMs": 60_000
    });

    assert!(
        !native_tool_requires_terminal_drain(&executor, "background_tasks", &args),
        "bounded shutdown must not become an unbounded wait for observation-only actions"
    );
}

#[test]
fn read_only_github_actions_do_not_require_terminal_drain() {
    let executor = runtime_test_host_handle();
    for (tool, action) in [
        ("gh_pr", "view"),
        ("gh_pr", "list"),
        ("gh_pr", "checks"),
        ("gh_pr", "diff"),
        ("gh_issue", "view"),
        ("gh_issue", "list"),
        ("gh_repo", "view"),
    ] {
        let args = serde_json::json!({"action": action});
        assert!(
            !native_tool_requires_terminal_drain(&executor, tool, &args),
            "{tool} {action} is observation-only"
        );
    }
}

#[test]
fn each_queued_prompt_consumes_only_its_own_staged_skills() {
    // A single shared staged value let a prompt run with instructions only
    // a later queued prompt triggered. Keyed entries are consumed by the
    // prompt they belong to, in whatever order the queue drains.
    let mut staged: HashMap<u64, (u64, String)> = HashMap::new();
    staged.insert(1, (0, "base + skill-a".to_string()));
    staged.insert(2, (0, "base + skill-a + skill-b".to_string()));

    // A steer jumps the queue and runs second-in-line first.
    assert_eq!(
        staged_system_prompt_to_apply(staged.remove(&2), 0).as_deref(),
        Some("base + skill-a + skill-b")
    );
    // The earlier prompt still gets its own prompt, without skill-b.
    assert_eq!(
        staged_system_prompt_to_apply(staged.remove(&1), 0).as_deref(),
        Some("base + skill-a"),
        "an earlier prompt must not inherit a later prompt's skills"
    );
    assert!(staged.is_empty(), "each entry is consumed once");
}

#[test]
fn applying_one_staged_prompt_does_not_invalidate_its_queued_sibling() {
    let mut staged = HashMap::from([
        (1, (4, "base + skill-a".to_string())),
        (2, (4, "base + skill-a + skill-b".to_string())),
    ]);
    let authority_revision = 4;
    let mut runtime_revision = 9;
    let mut system_prompt = None;

    assert!(apply_staged_system_prompt(
        &mut staged,
        1,
        authority_revision,
        &mut system_prompt,
        &mut runtime_revision,
    ));
    assert_eq!(system_prompt.as_deref(), Some("base + skill-a"));
    assert_eq!(runtime_revision, 10);
    assert!(apply_staged_system_prompt(
        &mut staged,
        2,
        authority_revision,
        &mut system_prompt,
        &mut runtime_revision,
    ));
    assert_eq!(system_prompt.as_deref(), Some("base + skill-a + skill-b"));
    assert_eq!(runtime_revision, 11);
}

#[test]
fn a_prompt_that_activates_nothing_still_gets_its_own_entry() {
    // Skipping the staging for a prompt with no new activations left it
    // with no entry, so a steer that jumped the queue and applied its own
    // skills first left them in place for the prompt behind it. An empty
    // activation set is a statement about that prompt, not an absence.
    let mut staged: HashMap<u64, (u64, String)> = HashMap::new();
    staged.insert(1, (0, "base".to_string()));
    staged.insert(2, (0, "base + steer-skill".to_string()));
    staged.insert(3, (0, "base + steer-skill".to_string()));

    // The steer jumps ahead and applies its own skills.
    assert_eq!(
        staged_system_prompt_to_apply(staged.remove(&2), 0).as_deref(),
        Some("base + steer-skill")
    );
    // The prompt behind it activated nothing and must fall back to its own
    // snapshot, not keep the steer's.
    assert_eq!(
        staged_system_prompt_to_apply(staged.remove(&1), 0).as_deref(),
        Some("base"),
        "a prompt that activates nothing must not inherit the steer's skills"
    );
    // A prompt queued after the steer legitimately carries its skills.
    assert_eq!(
        staged_system_prompt_to_apply(staged.remove(&3), 0).as_deref(),
        Some("base + steer-skill")
    );
}

#[test]
fn a_staged_queued_prompt_applies_only_while_it_is_current() {
    let staged = Some((3, "with skills".to_string()));
    assert_eq!(
        staged_system_prompt_to_apply(staged.clone(), 3).as_deref(),
        Some("with skills")
    );
    assert_eq!(
        staged_system_prompt_to_apply(staged, 4),
        None,
        "an authoritative update after the staging supersedes it"
    );
    assert_eq!(staged_system_prompt_to_apply(None, 0), None);
}

#[test]
fn codex_native_operations_are_named_for_policy_hooks() {
    assert_eq!(
        codex_native_policy_tool("item/fileChange/requestApproval"),
        "codex_file_change"
    );
    assert_eq!(
        codex_native_policy_tool("applyPatchApproval"),
        "codex_file_change"
    );
    assert_eq!(
        codex_native_policy_tool("item/commandExecution/requestApproval"),
        "codex_command_execution"
    );
    assert_eq!(
        codex_native_policy_tool("execCommandApproval"),
        "codex_command_execution"
    );
}

#[test]
fn approved_codex_native_completion_emits_one_receipt_bearing_tool_end() {
    let mut correlations = HashMap::from([(
        "item-write-1".to_owned(),
        CodexNativeToolCorrelation {
            call_id: "call-write-1".to_owned(),
            tool_name: "codex_file_change".to_owned(),
        },
    )]);
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "turnId": "turn-1",
            "item": {
                "id": "item-write-1",
                "type": "fileChange",
                "status": "completed"
            }
        })),
    };

    let event = project_codex_native_completion(&notification, &mut correlations, None)
        .expect("approved native write completion must project");
    assert!(
        correlations.is_empty(),
        "completion has one idempotency owner"
    );
    assert!(matches!(
        event,
        FromAgent::ToolEnd {
            call_id,
            success: true,
            result: Some(ToolResult { success: true, .. }),
            receipt: Some(receipt),
        } if call_id == "call-write-1"
            && receipt.call_id == "call-write-1"
            && receipt.tool_name == "codex_file_change"
    ));
    assert!(
        project_codex_native_completion(&notification, &mut correlations, None).is_none(),
        "replayed item/completed must not emit a duplicate ToolEnd"
    );
}

#[test]
fn completed_file_change_preserves_paths_for_item_id_only_approval() {
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "item": {
                "id": "item-completed-write",
                "type": "fileChange",
                "status": "completed",
                "changes": [{
                    "path": "/tmp/workspace/src.rs",
                    "kind": {"type": "update", "content": "patched"}
                }]
            }
        })),
    };
    let mut known = HashMap::new();

    remember_codex_file_change_completion_paths(&notification, &mut known);

    let approval_paths =
        codex_native_file_change_paths(&json!({"itemId": "item-completed-write"}), Some(&known));
    assert_eq!(approval_paths, ["/tmp/workspace/src.rs"]);
}

#[test]
fn yolo_policy_approval_correlates_one_named_native_completion() {
    assert!(
        !codex_native_approval_requires_user(ApprovalMode::Yolo),
        "Yolo follows the approved_policy branch without a ToolCall prompt"
    );
    let params = json!({"itemId": "item-yolo-write"});
    let mut correlations = HashMap::new();
    remember_approved_codex_native_operation(
        Some(&params),
        "call-yolo-write",
        "codex_file_change",
        &mut correlations,
    );
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "item": {
                "id": "item-yolo-write",
                "type": "fileChange",
                "status": "completed"
            }
        })),
    };

    let terminals = [notification.clone(), notification]
        .into_iter()
        .filter_map(|notification| {
            project_codex_native_completion(&notification, &mut correlations, None)
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        terminals.as_slice(),
        [FromAgent::ToolEnd {
            call_id,
            success: true,
            receipt: Some(receipt),
            ..
        }] if call_id == "call-yolo-write"
            && receipt.call_id == "call-yolo-write"
            && receipt.tool_name == "codex_file_change"
            && receipt.status == crate::agent::ExecutionStatus::Succeeded
    ));
    assert!(correlations.is_empty());
}

#[test]
fn replayed_native_completion_emits_exactly_one_named_success_wire_terminal() {
    let mut correlations = HashMap::from([(
        "item-write-wire".to_owned(),
        CodexNativeToolCorrelation {
            call_id: "call-write-wire".to_owned(),
            tool_name: "codex_file_change".to_owned(),
        },
    )]);
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "item": {
                "id": "item-write-wire",
                "type": "fileChange",
                "status": "completed"
            }
        })),
    };

    let terminals = [notification.clone(), notification]
        .into_iter()
        .filter_map(|notification| {
            project_codex_native_completion(&notification, &mut correlations, None)
        })
        .collect::<Vec<_>>();

    assert!(matches!(
        terminals.as_slice(),
        [FromAgent::ToolEnd {
            call_id,
            success: true,
            receipt: Some(receipt),
            ..
        }] if call_id == "call-write-wire"
            && receipt.call_id == "call-write-wire"
            && receipt.tool_name == "codex_file_change"
    ));
}

#[test]
fn uncorrelated_codex_native_completion_cannot_claim_tool_success() {
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "item": {
                "id": "different-item",
                "type": "fileChange",
                "status": "completed"
            }
        })),
    };
    let mut correlations = HashMap::from([(
        "approved-item".to_owned(),
        CodexNativeToolCorrelation {
            call_id: "approved-call".to_owned(),
            tool_name: "codex_file_change".to_owned(),
        },
    )]);

    assert!(project_codex_native_completion(&notification, &mut correlations, None).is_none());
    assert!(correlations.contains_key("approved-item"));
}

#[test]
fn preapproval_codex_native_completion_is_deferred_until_approval() {
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "item": {
                "id": "item-before-approval",
                "type": "fileChange",
                "status": "completed"
            }
        })),
    };
    let mut pending = HashMap::new();
    let mut correlations = HashMap::new();

    assert!(
        project_or_defer_codex_native_completion(
            &notification,
            &mut correlations,
            &mut pending,
            None,
        )
        .is_none()
    );
    assert_eq!(pending.get("item-before-approval"), Some(&true));

    let approval = json!({"itemId": "item-before-approval"});
    remember_approved_codex_native_operation(
        Some(&approval),
        "call-before-approval",
        "codex_file_change",
        &mut correlations,
    );
    let terminal = project_deferred_codex_native_completion(
        Some(&approval),
        &mut pending,
        &mut correlations,
        None,
    )
    .expect("the deferred completion must emit after approval correlation");

    assert!(matches!(
        terminal,
        FromAgent::ToolEnd {
            call_id,
            success: true,
            receipt: Some(receipt),
            ..
        } if call_id == "call-before-approval"
            && receipt.call_id == "call-before-approval"
            && receipt.tool_name == "codex_file_change"
    ));
    assert!(pending.is_empty());
    assert!(correlations.is_empty());
    assert!(
        project_deferred_codex_native_completion(
            Some(&approval),
            &mut pending,
            &mut correlations,
            None,
        )
        .is_none()
    );
}

#[test]
fn nonzero_codex_command_completion_projects_failure() {
    let mut correlations = HashMap::from([(
        "item-command-1".to_owned(),
        CodexNativeToolCorrelation {
            call_id: "call-command-1".to_owned(),
            tool_name: "codex_command_execution".to_owned(),
        },
    )]);
    let notification = crate::codex_app_server::Notification {
        method: "item/completed".to_owned(),
        params: Some(json!({
            "item": {
                "id": "item-command-1",
                "type": "commandExecution",
                "exitCode": 17
            }
        })),
    };

    assert!(matches!(
        project_codex_native_completion(&notification, &mut correlations, None),
        Some(FromAgent::ToolEnd {
            call_id,
            success: false,
            result: Some(ToolResult { success: false, .. }),
            receipt: Some(receipt),
        }) if call_id == "call-command-1"
            && receipt.call_id == "call-command-1"
            && receipt.tool_name == "codex_command_execution"
    ));
    assert!(correlations.is_empty());
}

#[test]
fn a_codex_native_operation_is_charged_its_generated_payload() {
    // Codex runs commandExecution and fileChange itself, so these never
    // appear as `ToolCall` and were charged nothing at all.
    let patch = "x".repeat(4_000);
    let charged = codex_native_operation_chars(Some(&serde_json::json!({
        "changes": {"src/main.rs": patch},
    })));

    assert!(
        charged > 4_000,
        "the generated patch must be charged, got {charged}"
    );
    assert_eq!(
        codex_native_operation_chars(None),
        0,
        "an operation with no params carries no model output"
    );
}

#[test]
fn a_read_only_policy_maps_to_the_codex_read_only_sandbox() {
    use maestro_sandbox::SandboxPolicy;

    assert_eq!(
        codex_sandbox_mode(Some(&SandboxPolicy::ReadOnly)).as_deref(),
        Some("read-only")
    );
    assert_eq!(
        codex_sandbox_mode(Some(&SandboxPolicy::DangerFullAccess)).as_deref(),
        Some("danger-full-access")
    );
    assert_eq!(
        codex_sandbox_mode(Some(&SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        }))
        .as_deref(),
        Some("workspace-write")
    );
    assert_eq!(codex_sandbox_mode(None), None);
}

#[test]
fn a_read_only_policy_declines_codex_native_mutation() {
    use maestro_sandbox::SandboxPolicy;

    // Every subagent runs in Yolo, because a delegated child cannot answer
    // an approval prompt. Without this the approval mode alone decided,
    // and a read-only child had its native exec and file-change requests
    // auto-accepted.
    assert!(config_denies_mutation(Some(&SandboxPolicy::ReadOnly)));
    assert!(!config_denies_mutation(Some(
        &SandboxPolicy::DangerFullAccess
    )));
    assert!(!config_denies_mutation(None));
}

#[test]
fn codex_native_mutations_are_normalized_for_the_action_firewall() {
    let command_sets = codex_native_firewall_arg_sets(
        "item/commandExecution/requestApproval",
        Some(&json!({"command": "rm -rf /"})),
        None,
    );
    assert_eq!(command_sets.len(), 1);
    assert_eq!(command_sets[0].0, "bash");
    assert_eq!(command_sets[0].1["command"], "rm -rf /");

    let file_sets = codex_native_firewall_arg_sets(
        "item/fileChange/requestApproval",
        Some(&json!({"path": "/etc/passwd", "content": "x"})),
        None,
    );
    assert_eq!(file_sets.len(), 1);
    assert_eq!(file_sets[0].0, "write");
    assert_eq!(file_sets[0].1["file_path"], "/etc/passwd");

    let multi = codex_native_firewall_arg_sets(
        "item/fileChange/requestApproval",
        Some(&json!({
            "files": ["src/main.rs", "/etc/passwd"],
            "content": "x"
        })),
        None,
    );
    assert_eq!(multi.len(), 2);
    assert_eq!(multi[0].1["file_path"], "src/main.rs");
    assert_eq!(multi[1].1["file_path"], "/etc/passwd");

    let legacy = codex_native_firewall_arg_sets(
        "applyPatchApproval",
        Some(&json!({
            "fileChanges": {
                "src/ok.rs": {"type": "update"},
                "/etc/passwd": {"type": "update"}
            }
        })),
        None,
    );
    assert_eq!(legacy.len(), 2);

    let mut known = HashMap::new();
    remember_codex_file_change_item_paths(
        &json!({
            "itemId": "item-2",
            "path": "/tmp/workspace/ok.rs",
            "content": "safe"
        }),
        &mut known,
    );
    let correlated = codex_native_firewall_arg_sets(
        "item/fileChange/requestApproval",
        Some(&json!({"itemId": "item-2"})),
        Some(&known),
    );
    assert_eq!(correlated.len(), 1);
    assert_eq!(correlated[0].1["file_path"], "/tmp/workspace/ok.rs");

    // Path-sensitive policy hooks must receive the same correlated paths.
    let hook_args = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({"itemId": "item-2"})),
        &known,
    );
    assert_eq!(
        hook_args["paths"],
        json!(["/tmp/workspace/ok.rs"]),
        "itemId-only approvals must surface correlated paths to hooks"
    );
    assert_eq!(
        hook_args["fileChanges"]["/tmp/workspace/ok.rs"]["content"], "safe",
        "itemId-only approvals must replay cached patch metadata: {hook_args}"
    );
    // Path present but sparse: still replay cached content metadata.
    let already_pathed = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-2",
            "path": "/tmp/workspace/ok.rs"
        })),
        &known,
    );
    assert_eq!(
        already_pathed["fileChanges"]["/tmp/workspace/ok.rs"]["content"], "safe",
        "sparse path-only approvals must still receive cached metadata: {already_pathed}"
    );

    // Later notifications overwrite earlier metadata for the same path.
    let mut stale = HashMap::new();
    remember_codex_file_change_item_paths(
        &json!({
            "itemId": "item-stale",
            "path": "/tmp/workspace/x.rs",
            "content": "old",
            "kind": "update"
        }),
        &mut stale,
    );
    remember_codex_file_change_item_paths(
        &json!({
            "itemId": "item-stale",
            "path": "/tmp/workspace/x.rs",
            "content": "new"
        }),
        &mut stale,
    );
    assert_eq!(
        stale["item-stale"]["/tmp/workspace/x.rs"]["content"], "new",
        "later content must replace earlier content: {stale:?}"
    );
    assert_eq!(
        stale["item-stale"]["/tmp/workspace/x.rs"]["kind"], "update",
        "fields omitted from the later update must be retained: {stale:?}"
    );

    // Approval names only the rename source; cache also has the destination.
    let mut partial = HashMap::new();
    remember_codex_file_change_item_paths(
        &json!({
            "itemId": "item-partial",
            "changes": [{
                "path": "/tmp/workspace/src.rs",
                "kind": { "move_path": "/etc/passwd" }
            }]
        }),
        &mut partial,
    );
    let partial_merged = codex_native_file_change_paths(
        &json!({
            "itemId": "item-partial",
            "path": "/tmp/workspace/src.rs"
        }),
        Some(&partial),
    );
    assert!(
        partial_merged.iter().any(|p| p == "/tmp/workspace/src.rs"),
        "direct source must remain: {partial_merged:?}"
    );
    assert!(
        partial_merged.iter().any(|p| p == "/etc/passwd"),
        "cached destination must merge even when approval already has a path: {partial_merged:?}"
    );
    let partial_hook = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-partial",
            "path": "/tmp/workspace/src.rs"
        })),
        &partial,
    );
    assert!(
        partial_hook["paths"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|p| p == "/etc/passwd")),
        "hooks must see the cached destination: {partial_hook}"
    );

    // Existing fileChanges metadata must survive enrichment.
    let mut with_meta = HashMap::new();
    remember_codex_file_change_item_paths(
        &json!({
            "itemId": "item-meta",
            "path": "/tmp/workspace/extra.rs"
        }),
        &mut with_meta,
    );
    let meta_hook = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-meta",
            "fileChanges": {
                "/tmp/workspace/src.rs": {
                    "kind": { "move_path": "/tmp/workspace/dst.rs" },
                    "content": "patched"
                }
            }
        })),
        &with_meta,
    );
    assert_eq!(
        meta_hook["fileChanges"]["/tmp/workspace/src.rs"]["content"], "patched",
        "original fileChanges metadata must be preserved: {meta_hook}"
    );
    assert!(
        meta_hook["fileChanges"]
            .as_object()
            .is_some_and(|m| m.contains_key("/tmp/workspace/extra.rs")),
        "correlated missing paths must still be added: {meta_hook}"
    );

    // Snake-case file_changes must be updated in place (not only a new
    // camelCase field) so hooks that read the original alias see correlated
    // paths.
    let snake_hook = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-meta",
            "file_changes": {
                "/tmp/workspace/src.rs": {
                    "content": "snake-meta"
                }
            }
        })),
        &with_meta,
    );
    assert_eq!(
        snake_hook["file_changes"]["/tmp/workspace/src.rs"]["content"], "snake-meta",
        "snake-case metadata must be preserved: {snake_hook}"
    );
    assert!(
        snake_hook["file_changes"]
            .as_object()
            .is_some_and(|m| m.contains_key("/tmp/workspace/extra.rs")),
        "snake-case file_changes must include correlated paths: {snake_hook}"
    );
    assert_eq!(
        snake_hook["fileChanges"]["/tmp/workspace/src.rs"]["content"], "snake-meta",
        "canonical fileChanges always mirrors the complete set: {snake_hook}"
    );

    // Array aliases (files / changes) must also receive correlated paths.
    let files_hook = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-meta",
            "files": ["/tmp/workspace/src.rs"]
        })),
        &with_meta,
    );
    let files = files_hook["files"].as_array().cloned().unwrap_or_default();
    assert!(
        files
            .iter()
            .any(|v| v.as_str() == Some("/tmp/workspace/src.rs")),
        "original files entry preserved: {files_hook}"
    );
    assert!(
        files
            .iter()
            .any(|v| v.as_str() == Some("/tmp/workspace/extra.rs")),
        "correlated path must appear in files: {files_hook}"
    );
    assert!(
        files_hook["fileChanges"]
            .as_object()
            .is_some_and(|m| m.contains_key("/tmp/workspace/extra.rs")),
        "canonical fileChanges always present: {files_hook}"
    );

    let changes_hook = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-meta",
            "changes": [{
                "path": "/tmp/workspace/src.rs",
                "kind": { "update": {} },
                "content": "keep-me"
            }]
        })),
        &with_meta,
    );
    let changes = changes_hook["changes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        changes[0]["content"], "keep-me",
        "existing change metadata preserved: {changes_hook}"
    );
    assert!(
        changes
            .iter()
            .any(|c| c.get("path").and_then(Value::as_str) == Some("/tmp/workspace/extra.rs")),
        "correlated path must appear in changes: {changes_hook}"
    );

    // Notification-shaped payload (paths arrive before the approval RPC).
    let mut from_notification = HashMap::new();
    remember_codex_file_change_item_paths(
        &json!({
            "item": {
                "id": "item-3",
                "type": "fileChange",
                "path": "/tmp/workspace/from-notification.rs"
            }
        }),
        &mut from_notification,
    );
    assert!(
        from_notification
            .get("item-3")
            .is_some_and(|paths| paths.contains_key("/tmp/workspace/from-notification.rs")),
        "notification paths must be cached under item id: {from_notification:?}"
    );

    // Cached change metadata must be replayed for itemId-only approvals.
    let mut kind_cache = HashMap::new();
    remember_codex_file_change_item_paths(
        &json!({
            "itemId": "item-kind",
            "changes": [{
                "path": "/tmp/workspace/delete-me.rs",
                "kind": "delete",
                "diff": "-gone"
            }]
        }),
        &mut kind_cache,
    );
    let kind_hook = codex_native_policy_hook_args(
        "item/fileChange/requestApproval",
        Some(&json!({"itemId": "item-kind"})),
        &kind_cache,
    );
    assert_eq!(
        kind_hook["fileChanges"]["/tmp/workspace/delete-me.rs"]["kind"], "delete",
        "cached kind must be replayed: {kind_hook}"
    );
    assert_eq!(
        kind_hook["fileChanges"]["/tmp/workspace/delete-me.rs"]["diff"], "-gone",
        "cached diff must be replayed: {kind_hook}"
    );

    // Rename/move destinations must be checked, not only sources.
    let move_paths = codex_native_file_change_paths(
        &json!({
            "itemId": "item-4",
            "changes": [{
                "path": "/tmp/workspace/src.rs",
                "kind": { "move_path": "/etc/passwd" }
            }]
        }),
        None,
    );
    assert!(
        move_paths.iter().any(|p| p == "/tmp/workspace/src.rs"),
        "source path required: {move_paths:?}"
    );
    assert!(
        move_paths.iter().any(|p| p == "/etc/passwd"),
        "move destination required: {move_paths:?}"
    );
    // Single-path { path, move_path } must also enumerate the destination.
    let single_move = codex_native_file_change_paths(
        &json!({
            "path": "/tmp/workspace/src.rs",
            "move_path": "/etc/passwd"
        }),
        None,
    );
    assert!(
        single_move.iter().any(|p| p == "/tmp/workspace/src.rs"),
        "single-path source required: {single_move:?}"
    );
    assert!(
        single_move.iter().any(|p| p == "/etc/passwd"),
        "single-path move destination required: {single_move:?}"
    );
    let single_kind_move = codex_native_file_change_paths(
        &json!({
            "path": "/tmp/workspace/src.rs",
            "kind": { "move_path": "/etc/shadow" }
        }),
        None,
    );
    assert!(
        single_kind_move.iter().any(|p| p == "/etc/shadow"),
        "single-path kind.move_path destination required: {single_kind_move:?}"
    );
}

#[test]
fn a_restrictive_tool_profile_declines_codex_native_mutation() {
    // A code-role child with profile tools [read, grep] still has a
    // writable sandbox; the allowlist is what must stop Codex-native
    // commandExecution and fileChange from running outside that set.
    let read_only: HashSet<String> = ["read", "grep"].into_iter().map(str::to_owned).collect();
    assert!(
        codex_native_denied_by_active_tools("item/commandExecution/requestApproval", &read_only)
            .is_some()
    );
    assert!(
        codex_native_denied_by_active_tools("item/fileChange/requestApproval", &read_only)
            .is_some()
    );

    let with_bash: HashSet<String> = ["bash", "read"].into_iter().map(str::to_owned).collect();
    assert!(
        codex_native_denied_by_active_tools("item/commandExecution/requestApproval", &with_bash)
            .is_none()
    );
    assert!(
        codex_native_denied_by_active_tools("item/fileChange/requestApproval", &with_bash)
            .is_some()
    );

    let with_write: HashSet<String> = ["write", "read"].into_iter().map(str::to_owned).collect();
    assert!(
        codex_native_denied_by_active_tools("item/fileChange/requestApproval", &with_write)
            .is_none()
    );
}

#[test]
fn stale_codex_tool_calls_are_denied_by_the_current_active_allowlist() {
    let empty = HashSet::new();
    let allowed = HashSet::from([String::from("read"), String::from("write")]);

    assert!(codex_tool_call_denied_by_active_tools("write", &empty).is_some());
    assert!(codex_tool_call_denied_by_active_tools("write", &allowed).is_none());
    assert!(codex_tool_call_denied_by_active_tools("READ", &allowed).is_none());
}

#[test]
fn model_output_spills_only_when_read_is_model_visible() {
    let host = runtime_test_host_handle();
    let without_read = HashSet::from([String::from("bash")]);
    assert!(
        model_tool_spill_dir_for_active_tools(
            Some(&host),
            &without_read,
            "/tmp/work",
            Some("session"),
            true,
        )
        .is_none()
    );

    let with_read = HashSet::from([String::from("bash"), String::from("read")]);
    assert!(
        model_tool_spill_dir_for_active_tools(
            Some(&host),
            &with_read,
            "/tmp/work",
            Some("session"),
            true,
        )
        .is_some()
    );
    assert!(
        model_tool_spill_dir_for_active_tools(Some(&host), &with_read, "/tmp/work", None, true)
            .is_none(),
        "sessionless runs have no cleanup owner and must keep output bounded inline"
    );
}

#[test]
fn tool_search_activation_does_not_expand_the_current_codex_turn_allowlist() {
    let turn_start = HashSet::from([String::from("tool_search"), String::from("read")]);
    let mut live_after_tool_search = turn_start.clone();
    live_after_tool_search.insert("write".to_owned());

    assert!(
        codex_tool_call_denied_by_active_tools("write", &turn_start).is_some(),
        "same-turn calls must use the turn-start snapshot"
    );
    assert!(
        codex_tool_call_denied_by_active_tools("write", &live_after_tool_search).is_none(),
        "the live set may contain a tool activated for the next turn"
    );
}

#[test]
fn output_allowance_is_unchanged_without_a_cumulative_budget() {
    assert_eq!(output_token_allowance(16_384, None, 0), 16_384);
    assert_eq!(
        output_token_allowance(16_384, None, 1_000_000),
        16_384,
        "spend is only meaningful against a budget"
    );
}

#[test]
fn output_allowance_shrinks_to_the_unspent_budget() {
    // The runner owns this arithmetic so a delegated run cannot be granted
    // its whole allowance again on every request past a tool boundary.
    assert_eq!(output_token_allowance(4_096, Some(4_096), 0), 4_096);
    assert_eq!(output_token_allowance(4_096, Some(4_096), 4_000), 96);
    assert_eq!(
        output_token_allowance(4_096, Some(65_536), 0),
        4_096,
        "a budget above the per-request limit does not raise the limit"
    );
}

#[test]
fn a_spent_budget_still_yields_a_valid_request() {
    assert_eq!(
        output_token_allowance(4_096, Some(4_096), 4_096),
        1,
        "providers reject max_tokens: 0"
    );
    assert_eq!(output_token_allowance(4_096, Some(4_096), 9_000), 1);
}

#[test]
fn local_output_allowance_fits_the_estimated_request_in_live_context() {
    assert_eq!(
        clamp_output_to_remaining_context(4_096, 8_192, 5_000),
        Some(3_128)
    );
    assert_eq!(
        clamp_output_to_remaining_context(2_048, 8_192, 1_000),
        Some(2_048),
        "remaining context must not raise the configured output cap"
    );
    assert_eq!(
        clamp_output_to_remaining_context(4_096, 8_192, 9_000),
        None,
        "an input-filled context must fail before provider dispatch"
    );
}

#[test]
fn clear_pending_cancel_drops_only_prompts_stashed_before_boundary() {
    let mut deferred_commands = VecDeque::from([
        AgentCommand::SetThinking {
            enabled: true,
            budget: 1024,
        },
        AgentCommand::Prompt {
            content: "before cancel".to_string(),
            attachments: Vec::new(),
            kind: PromptKind::Prompt,
            queue_id: Some(1),
            managed_request_lineage: None,
            managed_inference_authorization: None,
        },
        AgentCommand::Prompt {
            content: "steer before cancel".to_string(),
            attachments: Vec::new(),
            kind: PromptKind::Steer,
            queue_id: Some(2),
            managed_request_lineage: None,
            managed_inference_authorization: None,
        },
        AgentCommand::Prompt {
            content: "follow-up before cancel".to_string(),
            attachments: Vec::new(),
            kind: PromptKind::FollowUp,
            queue_id: Some(3),
            managed_request_lineage: None,
            managed_inference_authorization: None,
        },
        AgentCommand::Prompt {
            content: "side question before cancel".to_string(),
            attachments: Vec::new(),
            kind: PromptKind::SideQuestion,
            queue_id: Some(4),
            managed_request_lineage: None,
            managed_inference_authorization: None,
        },
    ]);

    assert_eq!(clear_stashed_prompts(&mut deferred_commands), 3);

    deferred_commands.push_back(AgentCommand::Prompt {
        content: "after cancel".to_string(),
        attachments: Vec::new(),
        kind: PromptKind::Prompt,
        queue_id: Some(5),
        managed_request_lineage: None,
        managed_inference_authorization: None,
    });
    assert!(matches!(
        deferred_commands.pop_front(),
        Some(AgentCommand::SetThinking {
            enabled: true,
            budget: 1024
        })
    ));
    assert!(matches!(
        deferred_commands.pop_front(),
        Some(AgentCommand::Prompt {
            content,
            kind: PromptKind::SideQuestion,
            queue_id: Some(4),
            ..
        }) if content == "side question before cancel"
    ));
    assert!(matches!(
        deferred_commands.pop_front(),
        Some(AgentCommand::Prompt {
            content,
            queue_id: Some(5),
            ..
        }) if content == "after cancel"
    ));
    assert!(deferred_commands.is_empty());
}

// ─────────────────────────────────────────────────────────────────────
// `tool_requires_approval` -- the single decision point behind the
// dual-executor fix (issues #3149, #3156). These exercise it directly
// as a pure function so the safety-critical gate has coverage that
// doesn't depend on spinning up a real provider/streaming loop.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn safe_mode_requires_approval_even_for_a_selective_safe_command() {
    // Regression test for #3149: before the fix, this gate ignored
    // `ApprovalMode` entirely and used the Selective heuristic no
    // matter what, so Safe mode never actually gated anything the
    // Selective heuristic would have auto-approved (e.g. `ls`).
    let executor = runtime_policy_host_handle();
    let args = serde_json::json!({"command": "ls -la"});

    // Sanity check: Selective mode alone would NOT require approval
    // for this command (this is what made the old gate look correct
    // in the default mode while being wrong in Safe mode).
    assert!(
        !tool_requires_approval(
            ApprovalMode::Selective,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );

    assert!(
        tool_requires_approval(
            ApprovalMode::Safe,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
}

#[test]
fn code_authority_suppresses_only_eligible_prompts_and_preserves_refusal() {
    let executor = runtime_test_host_handle();
    let args = serde_json::json!({"command": "cargo build"});
    let mut denials = DenialMemory::new();
    for mode in [ApprovalMode::Selective, ApprovalMode::Yolo] {
        assert!(
            !tool_requires_approval(
                mode,
                false,
                &NativeFirewallVerdict::Allow,
                &executor,
                "bash",
                &args,
                &denials
            )
            .requires_approval()
        );
    }
    assert!(
        tool_requires_approval(
            ApprovalMode::Safe,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &args,
            &denials
        )
        .requires_approval()
    );
    assert!(
        tool_requires_approval(
            ApprovalMode::Selective,
            true,
            &NativeFirewallVerdict::Allow,
            &executor,
            "external",
            &args,
            &denials
        )
        .requires_approval()
    );
    denials.record("bash", &args);
    assert!(
        tool_requires_approval(
            ApprovalMode::Selective,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &args,
            &denials
        )
        .is_repeat_refusal()
    );
}

#[test]
fn yolo_mode_never_requires_approval_for_native_tools() {
    let executor = runtime_policy_host_handle();
    // Even a command the Selective heuristic would flag as risky must
    // not require approval in Yolo mode -- "auto-approve ALL tool
    // calls" is the documented contract of `ApprovalMode::Yolo`.
    let risky_args = serde_json::json!({"command": "rm -rf /tmp/whatever"});
    assert!(
        tool_requires_approval(
            ApprovalMode::Selective,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &risky_args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
    assert!(
        !tool_requires_approval(
            ApprovalMode::Yolo,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &risky_args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
}

#[test]
fn yolo_mode_still_requires_approval_for_sandbox_bypass_requests() {
    // Waiving the native sandbox must always be a decision a human
    // explicitly makes, so a `bypass_sandbox` request is the one
    // exception to Yolo's "auto-approve ALL tool calls" contract.
    let executor = runtime_sandbox_host_handle();
    let bypass_args = serde_json::json!({"command": "ls -la", "bypass_sandbox": true});
    assert!(
        tool_requires_approval(
            ApprovalMode::Yolo,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &bypass_args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );

    // Without an active sandbox policy the flag is meaningless and Yolo
    // auto-approves as usual.
    let unsandboxed = runtime_test_host_handle();
    assert!(
        !tool_requires_approval(
            ApprovalMode::Yolo,
            false,
            &NativeFirewallVerdict::Allow,
            &unsandboxed,
            "bash",
            &bypass_args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
}

#[test]
fn empty_bash_is_rewritten_before_approval_and_execution() {
    let (args, rewritten) =
        normalize_post_hook_tool_args("BASH", serde_json::json!({"command": " \n\t"}));

    assert!(rewritten);
    assert_eq!(args, serde_json::json!({"command": "pwd"}));

    let executor = runtime_test_host_handle();
    assert!(
        !tool_requires_approval(
            ApprovalMode::Yolo,
            false,
            &NativeFirewallVerdict::Allow,
            &executor,
            "bash",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
}

#[test]
fn hook_modified_empty_bash_is_normalized_before_approval_and_execution() {
    let hook_result = NativeHookResult::ModifyInput {
        new_input: serde_json::json!({"command": " \n\t"}),
    };
    let NativeHookResult::ModifyInput { new_input } = hook_result else {
        unreachable!("test constructs a ModifyInput result");
    };

    let (args, rewritten) = normalize_post_hook_tool_args("bash", new_input);

    assert!(rewritten);
    assert_eq!(args, serde_json::json!({"command": "pwd"}));
    assert!(
        runtime_test_host_handle()
            .missing_required("bash", &args)
            .is_empty(),
        "hook-produced arguments must be normalized before validation"
    );
}

#[test]
fn external_tool_always_requires_approval_regardless_of_mode() {
    // Callers embedding external tools (the SDK / ambient-agent path)
    // own execution and their own approval policy; even Yolo must not
    // let this runner treat the call as pre-approved.
    let executor = runtime_test_host_handle();
    let args = serde_json::json!({});
    assert!(
        tool_requires_approval(
            ApprovalMode::Yolo,
            true,
            &NativeFirewallVerdict::Allow,
            &executor,
            "some_external_tool",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
}

#[test]
fn firewall_soft_hold_is_bypassed_only_in_yolo_mode() {
    let executor = runtime_policy_host_handle();
    let args = serde_json::json!({"command": "ls"});
    let verdict = NativeFirewallVerdict::RequireApproval {
        reason: "test".to_string(),
    };

    // A firewall soft-hold forces approval in Safe and Selective mode
    // even for a command the per-tool heuristic alone would allow.
    assert!(
        tool_requires_approval(
            ApprovalMode::Selective,
            false,
            &verdict,
            &executor,
            "bash",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
    assert!(
        tool_requires_approval(
            ApprovalMode::Safe,
            false,
            &verdict,
            &executor,
            "bash",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
    // Yolo bypasses the soft hold too (matches the pre-existing
    // `app.rs` semantics this logic was migrated from).
    assert!(
        !tool_requires_approval(
            ApprovalMode::Yolo,
            false,
            &verdict,
            &executor,
            "bash",
            &args,
            &DenialMemory::new(),
        )
        .requires_approval()
    );
}

#[test]
fn codex_native_approval_modes_match_the_interactive_policy() {
    assert!(!codex_native_approval_requires_user(ApprovalMode::Yolo));
    assert!(codex_native_approval_requires_user(ApprovalMode::Selective));
    assert!(codex_native_approval_requires_user(ApprovalMode::Safe));
}

#[test]
fn test_config_with_custom_model() {
    let config = NativeAgentConfig {
        model: "gpt-5.1-codex-max".to_string(),
        max_tokens: 8192,
        system_prompt: Some("You are a helpful assistant.".to_string()),
        thinking_enabled: true,
        thinking_budget: 5000,
        cwd: "/tmp".to_string(),
        ..NativeAgentConfig::default()
    };
    assert_eq!(config.model, "gpt-5.1-codex-max");
    assert_eq!(config.max_tokens, 8192);
    assert!(config.thinking_enabled);
    assert_eq!(config.thinking_budget, 5000);
}

#[test]
fn test_thinking_config() {
    let thinking = ThinkingConfig::enabled(10000);
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, 10000);
}

#[test]
fn test_tool_definition_clone() {
    let tool_def = ToolDefinition {
        tool: Tool::new("test", "A test tool").with_schema(serde_json::json!({
            "type": "object",
            "properties": {}
        })),
        requires_approval: true,
    };
    let cloned = tool_def.clone();
    assert_eq!(cloned.tool.name, "test");
    assert!(cloned.requires_approval);
}

fn profile_fixture_definitions() -> HashMap<String, ToolDefinition> {
    [
        "bash",
        "read",
        "write",
        "tool_search",
        "explore",
        "get_rlm_context",
        "set_rlm_context",
        "websearch",
        "vscode_get_definition",
        "get_goal",
        "update_goal",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_owned(),
            ToolDefinition {
                tool: Tool::new(name, format!("fixture {name}")).with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {}
                })),
                requires_approval: false,
            },
        )
    })
    .collect()
}

#[test]
fn fast_tool_profile_is_small_but_has_an_escape_hatch() {
    let definitions = profile_fixture_definitions();
    let active = super::initial_active_tool_names(
        super::ToolProfile::Fast,
        &definitions,
        &HashSet::new(),
        None,
        super::ExternalToolSchemaPolicy::Eager,
    );

    assert!(active.contains("read"));
    assert!(active.contains("bash"));
    assert!(active.contains("tool_search"));
    assert!(active.contains("explore"));
    assert!(!active.contains("get_rlm_context"));
    assert!(!active.contains("set_rlm_context"));
    assert!(!active.contains("websearch"));
    assert!(!active.contains("vscode_get_definition"));
}

#[test]
fn all_tool_profile_preserves_every_registered_tool() {
    let definitions = profile_fixture_definitions();
    let active = super::initial_active_tool_names(
        super::ToolProfile::All,
        &definitions,
        &HashSet::new(),
        None,
        super::ExternalToolSchemaPolicy::Eager,
    );

    assert_eq!(active.len(), definitions.len());
}

#[test]
fn minimal_tool_profile_starts_with_native_file_search() {
    let mut definitions = profile_fixture_definitions();
    for name in ["grep", "glob"] {
        let mut definition = definitions["read"].clone();
        definition.tool = Tool::new(name, format!("fixture {name}"));
        definitions.insert(name.to_owned(), definition);
    }
    let active = super::initial_active_tool_names(
        super::ToolProfile::Minimal,
        &definitions,
        &HashSet::new(),
        None,
        super::ExternalToolSchemaPolicy::Eager,
    );
    assert!(active.contains("grep"));
    assert!(active.contains("glob"));
    assert!(!active.contains("get_goal"));
    assert!(!active.contains("explore"));
}

#[test]
fn minimal_tool_profile_keeps_discovery_and_explicit_capabilities() {
    let definitions = profile_fixture_definitions();
    let active = super::initial_active_tool_names(
        super::ToolProfile::Minimal,
        &definitions,
        &HashSet::new(),
        None,
        super::ExternalToolSchemaPolicy::Eager,
    );
    assert_eq!(
        active,
        HashSet::from(["read", "write", "bash", "tool_search"].map(String::from))
    );
    let explicit = HashSet::from([String::from("vscode_get_definition")]);
    let external = HashSet::from([String::from("external_fixture")]);
    let active = super::initial_active_tool_names(
        super::ToolProfile::Minimal,
        &definitions,
        &external,
        Some(&explicit),
        super::ExternalToolSchemaPolicy::Eager,
    );
    assert!(active.contains("vscode_get_definition"));
    assert!(active.contains("external_fixture"));
    assert!(!active.contains("explore"));
    for name in ["read", "bash", "edit", "write", "tool_search", "ask_user"] {
        assert!(super::ToolProfile::Minimal.includes(name));
    }
}

#[test]
fn minimal_tool_discovery_retains_fast_authority_restrictions() {
    assert!(super::tool_search_profile_allows(
        super::ToolProfile::Minimal,
        "grep",
        &HashSet::new()
    ));
    assert!(!super::tool_search_profile_allows(
        super::ToolProfile::Minimal,
        "set_rlm_context",
        &HashSet::new()
    ));
    assert!(super::tool_search_profile_allows(
        super::ToolProfile::Minimal,
        "set_rlm_context",
        &HashSet::from([String::from("set_rlm_context")])
    ));
    let governed = profile_fixture_definitions()
        .into_iter()
        .filter(|(name, _)| name == "read")
        .collect();
    assert_eq!(
        super::initial_active_tool_names(
            super::ToolProfile::Minimal,
            &governed,
            &HashSet::new(),
            None,
            super::ExternalToolSchemaPolicy::Eager,
        ),
        HashSet::from([String::from("read")])
    );
}

#[test]
fn fast_tool_search_blocks_rlm_unless_explicitly_allowed() {
    assert!(!super::tool_search_profile_allows(
        super::ToolProfile::Fast,
        "set_rlm_context",
        &HashSet::new()
    ));
    assert!(super::tool_search_profile_allows(
        super::ToolProfile::Fast,
        "set_rlm_context",
        &HashSet::from([String::from("set_rlm_context")])
    ));
    assert!(super::tool_search_profile_allows(
        super::ToolProfile::All,
        "set_rlm_context",
        &HashSet::new()
    ));
}

#[test]
fn all_tool_profile_cannot_widen_a_governed_registry() {
    let allowed = HashSet::from([String::from("read")]);
    let definitions = profile_fixture_definitions()
        .into_iter()
        .filter(|(name, _)| allowed.contains(name))
        .collect::<HashMap<_, _>>();
    let active = super::initial_active_tool_names(
        super::ToolProfile::All,
        &definitions,
        &HashSet::new(),
        Some(&allowed),
        super::ExternalToolSchemaPolicy::Eager,
    );

    assert_eq!(active, HashSet::from([String::from("read")]));
}

#[test]
fn explicit_allowed_tools_override_fast_profile() {
    let definitions = profile_fixture_definitions();
    let allowed = HashSet::from([String::from("websearch"), String::from("set_rlm_context")]);
    let active = super::initial_active_tool_names(
        super::ToolProfile::Fast,
        &definitions,
        &HashSet::new(),
        Some(&allowed),
        super::ExternalToolSchemaPolicy::Eager,
    );

    assert!(active.contains("websearch"));
    assert!(active.contains("set_rlm_context"));
}

#[test]
fn tool_visibility_matches_model_schema_filtering() {
    assert!(!super::tool_execution::tool_is_visible_to_model(
        "vscode_get_definition",
        false,
        false
    ));
    assert!(super::tool_execution::tool_is_visible_to_model(
        "vscode_get_definition",
        false,
        true
    ));
    assert!(!super::tool_execution::tool_is_visible_to_model(
        "get_goal", false, true
    ));
    assert!(super::tool_execution::tool_is_visible_to_model(
        "get_goal", true, false
    ));
    assert!(super::tool_execution::tool_is_visible_to_model(
        "websearch",
        false,
        false
    ));
}

#[test]
fn compact_tool_for_model_strips_legacy_alias_properties() {
    let tool = Tool::new("read", "Read a file").with_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file" },
            "file_path": { "type": "string", "description": "Legacy alias for path" },
            "offset": { "type": "number", "description": "Start line" }
        },
        "required": ["path"]
    }));
    let compact = compact_tool_for_model(tool);
    let props = compact.input_schema["properties"].as_object().unwrap();
    assert!(props.contains_key("path"));
    assert!(props.contains_key("offset"));
    assert!(!props.contains_key("file_path"));
}

#[test]
fn test_thinking_config_with_budget() {
    let config = NativeAgentConfig {
        model: "claude-opus-4-5-20251101".to_string(),
        max_tokens: 16384,
        system_prompt: None,
        thinking_enabled: true,
        thinking_budget: 15000,
        cwd: ".".to_string(),
        ..NativeAgentConfig::default()
    };

    let thinking = if config.thinking_enabled {
        Some(ThinkingConfig::enabled(config.thinking_budget))
    } else {
        None
    };

    assert!(thinking.is_some());
    let thinking = thinking.unwrap();
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, 15000);
}

#[test]
fn test_from_agent_variants() {
    // Test that FromAgent variants serialize/deserialize correctly
    let ready = FromAgent::Ready {
        model: "claude-sonnet".to_string(),
        provider: "Anthropic".to_string(),
    };
    if let FromAgent::Ready { model, provider } = ready {
        assert_eq!(model, "claude-sonnet");
        assert_eq!(provider, "Anthropic");
    } else {
        panic!("Expected Ready variant");
    }

    let chunk = FromAgent::ResponseChunk {
        response_id: "resp_123".to_string(),
        content: "Hello".to_string(),
        is_thinking: false,
    };
    if let FromAgent::ResponseChunk {
        content,
        is_thinking,
        ..
    } = chunk
    {
        assert_eq!(content, "Hello");
        assert!(!is_thinking);
    } else {
        panic!("Expected ResponseChunk variant");
    }
}

#[test]
fn test_tool_result_structure() {
    let success_result = ToolResult::success("Command executed successfully");
    assert!(success_result.success);
    assert!(!success_result.output.is_empty());
    assert!(success_result.error.is_none());

    let error_result = ToolResult::failure("Permission denied");
    assert!(!error_result.success);
    assert!(error_result.output.is_empty());
    assert!(error_result.error.is_some());
}

#[test]
fn test_parse_tool_input_empty_ok() {
    let parsed = parse_tool_input("noop", "").unwrap();
    assert_eq!(parsed, serde_json::json!({}));
}

#[test]
fn test_parse_tool_input_invalid_json() {
    let err = parse_tool_input("bash", "{invalid").unwrap_err();
    assert!(err.contains("bash"));
    assert!(err.contains("Failed to parse tool input JSON"));
}

#[test]
fn fatal_stream_error_discards_completed_tool_calls() {
    let mut assistant_content = vec![
        ContentBlock::Text {
            text: "partial response".to_string(),
        },
        ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            input: serde_json::json!({"command": "true"}),
            gemini_context: None,
        },
    ];
    let mut pending_tool_calls = vec![(
        "call-1".to_string(),
        "bash".to_string(),
        serde_json::json!({"command": "true"}),
        None,
    )];

    abort_pending_tools_after_stream_error(&mut assistant_content, &mut pending_tool_calls);

    assert!(pending_tool_calls.is_empty());
    assert!(
        assistant_content
            .iter()
            .all(|block| !matches!(block, ContentBlock::ToolUse { .. }))
    );
}

#[test]
fn lifecycle_tool_args_preserve_opaque_credential_references() {
    let vault = CredentialVault::new();
    let reference = vault.store("secret-value", crate::agent::CredentialType::Token);
    let args = serde_json::json!({
        "task": format!("Use {reference} in the child")
    });

    assert_eq!(
        tool_args_for_execution("spawn_subagent", &args, &vault),
        args,
        "durable lifecycle records must retain the opaque reference"
    );
    assert_eq!(
        tool_args_for_execution("bash", &args, &vault),
        serde_json::json!({"task": "Use secret-value in the child"})
    );
}

#[test]
fn provider_history_resolves_references_without_mutating_durable_history() {
    let vault = CredentialVault::new();
    let reference = vault.store("secret-value", crate::agent::CredentialType::Token);
    let history = vec![Message {
        role: Role::User,
        content: MessageContent::Text(format!("Use {reference} in the child")),
    }];

    let resolved = resolve_provider_history(&history, &vault).expect("history should resolve");
    let MessageContent::Text(resolved_text) = &resolved[0].content else {
        panic!("expected resolved text message");
    };
    assert_eq!(resolved_text, "Use secret-value in the child");

    let MessageContent::Text(durable_text) = &history[0].content else {
        panic!("expected vaulted text message");
    };
    assert_eq!(durable_text, &format!("Use {reference} in the child"));
}

#[test]
fn provider_history_without_references_reuses_shared_storage() {
    let history = Arc::new(vec![Message {
        role: Role::User,
        content: MessageContent::text("ordinary history"),
    }]);
    let resolved = resolve_provider_history_shared(&history, &CredentialVault::new())
        .expect("history should be reusable");

    assert!(Arc::ptr_eq(&history, &resolved));
}

#[tokio::test]
async fn test_wait_for_tool_response_buffers_out_of_order() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut coordinator = ToolResponseCoordinator::new(rx);
    let cancel = CancellationToken::new();
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    tx.send((
        "id-2".to_string(),
        true,
        None,
        ExecutionSource::Native,
        Some(second_tx),
    ))
    .unwrap();
    tx.send((
        "id-1".to_string(),
        false,
        None,
        ExecutionSource::RemoteClient,
        Some(first_tx),
    ))
    .unwrap();
    let first = coordinator.wait_for_tool_response("id-1", &cancel).await;
    assert!(matches!(
        first,
        ToolResponseWait::Response((false, None, ExecutionSource::RemoteClient))
    ));
    assert_eq!(first_rx.await.unwrap(), ToolResponseConsumption::Accepted);
    let second = coordinator.wait_for_tool_response("id-2", &cancel).await;
    assert!(matches!(
        second,
        ToolResponseWait::Response((true, None, ExecutionSource::Native))
    ));
    assert_eq!(second_rx.await.unwrap(), ToolResponseConsumption::Accepted);
}

#[test]
fn later_auto_approved_calls_defer_behind_an_approval_boundary() {
    assert_eq!(
        deferred_tool_call_disposition(true, false),
        Some(DeferredToolCallDisposition::AwaitApproval)
    );
    assert_eq!(
        deferred_tool_call_disposition(false, true),
        Some(DeferredToolCallDisposition::Execute)
    );
    assert_eq!(deferred_tool_call_disposition(false, false), None);
}

#[test]
fn approved_inline_environment_must_match_at_execution_boundary() {
    let approved = HashMap::from([
        ("PATH".to_string(), "/usr/bin".to_string()),
        ("GIT_ASKPASS".to_string(), "/approved/helper".to_string()),
    ]);
    let same = approved.clone();
    let changed = HashMap::from([
        ("PATH".to_string(), "/usr/bin".to_string()),
        ("GIT_ASKPASS".to_string(), "/new/helper".to_string()),
    ]);

    assert_eq!(
        approved_inline_env_change_rejection(Some(&approved), Some(&same)),
        None,
    );
    assert_eq!(
        approved_inline_env_change_rejection(Some(&approved), Some(&changed)),
        Some(
            "Inline tool environment changed after approval; retry to review refreshed environment"
        ),
    );
    assert_eq!(
        approved_inline_env_change_rejection(Some(&approved), None),
        Some(
            "Inline tool environment changed after approval; retry to review refreshed environment"
        ),
    );
    assert_eq!(
        approved_inline_env_change_rejection(None, Some(&changed)),
        None,
    );
}

#[test]
fn approval_event_carries_the_execution_snapshot_without_serializing_it() {
    let args = serde_json::json!({});
    let approved_env = HashMap::from([
        ("PATH".to_string(), "/approved/bin".to_string()),
        (
            "DATABASE_URL".to_string(),
            "postgres://user:password@example.test/db".to_string(),
        ),
    ]);
    let call = ToolCallContext {
        call_id: "call-inline".to_string(),
        tool_name: "deploy".to_string(),
        args: args.clone(),
        safe_args: args.clone(),
        extra_context: None,
        pre_hook_args: args,
        initial_firewall_verdict: NativeFirewallVerdict::RequireApproval {
            reason: "inline tool".to_string(),
        },
        approval_inline_env: Some(InlineToolApprovalContext {
            command: "./deploy.sh".to_string(),
            source_path: ".composer/tools.json".to_string(),
            source_label: "project".to_string(),
            cwd: "/workspace".to_string(),
            environment: approved_env.clone(),
            shell: "/bin/sh".to_string(),
            shell_arg: "-c".to_string(),
        }),
    };

    let event = deferred_tool_call_event(&call, true);
    let carried_env = match &event {
        FromAgent::ToolCall {
            approval_inline_env,
            ..
        } => approval_inline_env.as_ref(),
        event => panic!("expected ToolCall, got {event:?}"),
    };
    assert_eq!(
        carried_env.map(|context| &context.environment),
        Some(&approved_env)
    );

    // The raw snapshot exists only on the in-process handoff. Approval
    // rendering applies its credential redactor before displaying values,
    // and serialization must not leak the pre-redaction map.
    let serialized = serde_json::to_string(&event).expect("serialize ToolCall");
    assert!(!serialized.contains("approval_inline_env"));
    assert!(!serialized.contains("password"));
}

#[test]
fn deferred_hook_block_emits_one_terminal_receipt() {
    let args = serde_json::json!({"command": "touch later"});
    let call = ToolCallContext {
        call_id: "call-later".to_string(),
        tool_name: "bash".to_string(),
        args: args.clone(),
        safe_args: args.clone(),
        extra_context: None,
        pre_hook_args: args,
        initial_firewall_verdict: NativeFirewallVerdict::Allow,
        approval_inline_env: None,
    };
    let (events, result) = deferred_hook_block(&call, "state changed".to_string(), true, None);

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, FromAgent::ToolCall { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, FromAgent::ToolEnd { .. }))
            .count(),
        1
    );
    assert!(events.iter().any(|event| matches!(
        event,
        FromAgent::ToolEnd {
            success: false,
            receipt: Some(_),
            ..
        }
    )));
    assert!(matches!(
        result,
        ContentBlock::ToolResult {
            is_error: Some(true),
            ..
        }
    ));
}

#[test]
fn approved_deferred_call_requires_fresh_consent_for_new_firewall_hold() {
    let original_hold = NativeFirewallVerdict::RequireApproval {
        reason: "existing hold".to_string(),
    };
    assert_eq!(
        deferred_approved_policy_rejection(
            &original_hold,
            NativeFirewallVerdict::RequireApproval {
                reason: "existing hold".to_string(),
            },
        ),
        None
    );
    assert_eq!(
        deferred_approved_policy_rejection(
            &NativeFirewallVerdict::Allow,
            NativeFirewallVerdict::RequireApproval {
                reason: "new PII state".to_string(),
            },
        ),
        Some(
            "Tool requires fresh approval after earlier tool execution: new PII state".to_string()
        )
    );
    assert_eq!(
        deferred_approved_policy_rejection(
            &original_hold,
            NativeFirewallVerdict::RequireApproval {
                reason: "changed hold".to_string(),
            },
        ),
        Some("Tool requires fresh approval after earlier tool execution: changed hold".to_string())
    );
    assert_eq!(
        deferred_approved_policy_rejection(
            &NativeFirewallVerdict::Allow,
            NativeFirewallVerdict::Block {
                reason: "blocked now".to_string(),
            },
        ),
        Some("blocked now".to_string())
    );
}

/// The doom-loop block the deferred path used to read straight off
/// `SafetyController` now arrives through the extension registry, with the
/// same reason text and the same terminal events.
#[test]
fn deferred_execution_rechecks_updated_safety_history() {
    let mut extensions = ExtensionRegistry::with_default_tenants();
    let args = serde_json::json!({"command": "printf test"});
    let call = ToolCallContext {
        call_id: "call-3".to_string(),
        tool_name: "bash".to_string(),
        args: args.clone(),
        safe_args: args.clone(),
        extra_context: None,
        pre_hook_args: args.clone(),
        initial_firewall_verdict: NativeFirewallVerdict::Allow,
        approval_inline_env: None,
    };
    let planned = |index: u64| ExtensionToolCallContext {
        turn_id: "turn-1".to_string(),
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        args_hash: stable_stringify(&call.safe_args),
        args: call.safe_args.clone(),
        call_index: index,
    };
    let executed = ExtensionToolResultContext {
        edit: None,
        turn_id: "turn-1".to_string(),
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        args_hash: stable_stringify(&call.safe_args),
        args: call.safe_args.clone(),
        is_error: false,
        duration_ms: 1,
    };

    assert_eq!(
        extensions.on_tool_call_planned(&planned(0)),
        ExtensionVerdict::Proceed
    );
    for _ in 0..2 {
        extensions.on_tool_result(&executed, &mut ToolResultPayload::default());
    }

    let reason = match extensions.on_tool_call_planned(&planned(2)) {
        ExtensionVerdict::Block { reason } => reason,
        other => panic!("expected the deferred re-check to block, got {other:?}"),
    };
    assert!(
        reason.contains("doom loop"),
        "block reason should name the doom loop: {reason}"
    );

    assert!(matches!(
        deferred_rejection_output_event(&call, "doom loop"),
        FromAgent::ToolOutput { call_id, content }
            if call_id == "call-3" && content == "doom loop"
    ));
    match deferred_safety_rejection_event(&call, "doom loop", None) {
        FromAgent::ToolEnd {
            call_id,
            success,
            result,
            receipt,
        } => {
            assert_eq!(call_id, "call-3");
            assert!(!success);
            assert!(result.is_some_and(|result| !result.success));
            assert_eq!(
                receipt.map(|receipt| receipt.status),
                Some(crate::agent::ExecutionStatus::Failed)
            );
        }
        event => panic!("expected terminal event, got {event:?}"),
    }
}

#[test]
fn cancelled_deferred_calls_emit_terminal_queued_receipts() {
    let call = ToolCallContext {
        call_id: "call-later".to_string(),
        tool_name: "bash".to_string(),
        args: serde_json::json!({"command": "touch later"}),
        safe_args: serde_json::json!({"command": "touch later"}),
        extra_context: None,
        pre_hook_args: serde_json::json!({"command": "touch later"}),
        initial_firewall_verdict: NativeFirewallVerdict::Allow,
        approval_inline_env: None,
    };

    let (event, result_block) =
        cancelled_deferred_tool(&call, "Skipped after request cancellation.", None);

    match event {
        FromAgent::ToolEnd {
            call_id,
            success,
            receipt: Some(receipt),
            ..
        } => {
            assert_eq!(call_id, "call-later");
            assert!(!success);
            assert_eq!(
                receipt.status,
                crate::agent::ExecutionStatus::Cancelled {
                    phase: ExecutionPhase::Queued
                }
            );
        }
        other => panic!("expected ToolEnd, got {other:?}"),
    }
    assert!(matches!(
        result_block,
        ContentBlock::ToolResult {
            tool_use_id,
            is_error: Some(true),
            ..
        } if tool_use_id == "call-later"
    ));
}

#[test]
fn interruption_before_deferred_suffix_closes_every_call() {
    let deferred_calls = ["call-first", "call-second"].into_iter().map(|call_id| {
        DeferredToolCall::Execute(ToolCallContext {
            call_id: call_id.to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "touch later"}),
            safe_args: serde_json::json!({"command": "touch later"}),
            extra_context: None,
            pre_hook_args: serde_json::json!({"command": "touch later"}),
            initial_firewall_verdict: NativeFirewallVerdict::Allow,
            approval_inline_env: None,
        })
    });
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut tool_results = Vec::new();

    let cancelled_ids = cancel_deferred_suffix(&event_tx, deferred_calls, &mut tool_results, None);

    assert_eq!(tool_results.len(), 2);
    assert_eq!(
        cancelled_ids,
        HashSet::from(["call-first".to_string(), "call-second".to_string()])
    );
    let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    assert_eq!(events.len(), 6);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, FromAgent::ToolCall { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, FromAgent::ToolEnd { .. }))
            .count(),
        2
    );
}

#[test]
fn cancelled_suffix_announces_only_previously_unannounced_execute_calls() {
    let args = serde_json::json!({"command": "touch later"});
    let make_call = |call_id: &str| ToolCallContext {
        call_id: call_id.to_string(),
        tool_name: "bash".to_string(),
        args: args.clone(),
        safe_args: args.clone(),
        extra_context: None,
        pre_hook_args: args.clone(),
        initial_firewall_verdict: NativeFirewallVerdict::Allow,
        approval_inline_env: None,
    };
    let deferred_calls = [
        DeferredToolCall::AwaitApproval(make_call("call-announced")),
        DeferredToolCall::Execute(make_call("call-unannounced")),
    ];
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut tool_results = Vec::new();

    cancel_deferred_suffix(&event_tx, deferred_calls, &mut tool_results, None);

    let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    let announced_ids: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            FromAgent::ToolCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(announced_ids, vec!["call-unannounced"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, FromAgent::ToolEnd { .. }))
            .count(),
        2
    );
    assert_eq!(tool_results.len(), 2);
}

#[test]
fn cancelled_suffix_discards_only_its_queued_approvals() {
    let cancelled_ids = HashSet::from([
        "call-cancelled-buffered".to_string(),
        "call-cancelled-queued".to_string(),
    ]);
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send((
        "call-cancelled-queued".to_string(),
        true,
        None,
        ExecutionSource::Native,
        None,
    ))
    .expect("queue cancelled approval");
    tx.send((
        "call-unrelated".to_string(),
        false,
        None,
        ExecutionSource::Native,
        None,
    ))
    .expect("queue unrelated approval");
    let mut coordinator = ToolResponseCoordinator::new(rx);
    tx.send((
        "call-cancelled-buffered".to_string(),
        true,
        None,
        ExecutionSource::Native,
        None,
    ))
    .expect("queue buffered cancelled approval");
    tx.send((
        "call-existing".to_string(),
        false,
        None,
        ExecutionSource::Native,
        None,
    ))
    .expect("queue existing approval");
    coordinator.drain_available();
    coordinator.discard_cancelled(&cancelled_ids);

    assert!(
        coordinator
            .take_pending_for_repair("call-cancelled-buffered")
            .is_none()
    );
    assert!(
        coordinator
            .take_pending_for_repair("call-cancelled-queued")
            .is_none()
    );
    assert!(matches!(
        coordinator.take_pending_for_repair("call-existing"),
        Some((false, None, ExecutionSource::Native))
    ));
    assert!(matches!(
        coordinator.take_pending_for_repair("call-unrelated"),
        Some((false, None, ExecutionSource::Native))
    ));
}

/// Regression test for #3149: once a call is genuinely awaiting approval
/// (`requires_approval == true`, which after the fix now also holds in
/// Safe mode -- see `safe_mode_requires_approval_even_for_a_selective_safe_command`),
/// a `(call_id, false, None)` denial -- exactly what `handle_tool_approval`
/// sends on Deny -- must resolve `wait_for_tool_response` (covered by
/// `test_wait_for_tool_response_buffers_out_of_order` above) into a
/// denied `ToolExecution` that reads as an error to the model, and must
/// never reach `execute_tool`. `run_loop` only calls `execute_tool` when
/// `approved` is true (see the `if approved && result.is_none()` branch);
/// this asserts the denied-branch value it builds instead.
#[test]
fn denied_tool_response_is_an_error_result_and_never_executes() {
    let (approved, result): (bool, Option<ToolResult>) = (false, None);
    assert!(!approved, "run_loop must not call execute_tool when denied");

    let execution = if approved {
        unreachable!("this test only covers the denied branch");
    } else {
        ToolExecution::denied("call-1", "bash", DenialReason::User)
    };
    assert!(result.is_none());
    assert!(execution.is_error());
    assert!(
        execution
            .model_content()
            .to_lowercase()
            .contains("denied by user")
    );
}

/// Every assistant `ToolUse` id must have exactly one matching `ToolResult`
/// in a user message that follows it - the invariant the OpenAI and
/// Anthropic serializers (and providers) rely on.
fn assert_tool_call_pairing(messages: &[Message]) {
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut tool_result_ids: Vec<String> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let MessageContent::Blocks(blocks) = &message.content else {
            continue;
        };
        for block in blocks {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    assert_eq!(message.role, Role::Assistant);
                    tool_use_ids.push(id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    assert_eq!(message.role, Role::User);
                    assert!(index > 0, "tool result cannot lead the history");
                    tool_result_ids.push(tool_use_id.clone());
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        tool_use_ids, tool_result_ids,
        "every tool call must have exactly one tool result, in order"
    );
}

fn assistant_tool_use_message(calls: &[(&str, &str)]) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(
            calls
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    input: serde_json::json!({}),
                    gemini_context: None,
                })
                .collect(),
        ),
    }
}

fn tool_result_blocks(message: &Message) -> Vec<(String, String, Option<bool>)> {
    match &message.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id.clone(), content.clone(), *is_error)),
                _ => None,
            })
            .collect(),
        MessageContent::Text(_) => Vec::new(),
    }
}

#[test]
fn test_repair_orphaned_tool_calls_synthesizes_missing_results() {
    // A turn cancelled after the assistant message was recorded but before
    // any tool result was appended (the Ctrl+C-during-bash repro).
    let mut messages = vec![
        Message {
            role: Role::User,
            content: MessageContent::Text("run sleep 120 via bash".to_string()),
        },
        assistant_tool_use_message(&[("call_1", "bash"), ("call_2", "read")]),
    ];
    let (_sender, receiver) = mpsc::unbounded_channel();
    let mut coordinator = ToolResponseCoordinator::new(receiver);

    repair_orphaned_tool_calls(&mut messages, &mut coordinator, None);

    assert_eq!(messages.len(), 3);
    let repairs = tool_result_blocks(&messages[2]);
    assert_eq!(repairs.len(), 2);
    assert_eq!(repairs[0].0, "call_1");
    assert_eq!(repairs[1].0, "call_2");
    for (_, content, is_error) in &repairs {
        assert_eq!(content, "Tool execution cancelled by user.");
        assert_eq!(*is_error, Some(true));
    }
    assert_tool_call_pairing(&messages);

    // The cancellation terminal path repairs before emitting its durable
    // snapshot, so the next process never restores an orphaned ToolUse.
    let snapshot = conversation_snapshot_event(&messages).expect("snapshot event");
    let FromAgent::ConversationSnapshot {
        messages: snapshot_messages,
        ..
    } = snapshot
    else {
        panic!("expected semantic snapshot");
    };
    assert_tool_call_pairing(&snapshot_messages);

    // A subsequent prompt must not leave the orphaned call in the middle
    // of the history: the pairing still holds after it is appended.
    messages.push(Message {
        role: Role::User,
        content: MessageContent::Text("next prompt".to_string()),
    });
    assert_tool_call_pairing(&messages);
}

#[test]
fn test_repair_orphaned_tool_calls_prefers_late_real_results() {
    // The app still delivers the cancelled tool's real outcome on the
    // tool-response channel; use it instead of a synthesized message.
    let mut messages = vec![assistant_tool_use_message(&[("call_1", "bash")])];
    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send((
            "call_1".to_string(),
            true,
            Some(ToolResult::failure("Command cancelled")),
            ExecutionSource::Native,
            None,
        ))
        .expect("queue late tool result");
    let mut coordinator = ToolResponseCoordinator::new(receiver);

    repair_orphaned_tool_calls(&mut messages, &mut coordinator, None);

    assert_eq!(messages.len(), 2);
    let repairs = tool_result_blocks(&messages[1]);
    assert_eq!(repairs.len(), 1);
    assert_eq!(repairs[0].0, "call_1");
    assert!(repairs[0].1.contains("Command cancelled"));
    assert_eq!(repairs[0].2, Some(true));
    assert!(coordinator.take_pending_for_repair("call_1").is_none());
    assert_tool_call_pairing(&messages);
}

#[test]
fn test_repair_orphaned_tool_calls_records_denials() {
    let mut messages = vec![assistant_tool_use_message(&[("call_1", "write")])];
    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send((
            "call_1".to_string(),
            false,
            None,
            ExecutionSource::Native,
            None,
        ))
        .expect("queue denial");
    let mut coordinator = ToolResponseCoordinator::new(receiver);

    repair_orphaned_tool_calls(&mut messages, &mut coordinator, None);

    let repairs = tool_result_blocks(&messages[1]);
    assert_eq!(repairs.len(), 1);
    assert_eq!(repairs[0].2, Some(true));
    assert_tool_call_pairing(&messages);
}

#[test]
fn test_repair_orphaned_tool_calls_merges_into_existing_result_message() {
    // Partial results were already recorded for call_1 when the turn was
    // cancelled; call_2's result must join the same user message.
    let mut messages = vec![
        assistant_tool_use_message(&[("call_1", "read"), ("call_2", "bash")]),
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "file contents".to_string(),
                is_error: Some(false),
            }]),
        },
    ];
    let (_sender, receiver) = mpsc::unbounded_channel();
    let mut coordinator = ToolResponseCoordinator::new(receiver);

    repair_orphaned_tool_calls(&mut messages, &mut coordinator, None);

    assert_eq!(messages.len(), 2);
    let results = tool_result_blocks(&messages[1]);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_1");
    assert_eq!(results[0].1, "file contents");
    assert_eq!(results[1].0, "call_2");
    assert_eq!(results[1].1, "Tool execution cancelled by user.");
    assert_tool_call_pairing(&messages);
}

#[test]
fn test_repair_orphaned_tool_calls_noop_on_paired_history() {
    let mut messages = vec![
        assistant_tool_use_message(&[("call_1", "read")]),
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "ok".to_string(),
                is_error: Some(false),
            }]),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Text("done".to_string()),
        },
    ];
    let original = messages.clone();
    let (_sender, receiver) = mpsc::unbounded_channel();
    let mut coordinator = ToolResponseCoordinator::new(receiver);

    repair_orphaned_tool_calls(&mut messages, &mut coordinator, None);

    assert_eq!(messages.len(), original.len());
    assert_tool_call_pairing(&messages);
}

// ─────────────────────────────────────────────────────────────────────
// Per-turn denial memory. A denied call with identical arguments must be
// refused from the earlier decision instead of prompting the user again.
// ─────────────────────────────────────────────────────────────────────

fn approval_decision_for(
    executor: &NativeExecutionHostHandle,
    args: &serde_json::Value,
    denials: &DenialMemory,
) -> ApprovalDecision {
    tool_requires_approval(
        ApprovalMode::Selective,
        false,
        &NativeFirewallVerdict::Allow,
        executor,
        "bash",
        args,
        denials,
    )
}

#[test]
fn a_refused_call_is_not_prompted_again_until_the_next_turn() {
    let executor = runtime_policy_host_handle();
    let args = serde_json::json!({"command": "rm -rf /tmp/whatever"});
    let mut denials = DenialMemory::new();

    assert_eq!(
        approval_decision_for(&executor, &args, &denials),
        ApprovalDecision::Required,
        "the first attempt must ask the user"
    );

    denials.record("bash", &args);
    assert_eq!(
        approval_decision_for(&executor, &args, &denials),
        ApprovalDecision::RefusedEarlierThisTurn,
        "an identical retry must not prompt again"
    );

    let different = serde_json::json!({"command": "rm -rf /tmp/other"});
    assert_eq!(
        approval_decision_for(&executor, &different, &denials),
        ApprovalDecision::Required,
        "a different command is a different decision"
    );

    denials.begin_turn();
    assert_eq!(
        approval_decision_for(&executor, &args, &denials),
        ApprovalDecision::Required,
        "the refusal must be retired at the turn boundary"
    );
}

#[test]
fn a_repeat_refusal_still_counts_as_requiring_approval() {
    let executor = runtime_policy_host_handle();
    let args = serde_json::json!({"command": "rm -rf /tmp/whatever"});
    let mut denials = DenialMemory::new();
    denials.record("bash", &args);
    let decision = approval_decision_for(&executor, &args, &denials);
    assert!(decision.requires_approval(), "a refused call must not run");
    assert!(decision.is_repeat_refusal());
}

#[test]
fn a_refusal_does_not_gate_a_call_that_never_needed_approval() {
    let executor = runtime_policy_host_handle();
    let args = serde_json::json!({"command": "ls -la"});
    let mut denials = DenialMemory::new();
    denials.record("bash", &args);
    // Selective mode auto-approves `ls`, so the memory is not consulted
    // and the call is unaffected by a stale refusal key.
    assert_eq!(
        approval_decision_for(&executor, &args, &denials),
        ApprovalDecision::NotRequired
    );
}

#[test]
fn the_repeat_refusal_message_names_the_tool_and_the_turn() {
    let message = repeat_refusal_message("bash");
    assert!(message.contains("bash"), "{message}");
    assert!(message.contains("earlier in this turn"), "{message}");
}

#[tokio::test]
async fn shutdown_preempts_retry_backoff() {
    let request_cancel = CancellationToken::new();
    let shutdown_token = CancellationToken::new();
    let shutdown = shutdown_token.clone();

    let waiting = tokio::spawn(async move {
        wait_for_retry_delay(
            std::time::Duration::from_mins(1),
            &request_cancel,
            &shutdown_token,
        )
        .await
    });
    shutdown.cancel();

    let completed = tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
        .await
        .expect("shutdown should preempt the retry timer")
        .expect("retry wait task should not panic");
    assert!(!completed);
}

#[tokio::test]
async fn billed_thinking_only_response_preserves_signature_before_response_end() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for thinking_only in [true, false] {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_scripted_provider_request(&mut stream).await);
            let mut events = vec![serde_json::json!({
                "type":"message_start", "message": {
                    "id":"signed-response", "type":"message", "role":"assistant",
                    "model":"claude-fable-5-1", "content":[],
                    "usage":{"input_tokens":5,"output_tokens":0}
                }
            })];
            if thinking_only {
                events.extend([
                    serde_json::json!({"type":"content_block_start","index":0,
                        "content_block":{"type":"thinking","thinking":""}}),
                    serde_json::json!({"type":"content_block_delta","index":0,
                        "delta":{"type":"thinking_delta","thinking":"private reasoning"}}),
                    serde_json::json!({"type":"content_block_delta","index":0,
                        "delta":{"type":"signature_delta","signature":"signed-reasoning"}}),
                ]);
            } else {
                events.extend([
                    serde_json::json!({"type":"content_block_start","index":0,
                        "content_block":{"type":"text","text":""}}),
                    serde_json::json!({"type":"content_block_delta","index":0,
                        "delta":{"type":"text_delta","text":"Done."}}),
                ]);
            }
            events.extend([
                serde_json::json!({"type":"content_block_stop","index":0}),
                serde_json::json!({"type":"message_delta",
                    "delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
                serde_json::json!({"type":"message_stop"}),
            ]);
            let body = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect::<String>();
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
        }
        requests
    });
    let workspace = tempfile::tempdir().unwrap();
    let (agent, mut events) = NativeAgent::new_with_test_client(
        NativeAgentConfig {
            model: "anthropic/claude-fable-5-1".into(),
            cwd: workspace.path().display().to_string(),
            ..NativeAgentConfig::default()
        },
        UnifiedClient::Anthropic(
            crate::ai::AnthropicClient::with_base_url("test-key", format!("http://{address}"))
                .unwrap(),
        ),
    )
    .unwrap();
    agent
        .prompt("Finish the answer.".into(), vec![])
        .await
        .unwrap();
    let signed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut content = None;
        loop {
            match events.recv().await {
                Some(FromAgent::LocalAssistantContent {
                    response_id,
                    content: blocks,
                }) => {
                    content = Some((response_id, blocks));
                }
                Some(FromAgent::ResponseEnd { response_id, .. }) => {
                    let (recorded_id, blocks) = content
                        .expect("provider blocks must precede the thinking-only response end");
                    assert_eq!(recorded_id, response_id);
                    break blocks;
                }
                Some(_) => {}
                None => panic!("missing response end"),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(signed.as_slice(), [ContentBlock::Thinking { thinking, signature }]
        if thinking == "private reasoning" && signature.as_deref() == Some("signed-reasoning"))
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    agent.shutdown().await;
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "assistant"
                    && message["content"][0]["signature"] == "signed-reasoning"
            }),
        "the steered retry must retain the signed provider block"
    );
}

#[test]
fn content_block_stop_retains_empty_signed_thinking_block() {
    let mut assistant_content = Vec::new();
    let mut current_thinking = String::new();

    append_completed_thinking_block(
        &mut assistant_content,
        &mut current_thinking,
        Some("claude-signature".to_string()),
    );

    assert!(matches!(
        assistant_content.as_slice(),
        [ContentBlock::Thinking { thinking, signature }]
            if thinking.is_empty() && signature.as_deref() == Some("claude-signature")
    ));
}

#[test]
fn content_block_stop_omits_empty_unsigned_thinking_block() {
    let mut assistant_content = Vec::new();
    let mut current_thinking = String::new();

    append_completed_thinking_block(&mut assistant_content, &mut current_thinking, None);

    assert!(assistant_content.is_empty());
}

#[tokio::test]
async fn model_identity_reaches_provider_with_custom_system_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_scripted_provider_request(&mut stream).await;
            captured.lock().unwrap().push(request);
            let body = chat_sse_response("identity-fixture", "Done.", false);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/fixture-vision".into(),
        system_prompt: Some("Help with the user's task.".into()),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    let mut host = RuntimeTestHost::new(config.cwd.clone(), client);
    host.model_capabilities.insert(
        "openai/fixture-vision".into(),
        NativeModelCapabilities {
            vision: Some(true),
            tool_calling: Some(true),
            reasoning: Some(false),
            context_tokens: Some(128_000),
            output_tokens: Some(16_384),
        },
    );
    host.model_capabilities.insert(
        "openai/fixture-text".into(),
        NativeModelCapabilities {
            vision: Some(false),
            tool_calling: Some(true),
            reasoning: None,
            context_tokens: Some(8_192),
            output_tokens: None,
        },
    );
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    for index in 0..2 {
        if index == 1 {
            agent.set_model("openai/fixture-text").unwrap();
            agent
                .set_system_prompt("Use the updated task instructions.")
                .unwrap();
        }
        agent.prompt("Say done.".into(), vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await.unwrap() {
                    FromAgent::TurnCompleted { .. } => break,
                    FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                        panic!("{message}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
    agent.shutdown().await;
    server.await.unwrap();
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for (index, model, prompt) in [
        (0, "fixture-vision", "Help with the user's task."),
        (1, "fixture-text", "Use the updated task instructions."),
    ] {
        let system = captured[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(system.contains(prompt));
        assert!(
            system.contains("Your model is provided by Deixic."),
            "{system}"
        );
        assert!(system.contains(&format!("\"openai/{model}\"")), "{system}");
        assert_eq!(
            system.matches("Your model is provided by Deixic.").count(),
            1
        );
        let capabilities = model_capabilities_from_prompt(system);
        assert_eq!(capabilities["vision"], index == 0);
        assert_eq!(
            capabilities["context_tokens"],
            if index == 0 { 128_000 } else { 8_192 }
        );
        assert_eq!(
            capabilities["output_tokens"],
            if index == 0 {
                serde_json::json!(16_384)
            } else {
                Value::Null
            }
        );
        assert_eq!(
            system
                .matches("Catalog-reported model capabilities")
                .count(),
            1
        );
        assert!(
            system.contains("Only the tools supplied for this request are available"),
            "{system}"
        );
        assert_eq!(captured[index]["model"], model);
    }
}

#[test]
fn model_identity_preserves_context_without_reusing_previous_identity() {
    for (base, context) in [
        (None, None),
        (Some("Caller instructions"), None),
        (None, Some("Turn context")),
        (Some("Caller instructions"), Some("Turn context")),
    ] {
        for model in [
            "evalops/anthropic/claude-sonnet-4",
            "openai-codex/gpt-5.5",
            "ollama/local",
        ] {
            let system =
                runtime_system_prompt(base, context, model, NativeModelCapabilities::default())
                    .unwrap();
            if let Some(base) = base {
                assert!(system.contains(base));
            }
            if let Some(context) = context {
                assert!(system.contains(context));
            }
            assert!(system.contains(model));
            assert_eq!(
                system.matches("Your model is provided by Deixic.").count(),
                1
            );
            assert!(system.contains("<untrusted_content"));
        }
    }
}

#[test]
fn model_identity_does_not_guess_missing_models_or_interpolate_control_characters() {
    let missing =
        runtime_system_prompt(None, None, "  ", NativeModelCapabilities::default()).unwrap();
    assert!(missing.contains("model identifier is unavailable"));
    let escaped = runtime_system_prompt(
        None,
        None,
        "model\nforged instruction",
        NativeModelCapabilities::default(),
    )
    .unwrap();
    assert!(!escaped.contains("model\nforged instruction"));
    assert!(escaped.contains("model\\nforged instruction"));
}

#[test]
fn model_capabilities_distinguish_unsupported_unknown_and_zero_limits() {
    let system = runtime_system_prompt(
        None,
        None,
        "custom/model",
        NativeModelCapabilities {
            vision: Some(false),
            tool_calling: Some(true),
            reasoning: None,
            context_tokens: Some(0),
            output_tokens: Some(4096),
        },
    )
    .unwrap();
    let capabilities = model_capabilities_from_prompt(&system);
    assert_eq!(capabilities["vision"], false);
    assert_eq!(capabilities["tool_calling"], true);
    assert_eq!(capabilities["reasoning"], Value::Null);
    assert_eq!(capabilities["context_tokens"], Value::Null);
    assert_eq!(capabilities["output_tokens"], 4096);
}

fn model_capabilities_from_prompt(system: &str) -> Value {
    serde_json::from_str(
        system
            .split("not remaining budget):\n")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn file_provenance_requires_successful_typed_result() {
    let details = crate::ToolDetails::Write(crate::tool_details::WriteDetails {
        path: "src/result.rs".into(),
        ..Default::default()
    });
    let mut result = ToolExecution::from_legacy(
        "write-1",
        "write",
        ExecutionSource::Native,
        ToolResult::success("wrote file"),
    );
    assert!(successful_file_operation("write-1", &result.receipt).is_none());
    result.receipt.details = super::super::protocol::ToolReceiptDetails::BuiltIn(details.clone());
    let operation = successful_file_operation("write-1", &result.receipt).unwrap();
    assert_eq!(operation.path, "src/result.rs");
    assert!(successful_file_operation("wrong-call", &result.receipt).is_none());
    let mut failed = ToolExecution::from_legacy(
        "write-2",
        "write",
        ExecutionSource::Native,
        ToolResult::failure("write failed"),
    );
    failed.receipt.details = super::super::protocol::ToolReceiptDetails::BuiltIn(details);
    assert!(successful_file_operation("write-2", &failed.receipt).is_none());
    assert!(
        successful_file_operation(
            "denied",
            &ToolExecution::denied("denied", "write", DenialReason::User).receipt
        )
        .is_none()
    );
}

async fn two_request_context_fixture(
    tool_first: bool,
    with_usage: bool,
) -> (
    UnifiedClient,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_scripted_provider_request(&mut stream).await;
            captured.lock().unwrap().push(request);
            let mut body = chat_sse_response("continuity", "Done.", tool_first && index == 0);
            if with_usage {
                let usage = json!({"id":"continuity","object":"chat.completion.chunk","created":0,"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":360,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":300}}});
                body = body.replace("data: [DONE]", &format!("data: {usage}\n\ndata: [DONE]"));
            }
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        }
    });
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    (client, requests, server)
}

#[tokio::test]
async fn final_tool_projection_bounds_hook_output_and_retains_full_capture() {
    let workspace = tempfile::tempdir().unwrap();
    let (client, requests, server) = two_request_context_fixture(true, false).await;
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        ..Default::default()
    };
    let mut host = RuntimeTestHost::new(config.cwd.clone(), client);
    host.post_tool_context = Some(format!(
        "{}unique-end-marker",
        "large tool context ".repeat(5000)
    ));
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    agent
        .set_session_context(Some("output-fixture".into()), "new", true)
        .unwrap();
    agent.prompt("Read the file".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::ContextCalibration { .. } => {
                    panic!("missing usage must stay unobserved")
                }
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    server.await.unwrap();
    let captured = requests.lock().unwrap();
    let messages = captured[1]["messages"].as_array().unwrap();
    let tool = messages.iter().find(|m| m["role"] == "tool").unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(tool.len() < 40_000);
    assert!(tool.contains("Truncated"));
    let path = workspace
        .path()
        .join(".maestro/output-fixture/full-output.txt");
    assert!(tool.contains(path.to_str().unwrap()));
    let full = std::fs::read_to_string(path).unwrap();
    assert!(full.len() > 40_000);
    assert!(full.contains("unique-end-marker"));
}

#[tokio::test]
async fn steering_queued_before_checkpoint_install_reaches_next_request_once() {
    let workspace = tempfile::tempdir().unwrap();
    let (client, requests, server) = two_request_context_fixture(false, false).await;
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(1024),
        ..Default::default()
    };
    let barrier = Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
        AtomicBool::new(false),
    ));
    let mut host = RuntimeTestHost::new(config.cwd.clone(), client);
    host.checkpoint_barrier = Some(Arc::clone(&barrier));
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    agent.set_steering_mode(QueueMode::One).unwrap();
    agent.replace_history(
        (0..20)
            .map(|index| Message {
                role: if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: MessageContent::text("earlier context ".repeat(100)),
            })
            .collect(),
    );
    agent.prompt("Continue".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), barrier.0.notified())
        .await
        .unwrap();
    agent
        .prompt_with_kind(
            "Correction: preserve the database".into(),
            vec![],
            PromptKind::Steer,
            None,
        )
        .await
        .unwrap();
    barrier.1.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let snapshot = agent.runtime_audit_snapshot();
    agent.shutdown().await;
    server.await.unwrap();
    assert!(
        snapshot
            .request_cache
            .unwrap()
            .cache_topology
            .unwrap()
            .generation
            >= 2
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1]
            .to_string()
            .matches("Correction: preserve the database")
            .count(),
        1
    );
}

#[tokio::test]
async fn calibration_is_bound_to_each_completed_primary_request() {
    let workspace = tempfile::tempdir().unwrap();
    let (client, _requests, server) = two_request_context_fixture(true, true).await;
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        ..Default::default()
    };
    let (agent, mut events) =
        new_runtime_test_agent_with_host(config.clone(), RuntimeTestHost::new(config.cwd, client))
            .unwrap();
    agent.prompt("Read the file".into(), vec![]).await.unwrap();
    let mut observations = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::ContextCalibration { observation } => observations.push(observation),
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    server.await.unwrap();
    assert_eq!(observations.len(), 2);
    assert_ne!(observations[0].request_id, observations[1].request_id);
    assert!(
        observations[1].estimated_input_tokens > observations[0].estimated_input_tokens,
        "accounting must come from each prepared history, not a stale earlier request"
    );
    for observation in observations {
        assert_eq!(observation.observed_input_tokens, 360);
        assert_eq!(observation.generation, 1);
        assert!(observation.estimated_input_tokens > 0);
    }
}

#[tokio::test]
async fn cancelled_prepared_compaction_does_not_duplicate_user_history() {
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(1024),
        ..Default::default()
    };
    let large_response = (0..512)
        .map(|i| format!("response-{i} "))
        .collect::<String>();
    let scripted = crate::ai::ScriptedClient::new(
        "cancel-compaction",
        vec![
            crate::ai::ScriptedResponse::text(large_response.clone()),
            crate::ai::ScriptedResponse::text(large_response.clone()),
        ],
    );
    let barrier = Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
        AtomicBool::new(false),
    ));
    let mut host = RuntimeTestHost::new(config.cwd.clone(), UnifiedClient::Scripted(scripted));
    host.checkpoint_barrier = Some(Arc::clone(&barrier));
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    let sentinel = "original-boundary: retain this context".to_owned();
    let history = vec![Message {
        role: Role::User,
        content: MessageContent::text(sentinel.clone()),
    }];
    let compactor = crate::agent::compaction::ContextCompactor::new(
        crate::agent::compaction::CompactionConfig::for_model("openai/gpt-4o", Some(1024)),
    );
    assert!(!compactor.should_auto_compact(&history));
    let mut completed = history.clone();
    completed.push(Message {
        role: Role::User,
        content: MessageContent::text("Continue"),
    });
    completed.push(Message {
        role: Role::Assistant,
        content: MessageContent::text(large_response),
    });
    assert!(compactor.should_auto_compact(&completed));
    assert!(compactor.compact_with_tokens(&completed).was_compacted());
    agent.replace_history(history);
    agent.prompt("Continue".into(), vec![]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), barrier.0.notified())
        .await
        .unwrap();
    agent.cancel();
    barrier.1.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if matches!(event, FromAgent::TurnInterrupted { .. }) {
                return;
            }
        }
        panic!("missing interruption");
    })
    .await
    .unwrap();
    agent.prompt("Resume".into(), vec![]).await.unwrap();
    let record = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::Compaction {
                    continuation: Some(record),
                    ..
                } => return record,
                FromAgent::ProviderError { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
        panic!("missing resumed compaction");
    })
    .await
    .unwrap();
    agent.shutdown().await;
    assert_eq!(
        record
            .user_requests
            .iter()
            .filter(|request| **request == sentinel)
            .count(),
        1,
        "a cancelled prepared checkpoint must not retain the original history twice"
    );
}

/// Manual soak test: drains the real actor stream, retains only scalar metrics,
/// and uses a scripted provider so no credentials or paid requests are needed.
#[tokio::test]
#[ignore = "manual repeated-turn memory measurement"]
async fn many_turns_memory_probe() {
    let turns: usize = std::env::var("MAESTRO_MEMORY_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let reset_every: usize = std::env::var("MAESTRO_MEMORY_RESET_EVERY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let workspace = tempfile::tempdir().unwrap();
    let scripted = crate::ai::ScriptedClient::new(
        "memory-soak",
        (0..turns)
            .map(|_| crate::ai::ScriptedResponse::text("Done."))
            .collect(),
    );
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(4096),
        ..Default::default()
    };
    let (agent, mut events) =
        new_runtime_test_agent(config, UnifiedClient::Scripted(scripted.clone())).unwrap();
    let mut messages_bytes = 0;
    let mut continuation_bytes = 0;
    let mut retained_requests = 0;
    let started = Instant::now();
    eprintln!(
        "MEMORY_PROBE pid={} turns={turns} reset_every={reset_every}",
        std::process::id()
    );
    tokio::time::timeout(Duration::from_secs(600), async {
        for turn in 0..turns {
            if reset_every > 0 && turn % reset_every == 0 {
                agent.clear_history();
                continuation_bytes = 0;
                retained_requests = 0;
            }
            agent.prompt(format!("request-{turn}: {}", "Keep the task local and preserve the evidence. ".repeat(8)), vec![]).await.unwrap();
            while let Some(event) = events.recv().await {
                match event {
                    FromAgent::ConversationSnapshot { messages, .. } => {
                        messages_bytes = serde_json::to_vec(&messages).unwrap().len();
                    }
                    FromAgent::Compaction { continuation: Some(record), .. } => {
                        continuation_bytes = serde_json::to_vec(&record).unwrap().len();
                        retained_requests = record.user_requests.len();
                        assert!(retained_requests <= if reset_every > 0 { reset_every } else { turn + 1 }, "compaction duplicated requests");
                    }
                    FromAgent::TurnCompleted { .. } => break,
                    FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => panic!("turn {turn}: {message}"),
                    FromAgent::TurnInterrupted { reason, .. } => panic!("turn {turn}: {reason}"),
                    _ => {}
                }
            }
            if (turn + 1) % 100 == 0 || turn + 1 == turns {
                eprintln!("MEMORY_SAMPLE turn={} elapsed_ms={} messages_bytes={messages_bytes} continuation_bytes={continuation_bytes} retained_requests={retained_requests}", turn + 1, started.elapsed().as_millis());
            }
        }
    }).await.unwrap();
    agent.shutdown().await;
    assert_eq!(scripted.remaining(), 0);
}

#[test]
fn codex_turn_boundary_releases_patches_and_rejects_stale_item_approvals() {
    let mut correlations = CodexTurnCorrelations::default();
    for turn in 0..2000 {
        correlations.reset();
        assert_eq!(correlations.file_changes.capacity(), 0);
        assert!(correlations.approved.is_empty());
        assert!(correlations.pending_completions.is_empty());
        if turn > 0 {
            let stale = json!({"itemId": format!("item-{}", turn - 1)});
            assert!(
                codex_native_file_change_paths(&stale, Some(&correlations.file_changes)).is_empty(),
                "prior-turn metadata must not authorize a new approval"
            );
        }
        let id = format!("item-{turn}");
        let notification = crate::codex_app_server::Notification {
            method: "item/completed".into(),
            params: Some(
                json!({"item":{"id": id, "type":"fileChange", "status":"completed",
                "changes":[{"path":"/tmp/workspace/file.rs", "kind":{"type":"update", "content":"x".repeat(64*1024)}}]}}),
            ),
        };
        remember_codex_file_change_completion_paths(&notification, &mut correlations.file_changes);
        assert_eq!(
            codex_native_file_change_paths(
                &json!({"itemId": id}),
                Some(&correlations.file_changes)
            ),
            ["/tmp/workspace/file.rs"],
            "same-turn late approvals retain their policy metadata"
        );
        correlations.approved.insert(
            id.clone(),
            CodexNativeToolCorrelation {
                call_id: id.clone(),
                tool_name: "codex_file_change".into(),
            },
        );
        correlations.pending_completions.insert(id, true);
    }
    correlations.reset();
    assert_eq!(correlations.file_changes.capacity(), 0);
}

#[path = "session_scenarios.rs"]
pub(super) mod session_scenarios;

#[path = "experiment_schema_policy_tests.rs"]
mod experiment_schema_policy_tests;
