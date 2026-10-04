//! Gateway turns on the dex-loop kernel.
//!
//! Every gateway surface that runs a model turn composes it here: the same
//! local execution host the native actor used (`HostTools`), the host's prompt
//! hooks, a model resolved through that host, and `maestro_dex_host`'s
//! `HostTurnRun`. The chat endpoints project the turn onto the wire desktop
//! and web clients already read (`message_*`, `tool_execution_*`,
//! `action_approval_required`, `status`, `error`, `done`).
//!
//! The one human gate is the kernel's: a gated call returns a preview, the
//! model asks with `user.ask` bound to it, and the turn parks. That question
//! is shown as the existing approval card, keyed by the `user.ask` call id;
//! `/api/pending-requests/{id}/resume` delivers the decision through
//! `pending_tool_responses` exactly as it did for the native actor, and it
//! becomes the `Answer` that confirms or declines the exact call. Caller-run
//! tools (client tools and the session-messaging tools) park the same way and
//! resume with a `ClientToolResult`.

use super::*;
use crate::chat::{
    annotate_completed_turn, managed_gateway_receipt_status, record_chat_assistant_message,
    record_chat_error,
};
use crate::chat_output::ChatEventWriter;
use crate::native_turns::{NativeTurn, NativeTurnCommand};
use crate::turn_diffs::{ChatSnapshot, finish_chat_snapshot};
use maestro_dex_host::dex_loop::{
    ApprovalMode as TurnMode, Event, ExecutorKind, Exit, GovernanceClass, Outcome, Output,
    PrincipalId, ThreadId, ToolName, ToolSpec, TurnId,
};
use maestro_dex_host::{
    AiRsModel, HostTools, HostTurn, HostTurnRun, Observed, Park, Step, admit_prompt, turn_dir,
};
use maestro_local_host::agent::{NativeAgentConfig, dex_loop_execution_host};
use maestro_local_host::ai::ManagedGatewayReceipt;
use maestro_runtime::agent::native_host::{ApprovalMode, NativeExecutionHostHandle};
use maestro_runtime::agent::{CredentialVault, ToolResponseConsumption};

/// The principal a local gateway turn acts under. The gateway's own
/// authentication already admitted the caller; the kernel needs one stable
/// name so a confirmation binds to the person who was asked.
pub(crate) const LOCAL_PRINCIPAL: &str = "local";

/// What a gateway surface asks for.
pub(crate) struct KernelRequest {
    pub model: String,
    pub cwd: String,
    pub system_prompt: Option<String>,
    pub prompt: String,
    pub attachments: Vec<String>,
    pub thinking_budget: Option<u32>,
    pub sandbox_policy: Option<maestro_local_host::sandbox::SandboxPolicy>,
    pub background_task_access: maestro_local_host::tools::background_tasks::BackgroundTaskAccess,
}

/// A composed kernel turn, before its tools are chosen.
pub(crate) struct Kernel {
    pub host: NativeExecutionHostHandle,
    pub model: AiRsModel,
    /// Managed-gateway receipts, as each model call returns one.
    pub receipts: mpsc::UnboundedReceiver<ManagedGatewayReceipt>,
    /// The prompt and attachments the host's hooks admitted.
    pub prompt: String,
    pub attachments: Vec<String>,
}

/// The model name a request carries. A managed route keeps its namespace;
/// the managed boundary strips it immediately before dispatch, as it does
/// for the native actor.
fn request_model_name(model: &str) -> String {
    let model = model.trim();
    let managed = ["evalops/", "maestro-managed/"].iter().any(|prefix| {
        model
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    });
    if managed {
        model.to_owned()
    } else {
        maestro_local_host::ai::provider_model_name(model)
    }
}

/// Composes the host, runs the prompt hooks, and resolves the model the way
/// the native actor did for the same request.
pub(crate) async fn compose(request: KernelRequest) -> Result<Kernel, String> {
    compose_for_mode(request, crate::chat::InteractionMode::Implement).await
}

async fn compose_for_mode(
    request: KernelRequest,
    interaction_mode: crate::chat::InteractionMode,
) -> Result<Kernel, String> {
    let config = NativeAgentConfig {
        model: request.model.clone(),
        cwd: request.cwd,
        system_prompt: request.system_prompt.clone(),
        sandbox_policy: request.sandbox_policy,
        background_task_access: request.background_task_access,
        ..NativeAgentConfig::default()
    };
    let host = dex_loop_execution_host(&config, CredentialVault::new())
        .map_err(|error| error.to_string())?;
    // Hooks can run commands independently of model tools. Discuss admits
    // neither boundary; this fresh host is private to this turn.
    if interaction_mode == crate::chat::InteractionMode::Discuss {
        host.hook_set_enabled(false).await;
    }
    let admitted = admit_prompt(&host, request.prompt, request.attachments, &request.model).await?;
    let client = host.resolve_model(&request.model)?.client.ok_or_else(|| {
        format!(
            "model {} has no provider client for this runtime",
            request.model
        )
    })?;
    let system = maestro_runtime::agent::runtime_system_prompt(
        request.system_prompt.as_deref(),
        admitted.context.as_deref(),
        &request.model,
        host.model_capabilities(&request.model),
    );
    let (receipt_sender, receipts) = mpsc::unbounded_channel();
    let mut model = AiRsModel::new(
        client,
        request_model_name(&request.model),
        host.default_max_output_tokens(&request.model),
    )
    .with_receipts(receipt_sender);
    if env::var("MAESTRO_NATIVE_CODE_COMPANION").as_deref() == Ok("1") {
        let credentials = crate::native_credentials::Credentials::from_env()?;
        let refresh_http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "Native model credential transport is unavailable")?;
        let refresh_host = host.clone();
        let refresh_model = request.model.clone();
        model = model.with_client_refresh(move || {
            let credentials = credentials.clone();
            let http = refresh_http.clone();
            let host = refresh_host.clone();
            let model = refresh_model.clone();
            async move {
                credentials.current(&http).await?;
                host.resolve_model(&model)?
                    .client
                    .ok_or_else(|| "Native hosted model provider is unavailable".into())
            }
        });
    }
    if let Some(system) = system {
        model = model.with_system(system);
    }
    if let Some(budget) = request.thinking_budget {
        model = model.with_thinking(budget);
    }
    Ok(Kernel {
        host,
        model,
        receipts,
        prompt: admitted.prompt,
        attachments: admitted.attachments,
    })
}

/// The thread a gateway turn's log is scoped to.
pub(crate) fn local_thread(id: &str) -> ThreadId {
    ThreadId {
        org: "local".into(),
        workspace: "local".into(),
        thread: id.to_owned(),
    }
}

pub(crate) fn host_turn(thread: &str, dir: &Path, kernel: &Kernel, approval: TurnMode) -> HostTurn {
    HostTurn {
        thread: local_thread(thread),
        principal: PrincipalId::new(LOCAL_PRINCIPAL),
        turn: TurnId::new(
            dir.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("turn"),
        ),
        prompt: kernel.prompt.clone(),
        attachments: kernel.attachments.clone(),
        approval,
    }
}

/// A turn's private log directory, removed when the turn's driver is done
/// with it.
pub(crate) struct TurnDir(pub PathBuf);

impl Drop for TurnDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A caller-declared tool the kernel parks on.
pub(crate) fn client_tool_spec(definition: &ToolDefinition) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(&definition.tool.name),
        label: definition.tool.name.clone(),
        description: definition.tool.description.clone(),
        schema: definition.tool.input_schema.clone(),
        read_only: false,
        core: true,
        governance: GovernanceClass::Plain,
        executor: ExecutorKind::Client,
    }
}

fn tools_for_mode(
    host: NativeExecutionHostHandle,
    approval: ApprovalMode,
    client_tools: &[ToolDefinition],
    interaction_mode: crate::chat::InteractionMode,
) -> HostTools {
    let tools = HostTools::new(host, approval);
    if interaction_mode == crate::chat::InteractionMode::Discuss {
        // The policy port denies even unsolicited model calls because they
        // are absent from this catalog. No executable tool is delegated.
        tools.only(&HashSet::new())
    } else {
        tools.with_client_tools(client_tools.iter().map(client_tool_spec))
    }
}

fn strip_confirmation(args: &Value) -> Value {
    let mut args = args.clone();
    if let Some(map) = args.as_object_mut() {
        map.remove(maestro_dex_host::CONFIRMATION_FIELD);
    }
    args
}

/// What a turn said and did, in the shape sessions and A2A persist, plus the
/// chat frames each write projects to.
#[derive(Default)]
pub(crate) struct Transcript {
    pub response_started: bool,
    pub thinking_started: bool,
    pub assistant_text: String,
    pub thinking_text: String,
    pub tools: Vec<Value>,
    pub usage: Option<TokenUsage>,
    pub error: Option<String>,
    started: HashSet<String>,
    calls: HashMap<String, (String, Value)>,
}

impl Transcript {
    fn message(&self) -> Value {
        composer_assistant_message(&self.assistant_text, &self.thinking_text, None)
    }

    fn start_response(&mut self, frames: &mut Vec<Value>) {
        if !self.response_started {
            self.response_started = true;
            frames.push(serde_json::json!({ "type": "message_start", "message": self.message() }));
        }
    }

    fn tool_name(&self, id: &str) -> String {
        self.calls
            .get(id)
            .map(|(tool, _)| tool.clone())
            .unwrap_or_else(|| "tool".to_owned())
    }

    /// Records one accepted write and returns the chat frames it projects to.
    pub fn apply(&mut self, observed: Observed) -> Vec<Value> {
        let mut frames = Vec::new();
        let event = match observed {
            Observed::Text(text) => {
                self.start_response(&mut frames);
                self.assistant_text.push_str(&text);
                frames.push(serde_json::json!({
                    "type": "message_update",
                    "message": self.message(),
                    "assistantMessageEvent": { "type": "text_delta", "contentIndex": 0, "delta": text }
                }));
                return frames;
            }
            Observed::Event(_, event) => *event,
        };
        match event {
            Event::ThinkingDelta { text } => {
                self.start_response(&mut frames);
                if !self.thinking_started {
                    self.thinking_started = true;
                    let message = self.message();
                    frames.push(serde_json::json!({
                        "type": "message_update",
                        "message": message,
                        "assistantMessageEvent": { "type": "thinking_start", "contentIndex": 0, "partial": message }
                    }));
                }
                self.thinking_text.push_str(&text);
                frames.push(serde_json::json!({
                    "type": "message_update",
                    "message": self.message(),
                    "assistantMessageEvent": { "type": "thinking_delta", "contentIndex": 0, "delta": text }
                }));
            }
            Event::ModelStepCompleted { calls, .. } => {
                for call in calls {
                    self.calls.insert(
                        call.id.as_str().to_owned(),
                        (
                            call.tool.as_str().to_owned(),
                            strip_confirmation(&call.args),
                        ),
                    );
                }
                self.response_started = false;
                self.thinking_started = false;
            }
            Event::ToolStarted { call, tool, .. } => {
                let id = call.as_str().to_owned();
                let args = self
                    .calls
                    .get(&id)
                    .map(|(_, args)| args.clone())
                    .unwrap_or_else(|| serde_json::json!({}));
                record_tool_call_metadata(&mut self.tools, &id, tool.as_str(), args.clone());
                update_tool_metadata_status(&mut self.tools, &id, "running");
                self.started.insert(id.clone());
                frames.push(serde_json::json!({
                    "type": "tool_execution_start",
                    "toolCallId": id,
                    "toolName": tool.as_str(),
                    "args": args
                }));
            }
            Event::ToolProgress { call, label } => {
                frames.push(serde_json::json!({
                    "type": "tool_execution_update",
                    "toolCallId": call.as_str(),
                    "toolName": self.tool_name(call.as_str()),
                    "args": {},
                    "partialResult": label
                }));
            }
            Event::ToolFinished {
                call,
                outcome,
                output,
                ..
            } => {
                let id = call.as_str().to_owned();
                // A preview or denial finishes a call that never started;
                // the transcript shows only calls that ran.
                if self.started.remove(&id) {
                    let success = outcome == Outcome::Succeeded;
                    finish_tool_metadata(&mut self.tools, &id, success);
                    let text = match output {
                        Output::Text(text) => text,
                        _ => String::new(),
                    };
                    frames.push(serde_json::json!({
                        "type": "tool_execution_end",
                        "toolCallId": id,
                        "toolName": self.tool_name(&id),
                        "result": { "success": success, "output": text },
                        "isError": !success
                    }));
                }
            }
            Event::Usage(usage) => {
                let total = self.usage.get_or_insert_with(TokenUsage::default);
                total.input_tokens += usage.input_tokens;
                total.output_tokens += usage.output_tokens;
                total.cache_read_tokens += usage.cache_read_input_tokens;
                total.cache_write_tokens += usage.cache_creation_input_tokens;
                #[allow(clippy::cast_precision_loss)]
                let cost = usage.cost_micros as f64 / 1_000_000.0;
                if cost > 0.0 {
                    *total.cost.get_or_insert(0.0) += cost;
                }
            }
            Event::Error { message, .. } => self.error = Some(message),
            _ => {}
        }
        frames
    }

    /// Records a caller-run tool the turn parked on.
    pub fn client_call(&mut self, call: &str, tool: &str, args: &Value) {
        record_tool_call_metadata(&mut self.tools, call, tool, args.clone());
    }

    pub fn client_result(&mut self, call: &str, success: bool) {
        finish_tool_metadata(&mut self.tools, call, success);
    }
}

/// Which chat transport a turn writes to.
#[derive(Clone, Copy)]
pub(crate) enum Wire {
    Sse,
    WebSocket,
}

impl Wire {
    fn transport(self) -> CodexBridgeTransport {
        match self {
            Self::Sse => CodexBridgeTransport::Sse,
            Self::WebSocket => CodexBridgeTransport::WebSocket,
        }
    }
}

/// One chat turn's inputs, as the chat endpoints already prepared them.
pub(crate) struct DexChatTurn<'a> {
    pub state: &'a AppState,
    pub auth: &'a AuthContext,
    pub session_id: Option<String>,
    pub turn_scope: Option<String>,
    pub model: String,
    pub unattended: bool,
    pub system_prompt: Option<String>,
    pub prompt: String,
    pub client_tools: Vec<ToolDefinition>,
    pub interaction_mode: crate::chat::InteractionMode,
    /// Prepared attachment files; images reach the model as image blocks.
    pub attachments: Vec<String>,
    pub thinking_enabled: bool,
    pub usage_provider: String,
    pub usage_model: String,
    /// The session generation a context observation is recorded against;
    /// `None` leaves the session's context meter untouched.
    pub context_generation: Option<String>,
}

/// A chat stream that detaches instead of failing: once the client is gone
/// nothing more is written, and the turn is cancelled but still driven to its
/// exit, because a started mutation must never be dropped mid-flight.
struct Sink<'s, W: ChatEventWriter> {
    wire: Wire,
    stream: &'s mut W,
    detached: bool,
}

impl<W: ChatEventWriter> Sink<'_, W> {
    async fn send(&mut self, value: Value) {
        if !self.detached
            && self
                .stream
                .send_event(self.wire.transport(), &value)
                .await
                .is_err()
        {
            self.detached = true;
        }
    }
}

/// Resolves when the native turn that owns this run is stopped; never for an
/// observer-only stream.
async fn owner_cancelled(owner: Option<&Arc<NativeTurn>>) {
    match owner {
        Some(owner) => owner.cancel.cancelled().await,
        None => std::future::pending().await,
    }
}

/// The next control a native turn's owner sends; never without one.
async fn next_command(
    controls: Option<&mut mpsc::Receiver<NativeTurnCommand>>,
) -> Option<NativeTurnCommand> {
    match controls {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

fn pending_owner(
    native: Option<PendingToolResponseOwner>,
    auth: &AuthContext,
    session_id: Option<&str>,
    approval: bool,
) -> Option<PendingToolResponseOwner> {
    native
        .filter(|_| approval)
        .or_else(|| PendingToolResponseOwner::for_request(session_id, auth))
}

/// Waits for the decision on one pending request, registered under `id` so
/// the resume endpoint can deliver it and owner checks still apply. A native
/// owner's stop ends the wait with no decision.
async fn await_pending(
    state: &AppState,
    auth: &AuthContext,
    session_id: Option<&str>,
    id: &str,
    owner: Option<&Arc<NativeTurn>>,
    approval: bool,
) -> Option<(bool, Option<ToolResult>)> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    {
        let mut senders = state.pending_tool_responses.lock().await;
        let mut owners = state.pending_tool_response_sessions.lock().await;
        senders.insert(id.to_owned(), sender);
        if let Some(owner) = pending_owner(
            owner.map(|turn| turn.pending_request_owner()),
            auth,
            session_id,
            approval,
        ) {
            owners.insert(id.to_owned(), owner);
        }
    }
    let decision = tokio::select! {
        received = receiver.recv() => received.map(|(_, approved, result, _, ack)| {
            if let Some(ack) = ack {
                let _ = ack.send(ToolResponseConsumption::Accepted);
            }
            (approved, result)
        }),
        () = owner_cancelled(owner) => None,
    };
    state.pending_tool_responses.lock().await.remove(id);
    state.pending_tool_response_sessions.lock().await.remove(id);
    decision
}

/// Runs one chat turn on the kernel and writes it to `stream`. Returns
/// whether the turn completed (the caller acknowledges peer messages then).
pub(crate) async fn run_dex_chat(
    wire: Wire,
    stream: &mut impl ChatEventWriter,
    turn: DexChatTurn<'_>,
    snapshot: &mut Option<ChatSnapshot>,
) -> Result<bool, String> {
    let dir = turn_dir("maestro-dex-chat");
    let mut sink = Sink {
        wire,
        stream,
        detached: false,
    };
    let outcome = drive(&mut sink, &turn, snapshot, &dir).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    match outcome {
        Ok(completed) => Ok(completed),
        Err(message) => {
            record_chat_error(turn.state, turn.session_id.as_deref(), message.clone()).await;
            sink.send(serde_json::json!({ "type": "error", "message": message }))
                .await;
            sink.send(serde_json::json!({ "type": "done" })).await;
            Ok(false)
        }
    }
}

async fn drive_governed_tool<W: ChatEventWriter>(
    sink: &mut Sink<'_, W>,
    client: &crate::governed_native::Client,
    admission: &crate::governed_native::Admission,
    step: &str,
    args: &Value,
    owner: Option<&NativeTurn>,
) -> Result<(bool, String, Option<Value>), String> {
    let mut execution = client.execute(admission, step, args).await?;
    let id = execution["id"]
        .as_str()
        .ok_or("Hosted execution identity is missing")?
        .to_owned();
    let mut shown_approval: Option<String> = None;
    loop {
        let state = crate::governed_native::state(&execution)?;
        let waiting = state == "TOOL_EXECUTION_STATE_WAITING_APPROVAL";
        if waiting && owner.is_some_and(|owner| owner.cancel.is_cancelled()) {
            if let Some(approval) = shown_approval.take() {
                sink.send(serde_json::json!({"type":"action_approval_resolved","requestId":approval,"executionId":id})).await;
            }
            return Ok((
                false,
                "Stopped while waiting for account approval".into(),
                None,
            ));
        }
        if waiting {
            let approval = execution["approvalWait"]["approvalRequestId"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or("Hosted approval identity is missing")?;
            if shown_approval.as_deref() != Some(approval) {
                shown_approval = Some(approval.to_owned());
                sink.send(
                    serde_json::json!({"type":"action_approval_required","request":{
                        "id":approval,"approvalRequestId":approval,"toolExecutionId":id,"platformAdmissionId":admission.admission_id,
                        "owner":"platform-tool-execution","toolName":"computer.shell","args":args,
                        "reason":execution["approvalWait"]["reason"]
                    }}),
                )
                .await;
            }
        } else if let Some(approval) = shown_approval.take() {
            sink.send(serde_json::json!({"type":"action_approval_resolved","requestId":approval,"executionId":id})).await;
        }
        match state {
            "TOOL_EXECUTION_STATE_SUCCEEDED" => {
                sink.send(serde_json::json!({"type":"status","status":"tool_execution_receipt","details":{"executionId":id,"state":state,"provenance":execution["provenance"]}})).await;
                return Ok((
                    true,
                    serde_json::to_string(&execution["output"]["safeOutput"])
                        .map_err(|_| "Hosted safe output is invalid")?,
                    crate::governed_changes::receipt(&execution, admission)?,
                ));
            }
            "TOOL_EXECUTION_STATE_FAILED"
            | "TOOL_EXECUTION_STATE_DENIED"
            | "TOOL_EXECUTION_STATE_CANCELLED" => {
                return Ok((
                    false,
                    format!(
                        "Hosted tool ended with {state}: {}",
                        execution["failureCode"]
                            .as_str()
                            .unwrap_or("TOOL_EXECUTION_FAILURE_CODE_UNSPECIFIED")
                    ),
                    crate::governed_changes::receipt(&execution, admission)?,
                ));
            }
            _ => {}
        }
        // Approval decisions and resumed execution stay with the account's
        // Platform owner. This credential has no decision authority.
        execution = client.observe(&id, admission, step, args).await?;
        if crate::governed_native::state(&execution)? == "TOOL_EXECUTION_STATE_WAITING_APPROVAL" {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

#[cfg(test)]
pub(crate) async fn drive_governed_tool_for_test(
    state: &AppState,
    client: &crate::governed_native::Client,
    admission: &crate::governed_native::Admission,
    owner: Arc<NativeTurn>,
    scope: &str,
    args: &Value,
) -> Result<(), String> {
    let mut output = crate::chat_output::NativeTurnOutput(owner.clone());
    let mut sink = Sink {
        wire: Wire::WebSocket,
        stream: &mut output,
        detached: false,
    };
    let (_, _, changes) = drive_governed_tool(
        &mut sink,
        client,
        admission,
        "tool-step",
        args,
        Some(&owner),
    )
    .await?;
    if let Some(changes) = changes {
        crate::governed_changes::save(state, admission, Some(scope), changes).await?;
    }
    Ok(())
}

async fn drive<W: ChatEventWriter>(
    sink: &mut Sink<'_, W>,
    turn: &DexChatTurn<'_>,
    snapshot: &mut Option<ChatSnapshot>,
    dir: &Path,
) -> Result<bool, String> {
    let state = turn.state;
    // A receipt captures the workspace's net change. Overlapping accepted
    // Implement turns cannot safely attribute that net change to either turn.
    let hosted_capture = (state.config.native_code_hosted
        && turn.interaction_mode == crate::chat::InteractionMode::Implement)
        .then(|| state.native_snapshot_registry.acquire());
    let session_mode = crate::pull_request_watch::unattended_approval_mode(
        state,
        turn.session_id.as_deref(),
        turn.unattended,
    )
    .await;
    let kernel = compose_for_mode(
        KernelRequest {
            model: turn.model.clone(),
            cwd: state.config.cwd.to_string_lossy().to_string(),
            system_prompt: turn.system_prompt.clone(),
            prompt: turn.prompt.clone(),
            attachments: turn.attachments.clone(),
            thinking_budget: turn.thinking_enabled.then(|| {
                env::var("MAESTRO_THINKING_BUDGET")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(10_000)
            }),
            sandbox_policy: None,
            background_task_access: crate::background::access_for_session(
                state,
                turn.session_id.as_deref(),
                turn.auth,
            )
            .await,
        },
        if state.config.native_code_hosted {
            crate::chat::InteractionMode::Discuss
        } else {
            turn.interaction_mode
        },
    )
    .await?;
    // `auto` approves what the native actor would have asked about, as
    // Yolo does; a sandbox bypass still asks, and `auto` confirms it below.
    let mode = if session_mode == "auto" {
        ApprovalMode::Yolo
    } else {
        ApprovalMode::Selective
    };
    let native_owner = sink.stream.native_turn();
    let governed = if state.config.native_code_hosted
        && turn.interaction_mode == crate::chat::InteractionMode::Implement
    {
        let admission = native_owner
            .as_ref()
            .and_then(|owner| owner.platform_admission())
            .ok_or("Hosted Implement requires its admitted native owner")?;
        Some((crate::governed_native::Client::from_env()?, admission))
    } else {
        None
    };
    let tools = if governed.is_some() {
        HostTools::new(kernel.host.clone(), mode)
            .only(&HashSet::new())
            .with_client_tools([crate::governed_native::spec()])
    } else {
        tools_for_mode(
            kernel.host.clone(),
            mode,
            &turn.client_tools,
            turn.interaction_mode,
        )
    };
    let request = host_turn(
        turn.session_id.as_deref().unwrap_or("chat"),
        dir,
        &kernel,
        TurnMode::Interactive,
    );
    let Kernel {
        host,
        model,
        mut receipts,
        ..
    } = kernel;
    let started = Instant::now();
    let mut run = HostTurnRun::start(dir, model, tools, request).await?;

    if let Some(session_id) = turn.session_id.as_deref() {
        sink.send(serde_json::json!({
            "type": "status",
            "status": "session",
            "details": { "sessionId": session_id, "runtime": "rust-dex-loop" }
        }))
        .await;
    }
    sink.send(serde_json::json!({ "type": "agent_start" }))
        .await;
    sink.send(serde_json::json!({ "type": "turn_start" })).await;

    let mut transcript = Transcript::default();
    // A native-owned turn is stopped by its owner, not by its observers; the
    // kernel keeps driving a started mutation to its exit either way.
    let mut controls = native_owner
        .as_ref()
        .and_then(|owner| owner.take_commands());
    let mut stop_requested = false;
    let exit = loop {
        if sink.detached {
            run.cancel();
        }
        let step = tokio::select! {
            step = run.next() => step?,
            () = owner_cancelled(native_owner.as_ref()), if !stop_requested => {
                stop_requested = true;
                run.cancel();
                continue;
            }
            command = next_command(controls.as_mut()) => {
                match command {
                    // The dex-loop kernel has no mid-turn steering input, so
                    // the owner hears that plainly instead of timing out.
                    Some(NativeTurnCommand::Steer { content, reply }) => {
                        tracing::debug!(
                            bytes = content.len(),
                            "steering refused: the dex-loop kernel takes no mid-turn input"
                        );
                        let _ = reply.send(Err(
                            "Steering is not available for this runtime".to_owned(),
                        ));
                    }
                    None => controls = None,
                }
                continue;
            }
            Some(receipt) = receipts.recv() => {
                sink.send(managed_gateway_receipt_status(
                    receipt.request_id,
                    receipt.record_id,
                    receipt.lineage_id,
                    receipt.record_status,
                ))
                .await;
                continue;
            }
        };
        match step {
            Step::Observed(observed) => {
                if let (Some(generation), Observed::Event(_, event)) =
                    (turn.context_generation.as_deref(), &observed)
                {
                    if let Event::Usage(usage) = event.as_ref() {
                        // Each model call reports its whole prompt, cached or
                        // not: the latest one is the session's context in use.
                        crate::session_context::record_observed_input(
                            state,
                            turn.session_id.as_deref(),
                            generation,
                            usage.input_tokens,
                            &turn.usage_provider,
                            &turn.usage_model,
                        )
                        .await;
                    }
                }
                for frame in transcript.apply(observed) {
                    sink.send(frame).await;
                }
            }
            Step::Park(Park::Confirm {
                question,
                binding,
                args,
            }) => {
                let approved = match session_mode.as_str() {
                    "auto" => true,
                    "fail" => {
                        sink.send(approval_blocked_tool_event(
                            question.as_str(),
                            binding.tool.as_str(),
                        ))
                        .await;
                        false
                    }
                    _ => {
                        sink.send(serde_json::json!({
                            "type": "action_approval_required",
                            "request": {
                                "id": question.as_str(),
                                "toolName": binding.tool.as_str(),
                                "args": args,
                                "reason": "Tool execution requires approval"
                            }
                        }))
                        .await;
                        // Parked, nothing in flight: a gone client ends here.
                        if sink.detached {
                            break Exit::Interrupted;
                        }
                        match await_pending(
                            state,
                            turn.auth,
                            turn.session_id.as_deref(),
                            question.as_str(),
                            native_owner.as_ref(),
                            true,
                        )
                        .await
                        {
                            Some((approved, _)) => approved,
                            None => break Exit::Interrupted,
                        }
                    }
                };
                run.confirm(question, &binding, approved).await?;
            }
            Step::Park(Park::ClientTool { call, tool, args }) => {
                transcript.client_call(call.as_str(), tool.as_str(), &args);
                let result = if let Some((client, admission)) = &governed {
                    if tool.as_str() != "computer.shell" {
                        return Err("Hosted tool is not in its governed catalog".into());
                    }
                    if native_owner
                        .as_ref()
                        .is_some_and(|owner| owner.cancel.is_cancelled())
                    {
                        break Exit::Interrupted;
                    }
                    let (success, output, changes) = drive_governed_tool(
                        sink,
                        client,
                        admission,
                        call.as_str(),
                        &args,
                        native_owner.as_deref(),
                    )
                    .await?;
                    if let Some(mut changes) = changes {
                        if hosted_capture
                            .as_ref()
                            .is_some_and(|lease| lease.ambiguous())
                        {
                            changes["changes"] = serde_json::json!({"availability":"unavailable","files":[],"reason":"Another Implement turn overlapped this workspace capture"});
                        }
                        crate::governed_changes::save(
                            state,
                            admission,
                            turn.turn_scope.as_deref(),
                            changes,
                        )
                        .await?;
                    }
                    Some(if success {
                        ToolResult::success(output)
                    } else {
                        ToolResult::failure(output)
                    })
                } else if crate::pull_request_watch::is_tool(tool.as_str()) {
                    let (sender, mut results) = mpsc::unbounded_channel();
                    if let Some(event) = crate::watch_tool_approval::dispatch(
                        state,
                        turn.auth,
                        turn.session_id.as_deref(),
                        crate::watch_tool_approval::WatchToolRequest {
                            call_id: call.as_str(),
                            tool: tool.as_str(),
                            args: &args,
                        },
                        sender,
                        &session_mode,
                    )
                    .await
                    {
                        sink.send(event).await;
                    }
                    if sink.detached {
                        state
                            .pending_tool_responses
                            .lock()
                            .await
                            .remove(call.as_str());
                        state
                            .pending_tool_response_sessions
                            .lock()
                            .await
                            .remove(call.as_str());
                        state
                            .completed_client_tool_results
                            .lock()
                            .await
                            .remove(call.as_str());
                        break Exit::Interrupted;
                    }
                    let (result, stopped) = tokio::select! {
                        received = results.recv() => (
                            received.and_then(|(_, approved, result, _, _)| {
                                approved.then_some(result).flatten()
                            }),
                            false,
                        ),
                        () = owner_cancelled(native_owner.as_ref()) => (None, true),
                    };
                    state
                        .pending_tool_responses
                        .lock()
                        .await
                        .remove(call.as_str());
                    state
                        .pending_tool_response_sessions
                        .lock()
                        .await
                        .remove(call.as_str());
                    state
                        .completed_client_tool_results
                        .lock()
                        .await
                        .remove(call.as_str());
                    if stopped {
                        break Exit::Interrupted;
                    }
                    result
                } else if is_session_messaging_tool(tool.as_str()) {
                    Some(
                        handle_session_messaging_tool_call(
                            state,
                            turn.auth,
                            turn.session_id.as_deref(),
                            turn.turn_scope.as_deref(),
                            call.as_str(),
                            tool.as_str(),
                            &args,
                        )
                        .await,
                    )
                } else {
                    sink.send(serde_json::json!({
                        "type": "tool_execution_start",
                        "toolCallId": call.as_str(),
                        "toolName": tool.as_str(),
                        "args": args,
                        "clientOwned": true
                    }))
                    .await;
                    if sink.detached {
                        break Exit::Interrupted;
                    }
                    match await_pending(
                        state,
                        turn.auth,
                        turn.session_id.as_deref(),
                        call.as_str(),
                        native_owner.as_ref(),
                        false,
                    )
                    .await
                    {
                        Some((_, result)) => result,
                        None => break Exit::Interrupted,
                    }
                };
                let result = result.unwrap_or_else(|| ToolResult::failure("no result".to_owned()));
                transcript.client_result(call.as_str(), result.success);
                let output = if result.success {
                    result.output
                } else {
                    result.error.unwrap_or(result.output)
                };
                run.client_result(call, result.success, output).await?;
            }
            Step::Exit(exit) => break exit,
        }
    };

    if transcript.usage.is_some() {
        record_usage_entry(
            state,
            turn.session_id.as_deref(),
            &turn.usage_provider,
            &turn.usage_model,
            transcript.usage.as_ref(),
        )
        .await;
    }
    match exit {
        Exit::Done => {
            let usage = transcript.usage.clone().unwrap_or_default();
            if turn.interaction_mode == crate::chat::InteractionMode::Implement
                && !state.config.native_code_hosted
            {
                // Post-message hooks can run commands or sync history; they
                // remain part of Implement and are absent from Discuss.
                let _ = host
                    .hook_post_message(
                        &transcript.assistant_text,
                        usage.input_tokens,
                        usage.output_tokens,
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        Some("stop"),
                    )
                    .await;
            }
            let mut message = composer_assistant_message_with_tools(
                &transcript.assistant_text,
                &transcript.thinking_text,
                transcript.usage.clone(),
                &transcript.tools,
            );
            finish_chat_snapshot(snapshot, &mut message).await;
            annotate_completed_turn(&mut message);
            record_chat_assistant_message(state, turn.session_id.as_deref(), message.clone()).await;
            sink.send(serde_json::json!({ "type": "message_end", "message": message }))
                .await;
            sink.send(
                serde_json::json!({ "type": "turn_end", "message": message, "toolResults": [] }),
            )
            .await;
            sink.send(serde_json::json!({ "type": "agent_end", "messages": [message], "stopReason": "stop" }))
                .await;
            sink.send(serde_json::json!({ "type": "done" })).await;
            Ok(true)
        }
        Exit::Interrupted => {
            sink.send(serde_json::json!({ "type": "error", "message": "Turn interrupted" }))
                .await;
            sink.send(serde_json::json!({ "type": "done" })).await;
            Ok(false)
        }
        _ => {
            let message = transcript
                .error
                .clone()
                .unwrap_or_else(|| "The turn failed".to_owned());
            record_chat_error(state, turn.session_id.as_deref(), message.clone()).await;
            sink.send(serde_json::json!({ "type": "error", "message": message }))
                .await;
            sink.send(serde_json::json!({ "type": "done" })).await;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_client_tool_results_keep_legacy_session_completion() {
        let session = crate::tests::test_session_record("client-session");
        let native = PendingToolResponseOwner::NativeTurn {
            session_id: session.id.clone(),
            session_created_at: session.created_at.clone(),
            turn_id: "client-turn".into(),
        };
        let state = crate::tests::test_app_state_with_sessions(HashMap::from([(
            session.id.clone(),
            session,
        )]));
        let auth = AuthContext::default();
        let owner = pending_owner(Some(native), &auth, Some("client-session"), false).unwrap();
        assert_eq!(
            owner,
            PendingToolResponseOwner::Session("client-session".into())
        );
        let (sender, mut receiver) = mpsc::unbounded_channel();
        state
            .pending_tool_responses
            .lock()
            .await
            .insert("client-call".into(), sender);
        state
            .pending_tool_response_sessions
            .lock()
            .await
            .insert("client-call".into(), owner);
        let body =
            serde_json::json!({"kind":"tool","content":"completed","isError":false}).to_string();
        let mut initial = format!("POST /api/pending-requests/client-call/resume HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_bytes();
        let head = crate::http::parse_request_head(&initial).unwrap();
        let (_client, mut stream) = crate::tests::tcp_stream_pair().await;
        let response = crate::sessions::handle_pending_request_resume_endpoint(
            &mut stream,
            &mut initial,
            &head,
            &state,
            &auth,
        )
        .await;
        assert!(
            String::from_utf8(response)
                .unwrap()
                .starts_with("HTTP/1.1 200")
        );
        let result = receiver.try_recv().unwrap().2.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "completed");
    }

    #[test]
    fn managed_routes_keep_their_namespace() {
        assert_eq!(
            request_model_name("evalops/accounts/fireworks/models/glm-5p3"),
            "evalops/accounts/fireworks/models/glm-5p3"
        );
        assert_eq!(
            request_model_name("Maestro-Managed/claude"),
            "Maestro-Managed/claude"
        );
        assert_eq!(
            request_model_name("anthropic/claude-sonnet-4-5"),
            maestro_local_host::ai::provider_model_name("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn previews_and_denials_never_reach_the_transcript() {
        let mut transcript = Transcript::default();
        let call = maestro_dex_host::dex_loop::CallId::new("c1");
        let finished = Observed::Event(
            maestro_dex_host::dex_loop::Cursor(2),
            Box::new(Event::ToolFinished {
                call: call.clone(),
                outcome: Outcome::Failed,
                output: Output::Text("{\"status\":\"needs_confirmation\"}".into()),
                receipt: None,
                summary: None,
            }),
        );
        assert!(transcript.apply(finished).is_empty());
        assert!(transcript.tools.is_empty());
    }

    #[tokio::test]
    async fn discuss_denies_unsolicited_commands_and_client_calls_even_in_yolo() {
        use maestro_dex_host::dex_loop::{CallId, ProposedCall, Tools, Verdict, rehydrate};
        let workspace = tempfile::tempdir().unwrap();
        let config = NativeAgentConfig {
            cwd: workspace.path().to_string_lossy().into_owned(),
            ..NativeAgentConfig::default()
        };
        let host = dex_loop_execution_host(&config, CredentialVault::new()).unwrap();
        let client_tools = vec![ToolDefinition {
            tool: Tool::new("client_write", "write the source"),
            requires_approval: false,
        }];
        let tools = tools_for_mode(
            host,
            ApprovalMode::Yolo,
            &client_tools,
            crate::chat::InteractionMode::Discuss,
        );
        assert!(tools.catalog().is_empty());
        let context = rehydrate(local_thread("discuss"), &[]);
        for name in ["bash", "write", "client_write", "user.ask"] {
            let call = ProposedCall::new(
                CallId::new(name),
                ToolName::new(name),
                serde_json::json!({"command":"touch source.txt"}),
                PrincipalId::new(LOCAL_PRINCIPAL),
            );
            assert!(matches!(
                tools.policy(&context, &call).await,
                Verdict::Deny(_)
            ));
        }
        assert!(!workspace.path().join("source.txt").exists());
    }
}
