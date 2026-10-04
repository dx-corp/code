use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use maestro_runtime::agent::{
    ApprovalMode, CredentialVault, ExecutionPhase, ExecutionSource, FromAgent,
    InlineToolApprovalContext, ModelChoice, ModelDynamicsConfig, NativeAgent, NativeAgentConfig,
    NativeCodexAuth, NativeCodingCompletion, NativeExecutionHost, NativeExecutionHostHandle,
    NativeFirewallVerdict, NativeHookEvent, NativeHookResult, NativeHostFuture, NativeModelRoute,
    NativeReadOnlyToolCall, NativeResolvedClient, NativeToolAnnotations,
    NativeToolExecutionOptions, SteerSignal, ThinkingLevel, ToolDefinition, ToolExecution,
    ToolOutcome, ToolResult, WorkflowStateSnapshot,
};
use maestro_runtime::ai::{
    ContentBlock, Message, MessageContent, OpenAiClient, ScriptedBlock, ScriptedClient,
    ScriptedResponse, StopReason, Tool, UnifiedClient,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const TURN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
enum ExecutionMode {
    Immediate,
    BlockUntilCancelled,
}

#[derive(Clone, Debug)]
struct Invocation {
    call_id: String,
    name: String,
    args: Value,
}

/// A public-boundary fixture host. It deliberately has no TUI registry,
/// session manager, or process executor: the actor must use this one host
/// through `NativeExecutionHostHandle` for every tool and lifecycle callback.
#[derive(Clone)]
struct FixtureHost {
    cwd: Arc<PathBuf>,
    client: Arc<UnifiedClient>,
    tools: Arc<Vec<ToolDefinition>>,
    execution_mode: ExecutionMode,
    invocations: Arc<Mutex<Vec<Invocation>>>,
    started_tx: watch::Sender<Option<String>>,
    cancelled_tx: watch::Sender<Option<String>>,
    shutdown_tx: watch::Sender<usize>,
    shutdown_count: Arc<AtomicUsize>,
    session_id: Arc<Mutex<Option<String>>>,
}

impl FixtureHost {
    fn new(
        cwd: impl Into<PathBuf>,
        client: UnifiedClient,
        tool_names: &[&str],
        execution_mode: ExecutionMode,
    ) -> Arc<Self> {
        let tools = tool_names
            .iter()
            .map(|name| ToolDefinition {
                tool: Tool::new(*name, format!("Fixture tool {name}")).with_schema(
                    serde_json::json!({
                        "type": "object",
                        "additionalProperties": true
                    }),
                ),
                requires_approval: false,
            })
            .collect();
        let (started_tx, _) = watch::channel(None::<String>);
        let (cancelled_tx, _) = watch::channel(None::<String>);
        let (shutdown_tx, _) = watch::channel(0usize);
        Arc::new(Self {
            cwd: Arc::new(cwd.into()),
            client: Arc::new(client),
            tools: Arc::new(tools),
            execution_mode,
            invocations: Arc::new(Mutex::new(Vec::new())),
            started_tx,
            cancelled_tx,
            shutdown_tx,
            shutdown_count: Arc::new(AtomicUsize::new(0)),
            session_id: Arc::new(Mutex::new(None)),
        })
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn started(&self) -> watch::Receiver<Option<String>> {
        self.started_tx.subscribe()
    }

    fn cancelled(&self) -> watch::Receiver<Option<String>> {
        self.cancelled_tx.subscribe()
    }

    fn shutdowns(&self) -> watch::Receiver<usize> {
        self.shutdown_tx.subscribe()
    }

    fn shutdown_count(&self) -> usize {
        self.shutdown_count.load(Ordering::SeqCst)
    }

    fn seed_session(&self, session_id: &str) {
        *self
            .session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(session_id.to_owned());
    }

    fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn execution_result(&self, call_id: &str, name: &str, args: &Value) -> ToolExecution {
        let output = format!("fixture execution for {name} ({call_id}) with {args}");
        ToolExecution::from_legacy(
            call_id,
            name,
            ExecutionSource::Native,
            ToolResult::success(output),
        )
    }

    fn hook_result() -> NativeHookResult {
        NativeHookResult::Continue
    }
}

impl NativeExecutionHost for FixtureHost {
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.as_ref().clone()
    }

    fn has_native_tool(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|definition| definition.tool.name.eq_ignore_ascii_case(name))
    }

    fn is_reserved_tool(&self, _name: &str) -> bool {
        false
    }

    fn goal_tools_visible(&self) -> bool {
        false
    }

    fn include_ide_tools(&self) -> bool {
        false
    }

    fn missing_required(&self, name: &str, args: &Value) -> Vec<String> {
        if name.eq_ignore_ascii_case("bridge_tool") && args.get("value").is_none() {
            vec!["value".to_owned()]
        } else {
            Vec::new()
        }
    }

    fn has_code_authority(&self) -> bool {
        true
    }

    fn requires_sandbox_bypass_approval(&self, _name: &str, _args: &Value) -> bool {
        false
    }

    fn mcp_permission_allows(&self, _name: &str) -> bool {
        false
    }

    fn requires_approval(&self, _name: &str, _args: &Value) -> bool {
        false
    }

    fn is_mcp_tool(&self, _name: &str) -> bool {
        false
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
        Box::pin(async { Err("video fixture is host-owned".to_owned()) })
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
        options: NativeToolExecutionOptions<'a>,
    ) -> NativeHostFuture<'a, ToolExecution> {
        let call_id = call_id.to_owned();
        let name = name.to_owned();
        let args = args.clone();
        let cancel = options.cancel;
        let execution_mode = self.execution_mode;
        let invocations = Arc::clone(&self.invocations);
        let started_tx = self.started_tx.clone();
        let cancelled_tx = self.cancelled_tx.clone();
        let host = self.clone();
        Box::pin(async move {
            invocations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(Invocation {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                });
            let _ = started_tx.send(Some(call_id.clone()));
            match execution_mode {
                ExecutionMode::Immediate => host.execution_result(&call_id, &name, &args),
                ExecutionMode::BlockUntilCancelled => {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            let _ = cancelled_tx.send(Some(call_id.clone()));
                            ToolExecution::cancelled(
                                call_id,
                                name,
                                ExecutionSource::Native,
                                ExecutionPhase::Running,
                            )
                        }
                        () = std::future::pending::<()>() => unreachable!("fixture tool only completes through cancellation"),
                    }
                }
            }
        })
    }

    fn execute_read_only_wave<'a>(
        &'a self,
        calls: &'a [NativeReadOnlyToolCall],
        _event_tx: &'a mpsc::UnboundedSender<FromAgent>,
        _cancel: Option<CancellationToken>,
    ) -> NativeHostFuture<'a, HashMap<String, ToolExecution>> {
        Box::pin(async move {
            calls
                .iter()
                .map(|call| {
                    (
                        call.call_id.clone(),
                        self.execution_result(&call.call_id, &call.tool_name, &call.args),
                    )
                })
                .collect()
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
        let count = self.shutdown_count.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.shutdown_tx.send(count);
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
        Box::pin(async { Self::hook_result() })
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

    fn hook_post_message<'a>(
        &'a self,
        _response: &'a str,
        _input_tokens: u64,
        _output_tokens: u64,
        _duration_ms: u64,
        _stop_reason: Option<&'a str>,
    ) -> NativeHostFuture<'a, NativeHookResult> {
        Box::pin(async { Self::hook_result() })
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
        16_384
    }

    fn is_local_model(&self, model: &str) -> bool {
        model.starts_with("local/") || model.starts_with("llamacpp/")
    }

    fn model_context_window(&self, _model: &str) -> Option<u64> {
        Some(128_000)
    }

    fn validate_model_transition(&self, _from: &str, _to: &str) -> Result<(), String> {
        Ok(())
    }

    fn boost_choice(
        &self,
        _current: &ModelChoice,
        _config: &ModelDynamicsConfig,
    ) -> Option<ModelChoice> {
        None
    }

    fn normalize_thinking(&self, _model: &str, requested: ThinkingLevel) -> ThinkingLevel {
        requested
    }

    fn codex_auth_context(&self) -> Result<NativeCodexAuth, String> {
        Err("Codex transport is outside this direct-provider fixture".to_owned())
    }

    fn codex_auth_is_usable(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok()
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

    fn model_tool_spill_dir(&self, cwd: &str, session_id: &str) -> PathBuf {
        self.cwd.join(cwd).join(".maestro").join(session_id)
    }

    fn open_todo_count(&self, _output: &str) -> Option<usize> {
        None
    }

    fn semantic_conversation_protocol(&self) -> &str {
        "maestro.semantic-conversation.v1"
    }

    fn model_route(&self, _model_id: &str) -> NativeModelRoute {
        NativeModelRoute::DirectProvider
    }
}

fn config(cwd: &Path, model: &str, approval_mode: ApprovalMode) -> NativeAgentConfig {
    NativeAgentConfig {
        model: model.to_owned(),
        cwd: cwd.display().to_string(),
        approval_mode,
        ..NativeAgentConfig::default()
    }
}

fn start_agent(
    config: NativeAgentConfig,
    host: Arc<FixtureHost>,
    client: UnifiedClient,
    external_tools: Vec<ToolDefinition>,
    allowed_tools: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<(NativeAgent, mpsc::UnboundedReceiver<FromAgent>)> {
    let resolved = NativeResolvedClient {
        client: Some(client.clone()),
        provider_name: client.provider_name().to_owned(),
        model_route: NativeModelRoute::DirectProvider,
    };
    NativeAgent::start_with_resolved_client(
        config,
        NativeExecutionHostHandle::new(host),
        external_tools,
        CredentialVault::new(),
        allowed_tools,
        resolved,
    )
}

async fn events_until_completed(events: &mut mpsc::UnboundedReceiver<FromAgent>) -> Vec<FromAgent> {
    tokio::time::timeout(TURN_TIMEOUT, async {
        let mut seen = Vec::new();
        loop {
            let event = events
                .recv()
                .await
                .expect("native runtime event channel closed before completion");
            match &event {
                FromAgent::Error {
                    message, terminal, ..
                } if *terminal => {
                    panic!("native runtime terminal error: {message}")
                }
                FromAgent::ProviderError { message, .. } => {
                    panic!("native runtime provider error: {message}")
                }
                FromAgent::TurnCompleted { .. } => {
                    seen.push(event);
                    return seen;
                }
                _ => seen.push(event),
            }
        }
    })
    .await
    .expect("native runtime completion timeout")
}

async fn events_until_interrupted(
    events: &mut mpsc::UnboundedReceiver<FromAgent>,
) -> Vec<FromAgent> {
    tokio::time::timeout(TURN_TIMEOUT, async {
        let mut seen = Vec::new();
        loop {
            let event = events
                .recv()
                .await
                .expect("native runtime event channel closed before interruption");
            match &event {
                FromAgent::TurnCompleted { .. } => {
                    panic!("cancelled native runtime emitted TurnCompleted")
                }
                FromAgent::TurnInterrupted { .. } => {
                    seen.push(event);
                    return seen;
                }
                _ => seen.push(event),
            }
        }
    })
    .await
    .expect("native runtime interruption timeout")
}

async fn wait_for_watch<T>(
    receiver: &mut watch::Receiver<Option<T>>,
    expected: impl Fn(&Option<T>) -> bool,
) where
    T: Clone,
{
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            if expected(&receiver.borrow()) {
                return;
            }
            receiver
                .changed()
                .await
                .expect("fixture watch channel closed before expected state");
        }
    })
    .await
    .expect("fixture watch state timeout")
}

async fn read_provider_request(stream: &mut TcpStream) -> Value {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .expect("read provider request");
        assert!(read > 0, "provider closed before request headers");
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("provider request header terminator");
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .expect("provider request content length");
    let body_start = header_end + 4;
    while buffer.len() - body_start < content_length {
        let read = stream.read(&mut chunk).await.expect("read provider body");
        assert!(read > 0, "provider closed before request body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&buffer[body_start..body_start + content_length])
        .expect("provider request JSON")
}

fn final_text_sse(id: &str, text: &str) -> String {
    let start = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
    });
    let stop = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    format!("data: {start}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

fn external_tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        tool: Tool::new(name, "Caller-owned fixture operation").with_schema(serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"]
        })),
        requires_approval: true,
    }
}

#[tokio::test]
async fn runtime_fixture_completes_real_scripted_turn_and_explicit_shutdown_owns_cleanup() {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let scripted = ScriptedClient::new(
        "fixture/native-turn",
        vec![ScriptedResponse::text("controlled native answer")],
    );
    let client = UnifiedClient::Scripted(scripted.clone());
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &[],
        ExecutionMode::Immediate,
    );
    let (agent, mut events) = start_agent(
        config(workspace.path(), "fixture/native-turn", ApprovalMode::Yolo),
        Arc::clone(&host),
        client,
        Vec::new(),
        None,
    )
    .expect("public runtime constructor");
    assert!(
        !agent.managed_run_id().is_empty(),
        "runtime must allocate a run id"
    );
    agent
        .prompt("Answer from the controlled fixture.".to_owned(), Vec::new())
        .await
        .expect("fixture prompt");
    let seen = events_until_completed(&mut events).await;
    let response = seen
        .iter()
        .filter_map(|event| match event {
            FromAgent::ResponseChunk {
                content,
                is_thinking: false,
                ..
            } => Some(content.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(response, "controlled native answer");
    assert_eq!(
        scripted.remaining(),
        0,
        "the real loop must consume the scripted provider turn"
    );
    assert!(
        host.invocations().is_empty(),
        "text-only turn must not execute a host tool"
    );

    assert_eq!(
        host.shutdown_count(),
        0,
        "completed turns must retain the host"
    );
    agent.shutdown().await;
    assert_eq!(
        host.shutdown_count(),
        1,
        "explicit shutdown owns host cleanup exactly once"
    );
}

#[tokio::test]
async fn dropping_runtime_handle_eventually_runs_host_cleanup_once() {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let scripted = ScriptedClient::new("fixture/drop", vec![ScriptedResponse::text("unused")]);
    let client = UnifiedClient::Scripted(scripted);
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &[],
        ExecutionMode::Immediate,
    );
    let mut shutdowns = host.shutdowns();
    let (agent, events) = start_agent(
        config(workspace.path(), "fixture/drop", ApprovalMode::Yolo),
        Arc::clone(&host),
        client,
        Vec::new(),
        None,
    )
    .expect("public runtime constructor");
    drop(events);
    drop(agent);

    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            if *shutdowns.borrow() == 1 {
                return;
            }
            shutdowns
                .changed()
                .await
                .expect("host cleanup watch closed before runner drop cleanup");
        }
    })
    .await
    .expect("dropped runtime handle cleanup timeout");
    assert_eq!(
        host.shutdown_count(),
        1,
        "detached runner cleanup must have one owner"
    );
}

#[tokio::test]
async fn caller_tool_result_preserves_id_and_indeterminate_outcome_without_local_execution() {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("provider listener");
    let address = listener.local_addr().expect("provider address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for first in [true, false] {
            let (mut stream, _) = listener.accept().await.expect("provider connection");
            requests.push(read_provider_request(&mut stream).await);
            let body = if first {
                let call = serde_json::json!({
                    "id": "caller-response", "object": "chat.completion.chunk", "created": 0,
                    "model": "gpt-4o", "choices": [{"index": 0, "delta": {
                        "role": "assistant", "tool_calls": [{"index": 0, "id": "caller-call-42",
                            "type": "function", "function": {"name": "caller_owned",
                                "arguments": "{\"value\":\"remote\"}"}}]
                    }, "finish_reason": null}]
                });
                let stop = serde_json::json!({
                    "id": "caller-response", "object": "chat.completion.chunk", "created": 0,
                    "model": "gpt-4o", "choices": [{"index": 0, "delta": {},
                        "finish_reason": "tool_calls"}]
                });
                format!("data: {call}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
            } else {
                final_text_sse("caller-completed", "caller result consumed")
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("provider response");
        }
        requests
    });
    let client = UnifiedClient::OpenAI(
        OpenAiClient::with_base_url("fixture-key", format!("http://{address}/v1"))
            .expect("loopback OpenAI client"),
    );
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &[],
        ExecutionMode::Immediate,
    );
    let (agent, mut events) = start_agent(
        config(workspace.path(), "openai/gpt-4o", ApprovalMode::Selective),
        Arc::clone(&host),
        client,
        vec![external_tool("caller_owned")],
        None,
    )
    .expect("caller-owned fixture constructor");
    agent
        .prompt("Run the caller-owned operation.".to_owned(), Vec::new())
        .await
        .expect("caller-owned prompt");

    let call_id = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            match events
                .recv()
                .await
                .expect("event channel closed before ToolCall")
            {
                FromAgent::ToolCall {
                    call_id,
                    tool,
                    args,
                    requires_approval,
                    ..
                } => {
                    assert_eq!(tool, "caller_owned");
                    assert_eq!(args, serde_json::json!({"value": "remote"}));
                    assert!(
                        requires_approval,
                        "external tools always cross the caller approval boundary"
                    );
                    break call_id;
                }
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("caller-owned turn failed before ToolCall: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("caller ToolCall timeout");
    assert_eq!(
        call_id, "caller-call-42",
        "the model call id crosses the host boundary unchanged"
    );

    let result = ToolResult::success("caller-owned output").with_details(serde_json::json!({
        "remoteOutcome": "unknown",
        "requiresReconciliation": true,
        "opaque": {"provider": "fixture"}
    }));
    let typed = ToolExecution::from_legacy(
        &call_id,
        "caller_owned",
        ExecutionSource::RemoteClient,
        result.clone(),
    );
    assert!(matches!(typed.outcome, ToolOutcome::Indeterminate { .. }));
    assert!(
        typed
            .model_content()
            .contains("Indeterminate remote outcome")
    );
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    agent
        .tool_response_sender()
        .send((
            call_id.clone(),
            true,
            Some(result),
            ExecutionSource::RemoteClient,
            Some(ack_tx),
        ))
        .expect("caller-owned result");
    assert_eq!(
        ack_rx.await.expect("caller result acknowledgement"),
        maestro_runtime::agent::ToolResponseConsumption::Accepted
    );

    let seen = events_until_completed(&mut events).await;
    let snapshot = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            FromAgent::ConversationSnapshot { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("caller-owned completion snapshot");
    let mut saw_use = false;
    let mut saw_result = false;
    for message in snapshot {
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse { id, name, .. }
                        if id == "caller-call-42" && name == "caller_owned" =>
                    {
                        saw_use = true
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: Some(true),
                    } if tool_use_id == "caller-call-42" => {
                        assert_eq!(content, "[tool result omitted from checkpoint]");
                        saw_result = true;
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(
        saw_use,
        "snapshot must retain the caller-owned call identity"
    );
    assert!(
        saw_result,
        "indeterminate caller result must remain an error in the snapshot"
    );
    assert!(
        host.invocations().is_empty(),
        "caller-owned result must not fall back to host execution"
    );
    let requests = tokio::time::timeout(TURN_TIMEOUT, server)
        .await
        .expect("both provider rounds complete")
        .expect("provider task");
    let result_message = requests[1]["messages"]
        .as_array()
        .expect("provider messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "caller-call-42")
        .expect("provider receives matching caller result");
    assert_eq!(result_message["content"], typed.model_content());
    assert!(
        result_message["content"]
            .as_str()
            .expect("tool result text")
            .contains("Indeterminate remote outcome")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn unknown_model_tool_fails_closed_without_tool_call_or_host_execution() {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let scripted = ScriptedClient::new(
        "fixture/missing-tool",
        vec![
            ScriptedResponse {
                blocks: vec![ScriptedBlock::ToolUse {
                    id: "missing-call-1".to_owned(),
                    name: "governed_missing".to_owned(),
                    input: serde_json::json!({"value": "must fail closed"}),
                }],
                stop_reason: StopReason::ToolUse,
                error: None,
            },
            ScriptedResponse::text("recovered after missing capability"),
        ],
    );
    let client = UnifiedClient::Scripted(scripted.clone());
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &[],
        ExecutionMode::Immediate,
    );
    let (agent, mut events) = start_agent(
        config(workspace.path(), "fixture/missing-tool", ApprovalMode::Yolo),
        Arc::clone(&host),
        client,
        Vec::new(),
        None,
    )
    .expect("missing-capability fixture constructor");
    agent
        .prompt(
            "Use the unavailable governed capability.".to_owned(),
            Vec::new(),
        )
        .await
        .expect("missing-capability prompt");
    let seen = events_until_completed(&mut events).await;
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, FromAgent::ToolCall { .. })),
        "unknown governed tools must not be offered to a caller for execution"
    );
    let snapshot = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            FromAgent::ConversationSnapshot { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("missing-capability completion snapshot");
    assert!(snapshot.iter().any(|message| {
        matches!(
            &message.content,
            MessageContent::Blocks(blocks)
                if blocks.iter().any(|block| matches!(
                    block,
                    ContentBlock::ToolResult { tool_use_id, is_error: Some(true), .. }
                        if tool_use_id == "missing-call-1"
                ))
        )
    }));
    assert!(
        host.invocations().is_empty(),
        "missing governed tools must never reach the host"
    );
    assert_eq!(scripted.remaining(), 0);
    agent.shutdown().await;
}

#[tokio::test]
async fn cancellation_crosses_native_execution_host_boundary_and_emits_interrupted_turn() {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let scripted = ScriptedClient::new(
        "fixture/cancel",
        vec![ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "cancel-call-1".to_owned(),
                name: "bridge_tool".to_owned(),
                input: serde_json::json!({"value": "wait"}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        }],
    );
    let client = UnifiedClient::Scripted(scripted);
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &["bridge_tool"],
        ExecutionMode::BlockUntilCancelled,
    );
    let mut started = host.started();
    let mut cancelled = host.cancelled();
    let (agent, mut events) = start_agent(
        config(workspace.path(), "fixture/cancel", ApprovalMode::Yolo),
        Arc::clone(&host),
        client,
        Vec::new(),
        None,
    )
    .expect("cancellation fixture constructor");
    agent
        .prompt(
            "Start the cancellable host operation.".to_owned(),
            Vec::new(),
        )
        .await
        .expect("cancellation prompt");

    wait_for_watch(&mut started, |call_id| {
        call_id.as_deref() == Some("cancel-call-1")
    })
    .await;
    agent.cancel();
    wait_for_watch(&mut cancelled, |call_id| {
        call_id.as_deref() == Some("cancel-call-1")
    })
    .await;
    let seen = events_until_interrupted(&mut events).await;
    assert!(seen.iter().any(|event| matches!(
        event,
        FromAgent::TurnInterrupted { reason, .. } if reason == "cancelled"
    )));
    assert_eq!(
        host.invocations().len(),
        1,
        "cancellation must not replay the host call"
    );
    assert_eq!(host.invocations()[0].call_id, "cancel-call-1");
    assert_eq!(host.invocations()[0].name, "bridge_tool");
    assert_eq!(
        host.invocations()[0].args,
        serde_json::json!({"value": "wait"})
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn failed_construction_preserves_host_session_and_does_not_spawn_runner() {
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let scripted = ScriptedClient::new(
        "fixture/failed-construction",
        vec![ScriptedResponse::text("unused")],
    );
    let client = UnifiedClient::Scripted(scripted);
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &[],
        ExecutionMode::Immediate,
    );
    host.seed_session("existing-session");
    let invalid_allowlist = std::collections::HashSet::from(["missing_native".to_owned()]);
    let result = start_agent(
        config(
            workspace.path(),
            "fixture/failed-construction",
            ApprovalMode::Yolo,
        ),
        Arc::clone(&host),
        client,
        Vec::new(),
        Some(&invalid_allowlist),
    );
    let error = match result {
        Ok(_) => panic!("unknown governed allowlist must reject construction"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("Unknown allowed tool"));
    assert_eq!(host.session_id().as_deref(), Some("existing-session"));
    assert_eq!(
        host.shutdown_count(),
        0,
        "failed construction must not spawn a cleanup runner"
    );
    assert!(host.invocations().is_empty());
}

#[tokio::test]
async fn restored_provider_history_keeps_hidden_tool_pair_and_stable_id() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback provider");
    let address = listener.local_addr().expect("loopback provider address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("provider connection");
        let request = read_provider_request(&mut stream).await;
        let body = final_text_sse("history-fixture", "history restored");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("provider response");
        request
    });
    let workspace = tempfile::tempdir().expect("fixture workspace");
    let client = UnifiedClient::OpenAI(
        OpenAiClient::with_base_url("fixture-key", format!("http://{address}/v1"))
            .expect("loopback OpenAI client"),
    );
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &["bridge_tool"],
        ExecutionMode::Immediate,
    );
    let (agent, mut events) = start_agent(
        config(workspace.path(), "openai/gpt-4o", ApprovalMode::Yolo),
        Arc::clone(&host),
        client,
        Vec::new(),
        None,
    )
    .expect("history fixture constructor");
    agent.replace_history(vec![
        Message {
            role: maestro_runtime::ai::Role::User,
            content: MessageContent::text("visible prior request"),
        },
        Message {
            role: maestro_runtime::ai::Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "restored-call-9".to_owned(),
                name: "bridge_tool".to_owned(),
                input: serde_json::json!({"value": "restored"}),
                gemini_context: None,
            }]),
        },
        Message {
            role: maestro_runtime::ai::Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "restored-call-9".to_owned(),
                content: "RESTORED_PROVIDER_RESULT".to_owned(),
                is_error: Some(false),
            }]),
        },
    ]);
    agent
        .prompt("current request after restore".to_owned(), Vec::new())
        .await
        .expect("history restore prompt");
    let seen = events_until_completed(&mut events).await;
    let request = server.await.expect("loopback provider task");
    let messages = request["messages"]
        .as_array()
        .expect("provider message array");
    let visible = messages
        .iter()
        .position(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("visible prior request")
        })
        .expect("visible user history");
    let call = messages
        .iter()
        .position(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .expect("restored assistant tool call");
    let calls = messages[call]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "restored-call-9");
    assert_eq!(calls[0]["type"], "function");
    assert_eq!(calls[0]["function"]["name"], "bridge_tool");
    let arguments: Value = serde_json::from_str(
        calls[0]["function"]["arguments"]
            .as_str()
            .expect("function arguments"),
    )
    .expect("JSON function arguments");
    assert_eq!(arguments, serde_json::json!({"value": "restored"}));
    let result = messages
        .get(call + 1)
        .expect("tool result after assistant call");
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], calls[0]["id"]);
    assert_eq!(result["content"], "RESTORED_PROVIDER_RESULT");
    let current = messages
        .iter()
        .position(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("current request after restore")
        })
        .expect("current user request");
    assert!(visible < call && call + 1 < current);
    assert!(
        seen.iter()
            .any(|event| matches!(event, FromAgent::TurnCompleted { .. }))
    );
    assert!(
        host.invocations().is_empty(),
        "history restoration must not execute historical tools"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn process_usage_refusal_persists_only_completed_assistant_content() {
    for stream_failed in [false, true] {
        let workspace = tempfile::tempdir().expect("budget fixture workspace");
        let scripted = ScriptedClient::new(
            "fixture/budget-refusal",
            vec![ScriptedResponse {
                blocks: vec![
                    ScriptedBlock::Text("budgeted assistant content".to_owned()),
                    ScriptedBlock::BilledSilence { output_tokens: 2 },
                ],
                stop_reason: StopReason::EndTurn,
                error: stream_failed.then(|| "fixture stream failure".to_owned()),
            }],
        );
        let client = UnifiedClient::Scripted(scripted.clone());
        let host = FixtureHost::new(
            workspace.path(),
            client.clone(),
            &[],
            ExecutionMode::Immediate,
        );
        let (agent, mut events) = start_agent(
            config(
                workspace.path(),
                "fixture/budget-refusal",
                ApprovalMode::Yolo,
            ),
            Arc::clone(&host),
            client,
            Vec::new(),
            None,
        )
        .expect("budget fixture constructor");
        agent
            .install_process_budget(
                maestro_runtime::agent::process_budget::ProcessBudgetLimits {
                    event_id: "budget-fixture-event".to_owned(),
                    max_requests: 2,
                    max_total_tokens: 1,
                    max_cost_micros: 100,
                    cost_micros_per_token: 1,
                },
                None,
            )
            .await
            .expect("install process budget");
        agent
            .prompt("exceed the token ceiling".to_owned(), Vec::new())
            .await
            .expect("budget fixture prompt");
        let persisted = tokio::time::timeout(TURN_TIMEOUT, async {
            let mut persisted = Vec::new();
            loop {
                match events.recv().await.expect("budget refusal event") {
                    FromAgent::LocalAssistantContent { content, .. } => persisted.push(content),
                    FromAgent::Error {
                        message,
                        terminal: true,
                        ..
                    } => {
                        assert!(message.contains("budget exhausted"), "{message}");
                        break persisted;
                    }
                    FromAgent::TurnCompleted { .. } => panic!("over-budget turn completed"),
                    _ => {}
                }
            }
        })
        .await
        .expect("budget refusal timeout");
        if stream_failed {
            assert!(
                persisted.is_empty(),
                "failed partial output is not durable assistant content"
            );
        } else {
            assert_eq!(
                persisted.len(),
                1,
                "completed provider content persists before refusal"
            );
            assert!(persisted[0].iter().any(|block| matches!(block,
                ContentBlock::Text { text } if text == "budgeted assistant content")));
        }
        assert!(host.invocations().is_empty());
        assert_eq!(scripted.remaining(), 0);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn native_loop_retains_gemini_context_without_passing_it_to_tool_execution() {
    let workspace = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for first in [true, false] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_provider_request(&mut stream).await;
            let output = if first {
                serde_json::json!([{"type":"function_call", "id":"native-item", "call_id":"call-native",
                    "name":"bridge_tool", "arguments":"{\"value\":\"500500\"}",
                    "extra_content":{"google":{"native_name":"bridge_tool", "thought_signature":"opaque-signature"}}}])
            } else {
                serde_json::json!([{"type":"message", "content":[{"type":"output_text", "text":"result consumed"}]}])
            };
            let event = serde_json::json!({"type":"response.completed", "response":{"id":"response-native", "output":output}});
            let body = format!("data: {event}\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = UnifiedClient::OpenAI(
        OpenAiClient::with_base_url("fixture-key", format!("http://{address}/v1")).unwrap(),
    );
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &["bridge_tool"],
        ExecutionMode::Immediate,
    );
    let (agent, mut events) = start_agent(
        config(
            workspace.path(),
            "openai/gpt-5.1-codex-max",
            ApprovalMode::Yolo,
        ),
        Arc::clone(&host),
        client,
        Vec::new(),
        None,
    )
    .unwrap();
    agent
        .prompt("Run the operation.".into(), Vec::new())
        .await
        .unwrap();
    let seen = events_until_completed(&mut events).await;
    server.await.unwrap();
    assert_eq!(host.invocations().len(), 1);
    assert_eq!(
        host.invocations()[0].args,
        serde_json::json!({"value":"500500"})
    );
    let content = seen
        .iter()
        .find_map(|event| match event {
            FromAgent::LocalAssistantContent { content, .. }
                if content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. })) =>
            {
                Some(content)
            }
            _ => None,
        })
        .expect("durable assistant content");
    assert!(content.iter().any(
        |block| matches!(block, ContentBlock::ToolUse {gemini_context:Some(context), ..}
        if context.thought_signature.as_deref() == Some("opaque-signature"))
    ));
    let snapshot = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            FromAgent::ConversationSnapshot { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("completed conversation checkpoint");
    assert!(snapshot.iter().any(|message| matches!(&message.content, MessageContent::Blocks(blocks)
        if blocks.iter().any(|block| matches!(block, ContentBlock::ToolUse {gemini_context:Some(context), ..}
            if context.thought_signature.as_deref() == Some("opaque-signature"))))));
    agent.shutdown().await;
}

#[tokio::test]
async fn queued_prompt_observes_its_config_only_at_the_native_turn_boundary() {
    use maestro_runtime::agent::PromptKind;
    use sha2::{Digest, Sha256};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        let (mut stream, _) = listener.accept().await.unwrap();
        requests.push(read_provider_request(&mut stream).await);
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        for index in 0..2 {
            let body = final_text_sse(&format!("queued-{index}"), "done");
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            if index == 0 {
                (stream, _) = listener.accept().await.unwrap();
                requests.push(read_provider_request(&mut stream).await);
            }
        }
        requests
    });
    let workspace = tempfile::tempdir().unwrap();
    let client = UnifiedClient::OpenAI(
        OpenAiClient::with_base_url("fixture-key", format!("http://{address}/v1")).unwrap(),
    );
    let host = FixtureHost::new(
        workspace.path(),
        client.clone(),
        &[],
        ExecutionMode::Immediate,
    );
    let mut cfg = config(workspace.path(), "openai/gpt-4o", ApprovalMode::Yolo);
    cfg.system_prompt = Some("FIRST_PINNED_CONFIG".to_owned());
    let (agent, mut events) = start_agent(cfg, host, client, Vec::new(), None).unwrap();
    agent.prompt("first".to_owned(), Vec::new()).await.unwrap();
    tokio::time::timeout(TURN_TIMEOUT, started_rx)
        .await
        .unwrap()
        .unwrap();
    agent
        .set_system_prompt_for_queued_prompt(43, "CANCELED_CONFIG".to_owned())
        .unwrap();
    agent
        .prompt_with_kind(
            "discard this".to_owned(),
            Vec::new(),
            PromptKind::FollowUp,
            Some(43),
        )
        .await
        .unwrap();
    agent.cancel_queued(43);
    agent
        .set_system_prompt_for_queued_prompt(42, "SECOND_PINNED_CONFIG".to_owned())
        .unwrap();
    agent
        .prompt_with_kind(
            "second".to_owned(),
            Vec::new(),
            PromptKind::FollowUp,
            Some(42),
        )
        .await
        .unwrap();
    let mut discarded_ids = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let FromAgent::QueuedPromptConfiguration {
            queue_ids,
            system_prompt_sha256: None,
        } = &event
        {
            discarded_ids.extend(queue_ids.iter().copied());
        }
        assert!(
            !matches!(
                event,
                FromAgent::QueuedPromptConfiguration {
                    system_prompt_sha256: Some(_),
                    ..
                }
            ),
            "enqueue must not claim activation"
        );
    }
    release_tx.send(()).unwrap();
    let requests = tokio::time::timeout(TURN_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("FIRST_PINNED_CONFIG")
    );
    assert!(
        !requests[0]["messages"]
            .to_string()
            .contains("SECOND_PINNED_CONFIG")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("SECOND_PINNED_CONFIG")
    );
    let expected = format!("sha256:{:x}", Sha256::digest(b"SECOND_PINNED_CONFIG"));
    let mut installed = Vec::new();
    tokio::time::timeout(TURN_TIMEOUT, async {
        while let Some(event) = events.recv().await {
            if let FromAgent::QueuedPromptConfiguration {
                queue_ids,
                system_prompt_sha256,
            } = &event
            {
                if system_prompt_sha256.is_some() {
                    installed.push((queue_ids.clone(), system_prompt_sha256.clone()));
                } else {
                    discarded_ids.extend(queue_ids.iter().copied());
                }
            }
            if matches!(event, FromAgent::TurnCompleted { .. }) && !installed.is_empty() {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(installed, vec![(vec![42], Some(expected))]);
    assert_eq!(discarded_ids, vec![43]);
    assert!(
        !requests[1]["messages"]
            .to_string()
            .contains("CANCELED_CONFIG")
    );
    agent.shutdown().await;
}
