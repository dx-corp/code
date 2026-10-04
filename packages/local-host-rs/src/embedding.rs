//! Supported local embedding API for the native Maestro actor.
//!
//! [`EmbeddedAgentBuilder`] composes the existing local host, native actor,
//! provider resolution, and event relay. It does not create a second runtime
//! or accept Platform authority. Hosted callers continue to use their
//! Platform-owned admission and execution paths.

use std::{collections::HashSet, path::Path, sync::Arc};

use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(feature = "runtime-gateway-bridge")]
use crate::agent::ToolResponseMessage;
use crate::agent::{
    ExecutionSource, FromAgent, MaxTokensSource, NativeAgent, NativeAgentConfig, TokenUsage,
    ToolDefinition, ToolResult,
};

/// The single event receiver returned by a started embedding.
pub type EmbeddedAgentEvents = mpsc::UnboundedReceiver<FromAgent>;

/// Terminal result of one runner-owned native turn.
#[derive(Debug, Clone)]
pub struct EmbeddedRunCompleted {
    response_id: String,
    output: String,
    final_response_usage: Option<TokenUsage>,
}

impl EmbeddedRunCompleted {
    /// Final provider response observed before the native turn completed.
    ///
    /// If the native producer did not emit a response start, this falls back
    /// to the protocol's terminal completion ID.
    #[must_use]
    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    /// Non-thinking text from the final provider response in this turn.
    #[must_use]
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Usage reported with the final provider response, when the provider
    /// supplies it. This is a response snapshot, not a cumulative billing
    /// total for tool-driven turns.
    #[must_use]
    pub fn final_response_usage(&self) -> Option<&TokenUsage> {
        self.final_response_usage.as_ref()
    }
}

/// One tool call whose approval decision is owned by an [`EmbeddedAgentRunner`].
///
/// The fields remain private so a response can only use a call issued by the
/// runner. The runner still checks that the call is pending before forwarding
/// any response to the native actor.
#[derive(Debug)]
pub struct EmbeddedToolCall {
    call_id: String,
    tool: String,
    arguments: serde_json::Value,
    runner_token: Arc<RunnerToken>,
}

impl EmbeddedToolCall {
    /// Stable ID required for the native actor's matching response.
    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Model-visible tool name.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Model-supplied arguments after the native pre-tool hook boundary.
    #[must_use]
    pub fn arguments(&self) -> &serde_json::Value {
        &self.arguments
    }
}

/// A tool decision the runner is waiting for.
///
/// External tools are the definitions passed to
/// [`EmbeddedAgentBuilder::external_tools`]. Host tools include local and MCP
/// tools resolved by the existing local host.
#[derive(Debug)]
pub enum EmbeddedPendingTool {
    External(EmbeddedToolCall),
    Host(EmbeddedToolCall),
}

impl EmbeddedPendingTool {
    /// Tool call details shared by both ownership variants.
    #[must_use]
    pub fn tool_call(&self) -> &EmbeddedToolCall {
        match self {
            Self::External(call) | Self::Host(call) => call,
        }
    }

    /// Whether this call belongs to an embedding-supplied external tool.
    #[must_use]
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    fn owner(&self) -> PendingToolOwner {
        match self {
            Self::External(_) => PendingToolOwner::External,
            Self::Host(_) => PendingToolOwner::Host,
        }
    }
}

/// A single event or state transition from a runner-owned native turn.
#[derive(Debug)]
pub enum EmbeddedRunEvent {
    /// A native event that does not require a caller decision and does not
    /// finish the turn. This includes `ResponseEnd`.
    Event(Box<FromAgent>),
    /// A gated tool call. The runner will not read another event until the
    /// caller approves, denies, or supplies a permitted external result.
    AwaitingTool(EmbeddedPendingTool),
    /// The positive terminal. The native protocol reserves this result for
    /// `FromAgent::TurnCompleted`.
    Completed(EmbeddedRunCompleted),
}

/// Result of collecting a runner-owned turn until it needs a caller decision
/// or reaches the positive native terminal.
#[derive(Debug)]
pub enum EmbeddedRunProgress {
    AwaitingTool(EmbeddedPendingTool),
    Completed(EmbeddedRunCompleted),
}

/// An error from a runner-owned native turn.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddedRunError {
    /// The runner admits exactly one prompt, which keeps its private receiver
    /// attributable to that prompt.
    #[error("embedded runner already started its only supported turn")]
    AlreadyStarted,
    #[error("embedded runner has not started a turn")]
    NotStarted,
    #[error("embedded runner is waiting for a tool response")]
    AwaitingToolResponse,
    #[error("embedded runner has reached a terminal state")]
    Terminal,
    #[error("failed to queue embedded prompt: {message}")]
    Prompt { message: String },
    #[error("embedded event stream ended before a terminal turn event")]
    EventStreamClosed,
    #[error("embedded turn was interrupted: {reason}")]
    Interrupted { response_id: String, reason: String },
    #[error("embedded provider failed ({kind}): {message}")]
    Provider { kind: String, message: String },
    #[error("embedded runtime failed: {message}")]
    Runtime {
        message: String,
        fatal: bool,
        terminal: bool,
        retryable: bool,
    },
    #[error("embedded turn completed after cancellation was requested")]
    CompletedAfterCancellation,
    #[error("invalid embedded tool response: {message}")]
    ToolResponse { message: String },
    #[error("embedded agent stopped accepting tool responses")]
    ToolResponseChannelClosed,
}

/// Result returned by [`EmbeddedAgentRunner`] operations.
pub type EmbeddedRunResult<T> = std::result::Result<T, EmbeddedRunError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingToolOwner {
    External,
    Host,
}

#[derive(Debug)]
struct PendingToolState {
    call_id: String,
    owner: PendingToolOwner,
    runner_token: Arc<RunnerToken>,
}

/// Opaque per-tool-decision identity carried only inside pending tool values.
///
/// This has process-local pointer identity and is never serialized or exposed
/// as a durable session identifier.
#[derive(Debug)]
struct RunnerToken;

#[derive(Debug)]
enum EmbeddedRunnerState {
    Ready,
    Running,
    AwaitingTool(PendingToolState),
    Cancelling,
    Terminal,
}

#[derive(Debug, Default)]
struct ResponseCollection {
    response_id: Option<String>,
    output: String,
    final_response_usage: Option<TokenUsage>,
}

/// Builder for one local native-agent session.
///
/// The builder keeps the local host's defaults, including selective approval.
/// Tools supplied through [`Self::external_tools`] are caller-owned. Use
/// [`Self::start_runner`] when the caller needs to return external tool
/// results.
pub struct EmbeddedAgentBuilder {
    config: NativeAgentConfig,
    external_tools: Vec<ToolDefinition>,
}

impl EmbeddedAgentBuilder {
    /// Start a builder for an explicitly selected model.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        let config = NativeAgentConfig {
            max_tokens: crate::model_catalog::default_max_output_tokens(&model),
            model,
            ..NativeAgentConfig::default()
        };
        Self {
            config,
            external_tools: Vec::new(),
        }
    }

    /// Start a builder from an existing local-host configuration.
    ///
    /// This preserves every configured limit and policy field. It is useful
    /// for callers that already construct `NativeAgentConfig` values.
    #[must_use]
    pub fn from_config(config: NativeAgentConfig) -> Self {
        Self {
            config,
            external_tools: Vec::new(),
        }
    }

    /// Set the per-request output-token limit for the selected model.
    #[must_use]
    pub fn max_output_tokens(mut self, max_tokens: u32) -> Self {
        self.config.max_tokens = max_tokens;
        self.config.max_tokens_source = MaxTokensSource::Explicit;
        self
    }

    /// Set the local workspace used by the existing host's tools, hooks, and
    /// session integration.
    #[must_use]
    pub fn working_directory(mut self, path: impl AsRef<Path>) -> Self {
        self.config.cwd = path.as_ref().to_string_lossy().into_owned();
        self
    }

    /// Set the base instructions for the session.
    #[must_use]
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.config.system_prompt = Some(prompt.into());
        self
    }

    /// Enable or disable model thinking for this session.
    #[must_use]
    pub fn thinking(mut self, enabled: bool) -> Self {
        self.config.thinking_enabled = enabled;
        self
    }

    /// Set the model thinking-token budget for this session.
    #[must_use]
    pub fn thinking_budget(mut self, budget: u32) -> Self {
        self.config.thinking_budget = budget;
        self
    }

    /// Set the local host's approval mode for this embedding.
    ///
    /// This changes only the native local-host approval gate. It does not
    /// create Platform approval authority.
    #[must_use]
    pub fn approval_mode(mut self, mode: crate::state::ApprovalMode) -> Self {
        self.config.approval_mode = mode;
        self
    }

    /// Set the local native sandbox policy used by the existing host.
    ///
    /// The policy applies when the local host executes a tool. Hosted sandbox
    /// placement remains owned by Platform.
    #[must_use]
    pub fn sandbox_policy(mut self, policy: crate::sandbox::SandboxPolicy) -> Self {
        self.config.sandbox_policy = Some(policy);
        self
    }

    /// Set an explicit local model context window.
    ///
    /// A zero value is rejected when a runner is started.
    #[must_use]
    pub fn context_window(mut self, tokens: u64) -> Self {
        self.config.context_window = Some(tokens);
        self
    }

    /// Bound the number of tool-loop steps in a runner-owned turn.
    ///
    /// A zero value is rejected when a runner is started. This builder does
    /// not expose the raw unbounded-turn configuration.
    #[must_use]
    pub fn max_turn_steps(mut self, steps: usize) -> Self {
        self.config.max_turn_steps = steps;
        self
    }

    /// Add tools whose execution and results are owned by the embedding
    /// caller.
    ///
    /// When a caller-owned tool reaches the native approval boundary, the
    /// loop emits a `ToolCall` and waits for the caller to approve, deny, or
    /// supply a result. This method does not register a local executable or
    /// grant Platform authority.
    #[must_use]
    pub fn external_tools(mut self, tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        self.external_tools.extend(tools);
        self
    }

    /// Compose the local host and start its native actor.
    pub fn start(self) -> Result<EmbeddedAgentSession> {
        self.validate()?;
        let (agent, events) = NativeAgent::new_with_tools(self.config, self.external_tools)?;
        Ok(EmbeddedAgentSession {
            agent: EmbeddedAgent { inner: agent },
            events,
        })
    }

    /// Start a fresh, runner-owned native turn session.
    ///
    /// The runner owns both the command handle and event receiver. It accepts
    /// one prompt, so a terminal event cannot be mistaken for a turn queued by
    /// another consumer. Use [`EmbeddedAgentBuilder::start`] for the existing
    /// raw multi-prompt session surface.
    pub fn start_runner(self) -> Result<EmbeddedAgentRunner> {
        self.validate_runner_limits()?;
        let external_tools = external_tool_names(&self.external_tools);
        let (agent, events) = NativeAgent::new_with_tools(self.config, self.external_tools)?;
        Ok(EmbeddedAgentRunner {
            agent: Some(agent),
            events,
            external_tools,
            state: EmbeddedRunnerState::Ready,
            response: ResponseCollection::default(),
            #[cfg(feature = "test-support")]
            drop_shutdown_complete: None,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.config.model.trim().is_empty() {
            anyhow::bail!("an embedded agent requires a model identifier");
        }
        if self.config.cwd.trim().is_empty() {
            anyhow::bail!("an embedded agent requires a working directory");
        }
        Ok(())
    }

    fn validate_runner_limits(&self) -> Result<()> {
        self.validate()?;
        if self.config.max_tokens == 0 {
            anyhow::bail!("an embedded runner requires a positive output-token limit");
        }
        if self.config.context_window == Some(0) {
            anyhow::bail!("an embedded runner requires a positive context window");
        }
        if self.config.max_turn_steps == 0 {
            anyhow::bail!("an embedded runner requires at least one turn step");
        }
        Ok(())
    }

    #[cfg(feature = "test-support")]
    fn start_with_test_client(
        self,
        client: crate::ai::UnifiedClient,
    ) -> Result<EmbeddedAgentSession> {
        self.validate()?;
        let (agent, events) =
            NativeAgent::new_with_tools_and_test_client(self.config, self.external_tools, client)?;
        Ok(EmbeddedAgentSession {
            agent: EmbeddedAgent { inner: agent },
            events,
        })
    }

    #[cfg(feature = "test-support")]
    fn start_runner_with_test_client(
        self,
        client: crate::ai::UnifiedClient,
    ) -> Result<EmbeddedAgentRunner> {
        self.validate_runner_limits()?;
        let external_tools = external_tool_names(&self.external_tools);
        let (agent, events) =
            NativeAgent::new_with_tools_and_test_client(self.config, self.external_tools, client)?;
        Ok(EmbeddedAgentRunner {
            agent: Some(agent),
            events,
            external_tools,
            state: EmbeddedRunnerState::Ready,
            response: ResponseCollection::default(),
            drop_shutdown_complete: None,
        })
    }
}

fn external_tool_names(tools: &[ToolDefinition]) -> HashSet<String> {
    tools
        .iter()
        .map(|definition| definition.tool.name.to_ascii_lowercase())
        .collect()
}

/// A started local embedding and its single event receiver.
///
/// Call [`Self::shutdown`] when the embedding stops. It cancels active and
/// queued work, then waits for the native actor to finish cleanup.
pub struct EmbeddedAgentSession {
    agent: EmbeddedAgent,
    events: EmbeddedAgentEvents,
}

impl EmbeddedAgentSession {
    /// Access the command and tool-response side of the embedding.
    #[must_use]
    pub fn agent(&self) -> &EmbeddedAgent {
        &self.agent
    }

    /// Receive the native actor's event stream.
    pub fn events(&mut self) -> &mut EmbeddedAgentEvents {
        &mut self.events
    }

    /// Split the command handle from the event receiver.
    ///
    /// The caller that receives the handle must call
    /// [`EmbeddedAgent::shutdown`] after event processing finishes.
    #[must_use]
    pub fn into_parts(self) -> (EmbeddedAgent, EmbeddedAgentEvents) {
        (self.agent, self.events)
    }

    /// Cancel active work and wait for the actor's cleanup barrier.
    pub async fn shutdown(self) {
        self.agent.shutdown().await;
    }
}

/// Command handle for a started local embedding.
pub struct EmbeddedAgent {
    inner: NativeAgent,
}

impl EmbeddedAgent {
    /// Queue a prompt with no attachments.
    pub async fn prompt(&self, content: impl Into<String>) -> Result<()> {
        self.inner.prompt(content.into(), Vec::new()).await
    }

    /// Queue a prompt with caller-selected attachment paths.
    pub async fn prompt_with_attachments(
        &self,
        content: impl Into<String>,
        attachments: Vec<String>,
    ) -> Result<()> {
        self.inner.prompt(content.into(), attachments).await
    }

    /// Steer the active native turn at its next supported interruption point.
    /// The native actor owns consumption; this never restarts the original prompt.
    #[cfg(feature = "runtime-gateway-bridge")]
    pub async fn steer(&self, content: String) -> Result<()> {
        self.inner
            .prompt_with_kind(
                content,
                Vec::new(),
                maestro_runtime::agent::PromptKind::Steer,
                None,
            )
            .await
    }

    /// Associate subsequent host hooks with a caller-owned session.
    ///
    /// The embedding does not take ownership of persisted tool spill cleanup.
    pub fn set_session_context(
        &self,
        session_id: Option<String>,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.inner.set_session_context(session_id, reason, false)
    }

    /// Cancel the active prompt and any queued prompts.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Send one caller-owned tool denial to the native loop.
    pub fn send_tool_response(&self, response: EmbeddedToolResponse) -> Result<()> {
        let (call_id, approved, result, source) = response.into_parts();
        self.inner
            .tool_response_sender()
            .send((call_id, approved, result, source, None))
            .map_err(|_| anyhow::anyhow!("embedded agent has stopped accepting tool responses"))
    }

    /// Obtain the underlying native response channel for runtime-gateway compatibility.
    ///
    /// This is a legacy bridge for internal hosted transports that already
    /// perform their own admission and ownership checks. New embedded
    /// applications should use [`EmbeddedAgentBuilder::start_runner`] for
    /// caller-owned tool results, or [`Self::send_tool_response`] for raw
    /// denial-only decisions. The owning runtime-gateway transport must
    /// enforce admission and tool ownership before it sends a response.
    #[cfg(feature = "runtime-gateway-bridge")]
    #[doc(hidden)]
    #[must_use]
    pub fn tool_response_sender(&self) -> mpsc::UnboundedSender<ToolResponseMessage> {
        self.inner.tool_response_sender()
    }

    /// Cancel active work and wait for the native actor's cleanup barrier.
    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }
}

/// A fresh, single-turn embedding with controlled event and approval handling.
///
/// Construct this type with [`EmbeddedAgentBuilder::start_runner`]. It owns
/// the native command handle and event receiver, so no other consumer can
/// queue work or drain events. [`Self::run`] collects progress until the turn
/// completes or needs a tool decision. [`Self::start`] and
/// [`Self::next_event`] expose the same turn as an event stream. A gated tool
/// call becomes [`EmbeddedRunEvent::AwaitingTool`] and must be resolved before
/// another event can be read.
pub struct EmbeddedAgentRunner {
    agent: Option<NativeAgent>,
    events: EmbeddedAgentEvents,
    external_tools: HashSet<String>,
    state: EmbeddedRunnerState,
    response: ResponseCollection,
    #[cfg(feature = "test-support")]
    drop_shutdown_complete: Option<tokio::sync::oneshot::Sender<()>>,
}

impl EmbeddedAgentRunner {
    /// Start and collect this runner's only supported prompt.
    ///
    /// This pauses at a gated tool call instead of approving it automatically.
    /// Use [`Self::external_result`], [`Self::approve_host_tool`], or
    /// [`Self::deny_tool`], then [`Self::resume`].
    pub async fn run(
        &mut self,
        content: impl Into<String>,
    ) -> EmbeddedRunResult<EmbeddedRunProgress> {
        self.run_with_attachments(content, Vec::new()).await
    }

    /// Start and collect this runner's only supported prompt with
    /// caller-selected attachment paths.
    pub async fn run_with_attachments(
        &mut self,
        content: impl Into<String>,
        attachments: Vec<String>,
    ) -> EmbeddedRunResult<EmbeddedRunProgress> {
        self.start_with_attachments(content, attachments).await?;
        self.collect_until_pause().await
    }

    /// Queue this runner's only supported prompt for event-by-event handling.
    ///
    /// Use [`Self::next_event`] to receive streamed events and the terminal
    /// result. A runner cannot accept another prompt after this call, even if
    /// the turn later errors or is cancelled.
    pub async fn start(&mut self, content: impl Into<String>) -> EmbeddedRunResult<()> {
        self.start_with_attachments(content, Vec::new()).await
    }

    /// Queue this runner's only supported prompt with caller-selected
    /// attachment paths for event-by-event handling.
    pub async fn start_with_attachments(
        &mut self,
        content: impl Into<String>,
        attachments: Vec<String>,
    ) -> EmbeddedRunResult<()> {
        match &self.state {
            EmbeddedRunnerState::Ready => {}
            EmbeddedRunnerState::Running
            | EmbeddedRunnerState::AwaitingTool(_)
            | EmbeddedRunnerState::Cancelling => return Err(EmbeddedRunError::AlreadyStarted),
            EmbeddedRunnerState::Terminal => return Err(EmbeddedRunError::AlreadyStarted),
        }

        self.response = ResponseCollection::default();
        if let Err(error) = self.agent().prompt(content.into(), attachments).await {
            self.state = EmbeddedRunnerState::Terminal;
            return Err(EmbeddedRunError::Prompt {
                message: error.to_string(),
            });
        }
        self.state = EmbeddedRunnerState::Running;
        Ok(())
    }

    /// Receive the next native event or runner state transition.
    ///
    /// A non-gated tool call remains an ordinary [`EmbeddedRunEvent::Event`]
    /// because the existing host executes it. A gated tool call becomes an
    /// [`EmbeddedRunEvent::AwaitingTool`] and pauses this method until the
    /// caller supplies one permitted decision.
    pub async fn next_event(&mut self) -> EmbeddedRunResult<EmbeddedRunEvent> {
        match &self.state {
            EmbeddedRunnerState::Ready => return Err(EmbeddedRunError::NotStarted),
            EmbeddedRunnerState::AwaitingTool(_) => {
                return Err(EmbeddedRunError::AwaitingToolResponse);
            }
            EmbeddedRunnerState::Terminal => return Err(EmbeddedRunError::Terminal),
            EmbeddedRunnerState::Running | EmbeddedRunnerState::Cancelling => {}
        }

        let Some(event) = self.events.recv().await else {
            self.state = EmbeddedRunnerState::Terminal;
            return Err(EmbeddedRunError::EventStreamClosed);
        };

        self.record_response_event(&event);
        self.classify_event(event)
    }

    /// Continue collecting after a permitted tool decision.
    ///
    /// This is also useful after [`Self::start`] when no raw event handling is
    /// needed. It never sends a response or approves a tool on the caller's
    /// behalf.
    pub async fn resume(&mut self) -> EmbeddedRunResult<EmbeddedRunProgress> {
        match &self.state {
            EmbeddedRunnerState::Ready => return Err(EmbeddedRunError::NotStarted),
            EmbeddedRunnerState::AwaitingTool(_) => {
                return Err(EmbeddedRunError::AwaitingToolResponse);
            }
            EmbeddedRunnerState::Terminal => return Err(EmbeddedRunError::Terminal),
            EmbeddedRunnerState::Running | EmbeddedRunnerState::Cancelling => {}
        }
        self.collect_until_pause().await
    }

    /// Approve a pending local-host or MCP tool call.
    ///
    /// The response carries no caller-supplied result. The existing native
    /// host reruns its hook, firewall, sandbox, and execution checks.
    pub fn approve_host_tool(&mut self, pending: &EmbeddedPendingTool) -> EmbeddedRunResult<()> {
        self.send_pending_response(
            pending,
            PendingToolOwner::Host,
            true,
            None,
            ExecutionSource::Native,
        )
    }

    /// Return a caller-produced result for a pending registered external tool.
    ///
    /// A local-host or MCP tool cannot receive an external result through this
    /// method.
    pub fn external_result(
        &mut self,
        pending: &EmbeddedPendingTool,
        result: ToolResult,
    ) -> EmbeddedRunResult<()> {
        self.send_pending_response(
            pending,
            PendingToolOwner::External,
            true,
            Some(result),
            ExecutionSource::RemoteClient,
        )
    }

    /// Deny a pending external, local-host, or MCP tool call.
    pub fn deny_tool(&mut self, pending: &EmbeddedPendingTool) -> EmbeddedRunResult<()> {
        let owner = pending.owner();
        let source = match owner {
            PendingToolOwner::External => ExecutionSource::RemoteClient,
            PendingToolOwner::Host => ExecutionSource::Native,
        };
        self.send_pending_response(pending, owner, false, None, source)
    }

    /// Request cancellation of the active prompt or approval wait.
    ///
    /// Continue calling [`Self::next_event`] to observe the resulting terminal
    /// interruption. [`Self::shutdown`] awaits the actor cleanup barrier.
    pub fn cancel(&mut self) -> EmbeddedRunResult<()> {
        match &self.state {
            EmbeddedRunnerState::Running | EmbeddedRunnerState::AwaitingTool(_) => {
                self.state = EmbeddedRunnerState::Cancelling;
                self.agent().cancel();
                Ok(())
            }
            EmbeddedRunnerState::Ready => Err(EmbeddedRunError::NotStarted),
            EmbeddedRunnerState::Cancelling => Err(EmbeddedRunError::Terminal),
            EmbeddedRunnerState::Terminal => Err(EmbeddedRunError::Terminal),
        }
    }

    /// Cancel any active work and wait until the native actor has cleaned up.
    ///
    /// This is the deterministic cleanup barrier. Ordinary drop starts best
    /// effort cancellation but cannot await the native actor.
    pub async fn shutdown(mut self) {
        if let Some(agent) = self.agent.take() {
            agent.shutdown().await;
        }
    }

    fn agent(&self) -> &NativeAgent {
        self.agent
            .as_ref()
            .expect("embedded runner agent is taken only during shutdown or drop")
    }

    #[cfg(feature = "test-support")]
    fn with_drop_shutdown_observer(mut self) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.drop_shutdown_complete = Some(sender);
        (self, receiver)
    }

    async fn collect_until_pause(&mut self) -> EmbeddedRunResult<EmbeddedRunProgress> {
        loop {
            match self.next_event().await? {
                EmbeddedRunEvent::Event(_) => {}
                EmbeddedRunEvent::AwaitingTool(pending) => {
                    return Ok(EmbeddedRunProgress::AwaitingTool(pending));
                }
                EmbeddedRunEvent::Completed(completed) => {
                    return Ok(EmbeddedRunProgress::Completed(completed));
                }
            }
        }
    }

    fn record_response_event(&mut self, event: &FromAgent) {
        match event {
            FromAgent::ResponseStart { response_id } => {
                self.response = ResponseCollection {
                    response_id: Some(response_id.clone()),
                    ..ResponseCollection::default()
                };
            }
            FromAgent::ResponseChunk {
                response_id,
                content,
                is_thinking: false,
            } if self.response.response_id.as_deref() == Some(response_id) => {
                self.response.output.push_str(content);
            }
            FromAgent::ResponseEnd { response_id, usage }
                if self.response.response_id.as_deref() == Some(response_id) =>
            {
                self.response.final_response_usage = usage.clone();
            }
            _ => {}
        }
    }

    fn classify_event(&mut self, event: FromAgent) -> EmbeddedRunResult<EmbeddedRunEvent> {
        match &event {
            FromAgent::ToolCall {
                call_id,
                tool,
                args,
                requires_approval: true,
                ..
            } if !matches!(&self.state, EmbeddedRunnerState::Cancelling) => {
                let owner = if self.external_tools.contains(&tool.to_ascii_lowercase()) {
                    PendingToolOwner::External
                } else {
                    PendingToolOwner::Host
                };
                let pending_token = Arc::new(RunnerToken);
                let pending = EmbeddedToolCall {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    arguments: args.clone(),
                    runner_token: Arc::clone(&pending_token),
                };
                self.state = EmbeddedRunnerState::AwaitingTool(PendingToolState {
                    call_id: call_id.clone(),
                    owner,
                    runner_token: pending_token,
                });
                Ok(EmbeddedRunEvent::AwaitingTool(match owner {
                    PendingToolOwner::External => EmbeddedPendingTool::External(pending),
                    PendingToolOwner::Host => EmbeddedPendingTool::Host(pending),
                }))
            }
            FromAgent::TurnCompleted { response_id, .. } => {
                let cancelling = matches!(&self.state, EmbeddedRunnerState::Cancelling);
                self.state = EmbeddedRunnerState::Terminal;
                if cancelling {
                    Err(EmbeddedRunError::CompletedAfterCancellation)
                } else {
                    let response = std::mem::take(&mut self.response);
                    Ok(EmbeddedRunEvent::Completed(EmbeddedRunCompleted {
                        response_id: response.response_id.unwrap_or_else(|| response_id.clone()),
                        output: response.output,
                        final_response_usage: response.final_response_usage,
                    }))
                }
            }
            FromAgent::TurnInterrupted {
                response_id,
                reason,
            } => {
                self.state = EmbeddedRunnerState::Terminal;
                Err(EmbeddedRunError::Interrupted {
                    response_id: response_id.clone(),
                    reason: reason.clone(),
                })
            }
            FromAgent::ProviderError { kind, message } => {
                self.state = EmbeddedRunnerState::Terminal;
                Err(EmbeddedRunError::Provider {
                    kind: format!("{kind:?}"),
                    message: message.clone(),
                })
            }
            FromAgent::Error {
                message,
                fatal,
                terminal,
                retryable,
            } if *fatal || *terminal => {
                self.state = EmbeddedRunnerState::Terminal;
                Err(EmbeddedRunError::Runtime {
                    message: message.clone(),
                    fatal: *fatal,
                    terminal: *terminal,
                    retryable: *retryable,
                })
            }
            _ => Ok(EmbeddedRunEvent::Event(Box::new(event))),
        }
    }

    fn send_pending_response(
        &mut self,
        pending: &EmbeddedPendingTool,
        required_owner: PendingToolOwner,
        approved: bool,
        result: Option<ToolResult>,
        source: ExecutionSource,
    ) -> EmbeddedRunResult<()> {
        let supplied_owner = pending.owner();
        if supplied_owner != required_owner {
            return Err(EmbeddedRunError::ToolResponse {
                message: format!(
                    "{} tool calls cannot receive this response",
                    pending.tool_call().tool()
                ),
            });
        }

        let (expected_call_id, expected_owner, expected_token) = match &self.state {
            EmbeddedRunnerState::AwaitingTool(expected) => (
                expected.call_id.clone(),
                expected.owner,
                Arc::clone(&expected.runner_token),
            ),
            EmbeddedRunnerState::Ready => {
                return Err(EmbeddedRunError::ToolResponse {
                    message: "no turn is active".to_owned(),
                });
            }
            EmbeddedRunnerState::Running => {
                return Err(EmbeddedRunError::ToolResponse {
                    message: "the runner is not waiting for a tool response".to_owned(),
                });
            }
            EmbeddedRunnerState::Cancelling => {
                return Err(EmbeddedRunError::ToolResponse {
                    message: "the turn is cancelling".to_owned(),
                });
            }
            EmbeddedRunnerState::Terminal => {
                return Err(EmbeddedRunError::ToolResponse {
                    message: "the turn already reached a terminal state".to_owned(),
                });
            }
        };
        if expected_owner != supplied_owner
            || expected_call_id != pending.tool_call().call_id()
            || !Arc::ptr_eq(&expected_token, &pending.tool_call().runner_token)
        {
            return Err(EmbeddedRunError::ToolResponse {
                message: "the response does not match the current pending tool call".to_owned(),
            });
        }

        // Change state before sending. A second response cannot race through a
        // shared handle, and a closed native channel leaves this runner terminal.
        self.state = EmbeddedRunnerState::Running;
        if self
            .agent()
            .tool_response_sender()
            .send((
                pending.tool_call().call_id().to_owned(),
                approved,
                result,
                source,
                None,
            ))
            .is_err()
        {
            self.state = EmbeddedRunnerState::Terminal;
            return Err(EmbeddedRunError::ToolResponseChannelClosed);
        }
        Ok(())
    }
}

impl Drop for EmbeddedAgentRunner {
    fn drop(&mut self) {
        let Some(agent) = self.agent.take() else {
            return;
        };

        // Drop cannot await the native actor. Begin cancellation synchronously
        // and, when a Tokio runtime is available, retain the actor in a cleanup
        // task that calls the same shutdown barrier as an explicit owner.
        agent.cancel();
        #[cfg(feature = "test-support")]
        let drop_shutdown_complete = self.drop_shutdown_complete.take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _cleanup = handle.spawn(async move {
                agent.shutdown().await;
                #[cfg(feature = "test-support")]
                if let Some(sender) = drop_shutdown_complete {
                    let _ = sender.send(());
                }
            });
        }
    }
}

/// A denial for one [`FromAgent::ToolCall`] emitted by a raw embedding session.
///
/// Caller-produced tool results require [`EmbeddedAgentRunner`], which binds
/// each result to a runner-issued external-tool capability.
pub struct EmbeddedToolResponse {
    call_id: String,
    approved: bool,
    result: Option<ToolResult>,
    source: ExecutionSource,
}

impl EmbeddedToolResponse {
    /// Deny an embedding-owned tool call without executing it.
    #[must_use]
    pub fn deny(call_id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            approved: false,
            result: None,
            source: ExecutionSource::RemoteClient,
        }
    }

    fn into_parts(self) -> (String, bool, Option<ToolResult>, ExecutionSource) {
        (self.call_id, self.approved, self.result, self.source)
    }
}

/// Deterministic embedding construction for developer tests and examples.
#[cfg(feature = "test-support")]
pub mod test_kit {
    use std::path::Path;

    use anyhow::Result;
    use tokio::sync::oneshot;

    use super::{EmbeddedAgentBuilder, EmbeddedAgentRunner, EmbeddedAgentSession};
    use crate::agent::{NativeAgentConfig, ToolDefinition};
    use crate::ai::{ScriptedClient, ScriptedResponse, UnifiedClient};
    use crate::state::ApprovalMode;

    const SCRIPTED_MODEL: &str = "scripted-replay/maestro-replay-v1";

    /// Builder backed by the deterministic scripted provider.
    pub struct ScriptedEmbeddingBuilder {
        builder: EmbeddedAgentBuilder,
        responses: Vec<ScriptedResponse>,
    }

    impl ScriptedEmbeddingBuilder {
        /// Start a scripted embedding with responses consumed in order.
        #[must_use]
        pub fn new(responses: Vec<ScriptedResponse>) -> Self {
            Self {
                builder: EmbeddedAgentBuilder::new(SCRIPTED_MODEL),
                responses,
            }
        }

        /// Start a scripted embedding from an existing native configuration.
        #[must_use]
        pub fn from_config(config: NativeAgentConfig) -> Self {
            Self {
                builder: EmbeddedAgentBuilder::from_config(config),
                responses: Vec::new(),
            }
        }

        /// Set the local workspace used by the existing host composition.
        #[must_use]
        pub fn working_directory(mut self, path: impl AsRef<Path>) -> Self {
            self.builder = self.builder.working_directory(path);
            self
        }

        /// Set the base instructions for the scripted session.
        #[must_use]
        pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
            self.builder = self.builder.system_prompt(prompt);
            self
        }

        /// Set the native local-host approval mode used by the scripted run.
        #[must_use]
        pub fn approval_mode(mut self, mode: ApprovalMode) -> Self {
            self.builder = self.builder.approval_mode(mode);
            self
        }

        /// Add caller-owned tools to the scripted session.
        #[must_use]
        pub fn external_tools(mut self, tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
            self.builder = self.builder.external_tools(tools);
            self
        }

        /// Compose the local host with a scripted provider.
        pub fn start(self) -> Result<EmbeddedAgentSession> {
            let model = self.builder.config.model.clone();
            let client = UnifiedClient::Scripted(ScriptedClient::new(model, self.responses));
            self.builder.start_with_test_client(client)
        }

        /// Compose a fresh runner with a scripted provider.
        pub fn start_runner(self) -> Result<EmbeddedAgentRunner> {
            let model = self.builder.config.model.clone();
            let client = UnifiedClient::Scripted(ScriptedClient::new(model, self.responses));
            self.builder.start_runner_with_test_client(client)
        }

        /// Compose a fresh runner whose test-only drop path reports when its
        /// native shutdown barrier has completed.
        pub fn start_runner_with_drop_observer(
            self,
        ) -> Result<(EmbeddedAgentRunner, oneshot::Receiver<()>)> {
            self.start_runner()
                .map(EmbeddedAgentRunner::with_drop_shutdown_observer)
        }
    }
}
