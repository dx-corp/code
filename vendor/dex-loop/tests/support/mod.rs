//! In-memory ports for scenario tests.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dex_loop::{
    ApprovalId, Budget, CallId, CancellationToken, Claim, ClientToolSpec, Context, Cursor, Effects,
    Engine, Entry, Event, ExecutorKind, Fenced, GovernanceClass, Lexicon, Log, Message, Model,
    ModelChunk, ModelError, NoCompaction, Outcome, Output, OutputRef, PrincipalId, ProposedCall,
    Summarize, ThreadId, ToolName, ToolResult, ToolSpec, Tools, TurnId, Usage, Verdict, rehydrate,
};
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::Barrier;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn thread() -> ThreadId {
    ThreadId {
        org: "org-1".into(),
        workspace: "ws-1".into(),
        thread: "thread-1".into(),
    }
}

pub fn alice() -> PrincipalId {
    PrincipalId::new("alice")
}

pub fn bob() -> PrincipalId {
    PrincipalId::new("bob")
}

pub fn call_id(turn: &str, step: u32, index: usize) -> CallId {
    CallId::new(format!("{turn}-{step}-{index}"))
}

pub type TestEngine<C = NoCompaction> =
    Engine<FakeLog, FakeModel, FakeTools, FakeEffects, Lexicon, C>;

pub fn engine(log: &FakeLog, model: &FakeModel, tools: &FakeTools, budget: Budget) -> TestEngine {
    engine_with(log, model, tools, &FakeEffects::default(), budget)
}

pub fn engine_with(
    log: &FakeLog,
    model: &FakeModel,
    tools: &FakeTools,
    effects: &FakeEffects,
    budget: Budget,
) -> TestEngine {
    Engine::new(
        log.clone(),
        model.clone(),
        tools.clone(),
        effects.clone(),
        Lexicon::default(),
        budget,
    )
}

// ---------------------------------------------------------------- Log

#[derive(Default)]
struct LogState {
    events: Vec<(Cursor, Event)>,
    text_writes: Vec<String>,
    /// Writes still allowed before every write is refused.
    allowed_writes: Option<usize>,
    refused: usize,
}

impl LogState {
    fn admit(&mut self) -> Result<(), Fenced> {
        if let Some(allowed) = &mut self.allowed_writes {
            if *allowed == 0 {
                self.refused += 1;
                return Err(Fenced::new("lease generation moved"));
            }
            *allowed -= 1;
        }
        Ok(())
    }

    fn push(&mut self, event: Event) -> Cursor {
        let cursor = Cursor(self.events.len() as i64 + 1);
        self.events.push((cursor, event));
        cursor
    }
}

/// Coalesces consecutive text into one `TextDelta` row, as a real log does.
#[derive(Clone, Default)]
pub struct FakeLog {
    state: Arc<Mutex<LogState>>,
}

impl FakeLog {
    /// A host-side write (ingress). Never fenced.
    pub fn host_append(&self, event: Event) -> Cursor {
        lock(&self.state).push(event)
    }

    /// Appends Alice's `UserMessage` and returns the rehydrated context.
    pub fn start_turn(&self, turn: &str, text: &str) -> Context {
        self.start_turn_with_client_tools(turn, text, Vec::new())
    }

    /// Appends Alice's `UserMessage`, declaring `client_tools`, and returns
    /// the rehydrated context.
    pub fn start_turn_with_client_tools(
        &self,
        turn: &str,
        text: &str,
        client_tools: Vec<ClientToolSpec>,
    ) -> Context {
        self.host_append(Event::UserMessage {
            turn: TurnId::new(turn),
            message_id: None,
            principal: alice(),
            text: text.into(),
            attachments: Vec::new(),
            client_tools,
            authorized_tools: Vec::new(),
            approval_mode: dex_loop::ApprovalMode::Interactive,
        });
        self.rehydrate()
    }

    /// Appends Alice's `UserMessage` under `approval_mode` and returns the
    /// rehydrated context.
    pub fn start_turn_with_approval_mode(
        &self,
        turn: &str,
        text: &str,
        approval_mode: dex_loop::ApprovalMode,
    ) -> Context {
        self.host_append(Event::UserMessage {
            turn: TurnId::new(turn),
            message_id: None,
            principal: alice(),
            text: text.into(),
            attachments: Vec::new(),
            client_tools: Vec::new(),
            authorized_tools: Vec::new(),
            approval_mode,
        });
        self.rehydrate()
    }

    /// The client's report for a `ClientToolRequested` call.
    pub fn submit_tool_result(&self, call: &CallId, outcome: Outcome, output: &str) {
        self.host_append(Event::ClientToolResult {
            call: call.clone(),
            principal: alice(),
            outcome,
            output: output.into(),
        });
    }

    pub fn rehydrate(&self) -> Context {
        rehydrate(thread(), &self.entries())
    }

    pub fn entries(&self) -> Vec<(Cursor, Event)> {
        lock(&self.state).events.clone()
    }

    pub fn events(&self) -> Vec<Event> {
        self.entries().into_iter().map(|(_, event)| event).collect()
    }

    pub fn shapes(&self) -> Vec<String> {
        self.events().iter().map(shape).collect()
    }

    /// Shapes of the events after the first `skip`.
    pub fn shapes_after(&self, skip: usize) -> Vec<String> {
        self.shapes().into_iter().skip(skip).collect()
    }

    pub fn len(&self) -> usize {
        lock(&self.state).events.len()
    }

    /// Every `append_text` call, before coalescing.
    pub fn text_writes(&self) -> Vec<String> {
        lock(&self.state).text_writes.clone()
    }

    /// The engine may make `writes` more writes; later writes fail.
    pub fn fence_after(&self, writes: usize) {
        lock(&self.state).allowed_writes = Some(writes);
    }

    pub fn refused(&self) -> usize {
        lock(&self.state).refused
    }

    /// The digest the engine put on the approval request for `call`.
    pub fn requested_digest(&self, call: &CallId) -> String {
        self.events()
            .into_iter()
            .find_map(|event| match event {
                Event::ApprovalRequested {
                    call: requested,
                    args_digest,
                    ..
                } if &requested == call => Some(args_digest),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no approval requested for {call}"))
    }

    pub fn decide(&self, call: &CallId, approval: &str, approved: bool) {
        let args_digest = self.requested_digest(call);
        self.host_append(Event::ApprovalDecided {
            call: call.clone(),
            approval: ApprovalId::new(approval),
            args_digest,
            approved,
            principal: alice(),
        });
    }
}

impl Log for FakeLog {
    async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
        let mut state = lock(&self.state);
        state.admit()?;
        Ok(events
            .iter()
            .map(|event| state.push(event.clone()))
            .collect())
    }

    async fn append_text(&self, text: String) -> Result<(), Fenced> {
        let mut state = lock(&self.state);
        state.admit()?;
        state.text_writes.push(text.clone());
        if let Some((_, Event::TextDelta { text: row })) = state.events.last_mut() {
            row.push_str(&text);
        } else {
            state.push(Event::TextDelta { text });
        }
        Ok(())
    }

    async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
        Ok(lock(&self.state)
            .events
            .iter()
            .filter(|(cursor, event)| *cursor > after && event.is_control())
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------- Model

pub fn text(text: &str) -> Result<ModelChunk, ModelError> {
    Ok(ModelChunk::Text(text.into()))
}

pub fn call(name: &str, args: serde_json::Value) -> Result<ModelChunk, ModelError> {
    Ok(ModelChunk::ToolCall {
        name: ToolName::new(name),
        args,
    })
}

pub fn usage(
    input_tokens: u64,
    output_tokens: u64,
    cost_micros: u64,
) -> Result<ModelChunk, ModelError> {
    Ok(ModelChunk::Usage(Usage {
        input_tokens,
        output_tokens,
        cost_micros,
    }))
}

#[derive(Default)]
struct ModelState {
    scripts: VecDeque<Vec<Result<ModelChunk, ModelError>>>,
    chunk_delay: Duration,
    seen: Vec<Vec<Message>>,
    offered: Vec<Vec<String>>,
}

/// Replays one script per model call and records what each call was sent.
#[derive(Clone, Default)]
pub struct FakeModel {
    state: Arc<Mutex<ModelState>>,
}

impl FakeModel {
    pub fn new(scripts: Vec<Vec<Result<ModelChunk, ModelError>>>) -> Self {
        let model = Self::default();
        lock(&model.state).scripts = scripts.into();
        model
    }

    pub fn with_chunk_delay(self, delay: Duration) -> Self {
        lock(&self.state).chunk_delay = delay;
        self
    }

    /// The history of every model call, in order.
    pub fn seen(&self) -> Vec<Vec<Message>> {
        lock(&self.state).seen.clone()
    }

    /// The tool names offered to every model call, in order.
    pub fn offered(&self) -> Vec<Vec<String>> {
        lock(&self.state).offered.clone()
    }

    pub fn calls(&self) -> usize {
        lock(&self.state).seen.len()
    }
}

impl Model for FakeModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let mut state = lock(&self.state);
        state.seen.push(
            ctx.history()
                .iter()
                .map(|entry| entry.message.clone())
                .collect(),
        );
        state
            .offered
            .push(tools.iter().map(|spec| spec.name.to_string()).collect());
        let script = state.scripts.pop_front().unwrap_or_else(|| {
            vec![Err(ModelError {
                message: "no script left".into(),
            })]
        });
        let delay = state.chunk_delay;
        stream::iter(script).then(move |chunk| async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            chunk
        })
    }
}

// ---------------------------------------------------------------- Tools

pub fn read_tool(name: &str) -> ToolSpec {
    spec(name, true, true, ExecutorKind::InProcess)
}

pub fn write_tool(name: &str) -> ToolSpec {
    spec(name, false, true, ExecutorKind::ToolExecutor)
}

pub fn ask_tool(name: &str) -> ToolSpec {
    spec(name, true, true, ExecutorKind::User)
}

/// A tool a test's client session declares on `Send`, exactly as
/// `Context::client_tools` stores it (unresolved: the client's own claim).
/// Composing that into an actual `Tools::catalog()` entry is a host concern
/// (`dex_tools::client::declare` in dex-runtime); a dex-loop-level test that
/// wants a dispatchable `Client`-executor tool uses `client_executed_tool`
/// on `FakeTools` directly instead.
pub fn client_tool(name: &str, read_only: bool) -> ClientToolSpec {
    ClientToolSpec {
        name: ToolName::new(name),
        schema: serde_json::json!({"type": "object"}),
        read_only,
        label: format!("Label for {name}"),
    }
}

/// A catalog entry with `ExecutorKind::Client`, as a host's `Tools::catalog`
/// would offer one already resolved from a client's declaration.
pub fn client_executed_tool(name: &str, read_only: bool) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        label: format!("Label for {name}"),
        schema: serde_json::json!({"type": "object"}),
        read_only,
        core: true,
        governance: if read_only {
            GovernanceClass::Plain
        } else {
            GovernanceClass::Approval
        },
        executor: ExecutorKind::Client,
    }
}

/// Not core: offered only after `tools.search` exposes it.
pub fn hidden_read_tool(name: &str) -> ToolSpec {
    spec(name, true, false, ExecutorKind::ToolExecutor)
}

fn spec(name: &str, read_only: bool, core: bool, executor: ExecutorKind) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        label: format!("Label for {name}"),
        schema: serde_json::json!({"type": "object"}),
        read_only,
        core,
        governance: GovernanceClass::Plain,
        executor,
    }
}

#[derive(Clone, Debug)]
pub struct RunRecord {
    pub call: CallId,
    /// The thread `Tools::run` was called with. `CallId` alone is unique
    /// only within a thread, so tests that check downstream uniqueness
    /// assert on the pair.
    pub thread: ThreadId,
    pub args: serde_json::Value,
    pub started: Instant,
    pub finished: Instant,
    pub cancelled: bool,
}

type Hook = Arc<dyn Fn(&ProposedCall) + Send + Sync>;

#[derive(Default)]
struct ToolState {
    verdicts: HashMap<String, Verdict>,
    principal_verdicts: HashMap<(String, PrincipalId), Verdict>,
    searches: HashMap<String, Vec<ToolName>>,
    delays: HashMap<String, Duration>,
    barrier: Option<(Arc<Barrier>, Vec<String>)>,
    on_run: Option<Hook>,
    runs: Vec<RunRecord>,
    policy_checks: Vec<(CallId, PrincipalId)>,
    /// Every call `Tools::wrap_client_result` was asked to finish, in order.
    wrapped: Vec<CallId>,
}

#[derive(Clone)]
pub struct FakeTools {
    catalog: Arc<[ToolSpec]>,
    state: Arc<Mutex<ToolState>>,
}

impl FakeTools {
    pub fn new(catalog: Vec<ToolSpec>) -> Self {
        Self {
            catalog: catalog.into(),
            state: Arc::default(),
        }
    }

    pub fn verdict(self, tool: &str, verdict: Verdict) -> Self {
        self.set_verdict(tool, verdict);
        self
    }

    /// Changes policy for later checks (a grant revoked while parked).
    pub fn set_verdict(&self, tool: &str, verdict: Verdict) {
        lock(&self.state).verdicts.insert(tool.into(), verdict);
    }

    /// Policy for `tool` when the call acts under `principal`.
    pub fn verdict_for(self, tool: &str, principal: PrincipalId, verdict: Verdict) -> Self {
        lock(&self.state)
            .principal_verdicts
            .insert((tool.into(), principal), verdict);
        self
    }

    pub fn search_result(self, query: &str, tools: &[&str]) -> Self {
        lock(&self.state).searches.insert(
            query.into(),
            tools.iter().map(|t| ToolName::new(*t)).collect(),
        );
        self
    }

    /// Calls whose `args.key` equals `key` sleep for `delay`.
    pub fn delay(self, key: &str, delay: Duration) -> Self {
        lock(&self.state).delays.insert(key.into(), delay);
        self
    }

    /// Calls with these `args.key` values wait for each other before
    /// sleeping. Serial dispatch would deadlock; the timeout turns that into
    /// an error result.
    pub fn barrier(self, keys: &[&str]) -> Self {
        lock(&self.state).barrier = Some((
            Arc::new(Barrier::new(keys.len())),
            keys.iter().map(|key| (*key).to_owned()).collect(),
        ));
        self
    }

    pub fn on_run(self, hook: impl Fn(&ProposedCall) + Send + Sync + 'static) -> Self {
        lock(&self.state).on_run = Some(Arc::new(hook));
        self
    }

    pub fn runs(&self) -> Vec<RunRecord> {
        lock(&self.state).runs.clone()
    }

    pub fn run_ids(&self) -> Vec<String> {
        self.runs().iter().map(|run| run.call.to_string()).collect()
    }

    pub fn run_of(&self, call: &CallId) -> RunRecord {
        self.runs()
            .into_iter()
            .find(|run| &run.call == call)
            .unwrap_or_else(|| panic!("{call} never ran"))
    }

    pub fn policy_checks(&self) -> Vec<(String, String)> {
        lock(&self.state)
            .policy_checks
            .iter()
            .map(|(call, principal)| (call.to_string(), principal.to_string()))
            .collect()
    }

    /// Every call id `wrap_client_result` was asked to finish, in order --
    /// the engine's own record of when it routed a client-reported result
    /// through the host, whether live or on replay.
    pub fn wrapped_calls(&self) -> Vec<CallId> {
        lock(&self.state).wrapped.clone()
    }
}

fn key(call: &ProposedCall) -> String {
    call.args
        .get("key")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub fn output_for(call: &CallId) -> ToolResult {
    ToolResult::stored(OutputRef::new(format!("out/{call}")), None)
}

impl Tools for FakeTools {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }

    async fn search(&self, _principal: &PrincipalId, query: &str) -> Vec<ToolName> {
        lock(&self.state)
            .searches
            .get(query)
            .cloned()
            .unwrap_or_default()
    }

    async fn policy(&self, _ctx: &Context, call: &ProposedCall) -> Verdict {
        let mut state = lock(&self.state);
        state
            .policy_checks
            .push((call.id.clone(), call.principal.clone()));
        let by_principal = (call.tool.to_string(), call.principal.clone());
        state
            .principal_verdicts
            .get(&by_principal)
            .or_else(|| state.verdicts.get(call.tool.as_str()))
            .cloned()
            .unwrap_or(Verdict::Allow)
    }

    async fn run(
        &self,
        thread: &ThreadId,
        call: &ProposedCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let started = Instant::now();
        let key = key(call);
        let (delay, barrier, hook) = {
            let state = lock(&self.state);
            let barrier = state
                .barrier
                .as_ref()
                .filter(|(_, keys)| keys.contains(&key))
                .map(|(barrier, _)| Arc::clone(barrier));
            (
                state.delays.get(&key).copied().unwrap_or_default(),
                barrier,
                state.on_run.clone(),
            )
        };
        if let Some(hook) = hook {
            hook(call);
        }
        let mut result = output_for(&call.id);
        let mut cancelled = false;
        if let Some(barrier) = barrier
            && tokio::time::timeout(Duration::from_secs(5), barrier.wait())
                .await
                .is_err()
        {
            result = ToolResult::error("barrier timed out: calls did not overlap");
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => {
                cancelled = true;
                result = ToolResult::error("cancelled");
            }
        }
        lock(&self.state).runs.push(RunRecord {
            call: call.id.clone(),
            thread: thread.clone(),
            args: call.args.clone(),
            started,
            finished: Instant::now(),
            cancelled,
        });
        result
    }

    /// Marks that this call's result was wrapped, and rewrites a text output
    /// so a test can tell a wrapped result from the client's raw one.
    async fn wrap_client_result(
        &self,
        _thread: &ThreadId,
        call: &ProposedCall,
        raw: ToolResult,
    ) -> ToolResult {
        lock(&self.state).wrapped.push(call.id.clone());
        match raw.output {
            Output::Text(text) => ToolResult {
                output: Output::Text(format!("wrapped:{text}")),
                ..raw
            },
            output => ToolResult { output, ..raw },
        }
    }
}

// ---------------------------------------------------------------- Effects

/// A ledger: claimed calls map to their recorded result, or `None` while the
/// outcome is not recorded.
#[derive(Clone, Default)]
pub struct FakeEffects {
    ledger: Arc<Mutex<HashMap<CallId, Option<ToolResult>>>>,
}

impl FakeEffects {
    /// A claim from before a crash, with or without a recorded outcome.
    pub fn seed(self, call: CallId, recorded: Option<ToolResult>) -> Self {
        lock(&self.ledger).insert(call, recorded);
        self
    }

    pub fn recorded(&self, call: &CallId) -> Option<Option<ToolResult>> {
        lock(&self.ledger).get(call).cloned()
    }
}

impl Effects for FakeEffects {
    async fn claim(&self, call: &ProposedCall) -> Result<Claim, Fenced> {
        let mut ledger = lock(&self.ledger);
        match ledger.get(&call.id) {
            Some(Some(result)) => Ok(Claim::Existing(result.clone())),
            Some(None) => Ok(Claim::Existing(ToolResult {
                outcome: Outcome::Running,
                output: Output::Text("dispatched; no outcome recorded yet".into()),
                receipt: None,
            })),
            None => {
                ledger.insert(call.id.clone(), None);
                Ok(Claim::Granted)
            }
        }
    }

    async fn record(&self, call: &CallId, result: &ToolResult) -> Result<(), Fenced> {
        lock(&self.ledger).insert(call.clone(), Some(result.clone()));
        Ok(())
    }
}

// ---------------------------------------------------------------- Summarizer

#[derive(Clone, Default)]
pub struct FakeSummarizer;

impl Summarize for FakeSummarizer {
    async fn summarize(&self, entries: &[Entry]) -> Option<String> {
        Some(format!("summary of {} entries", entries.len()))
    }
}

// ---------------------------------------------------------------- Assertions

fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Succeeded => "ok",
        Outcome::Failed => "err",
        Outcome::Running => "running",
        Outcome::Unknown => "unknown",
    }
}

fn ids(calls: &[ProposedCall]) -> String {
    calls
        .iter()
        .map(|call| call.id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// A compact, readable form of an event for sequence assertions.
pub fn shape(event: &Event) -> String {
    match event {
        Event::UserMessage { text, .. } => format!("user:{text}"),
        Event::Steer { text, .. } => format!("steer:{text}"),
        Event::Interrupt { .. } => "interrupt".into(),
        Event::ApprovalDecided { call, approved, .. } => format!("decided:{call}:{approved}"),
        Event::Answer { call, text, .. } => format!("answer:{call}:{text}"),
        Event::StepStarted { step, .. } => format!("step:{step}"),
        Event::TextDelta { text } => format!("delta:{text}"),
        Event::Usage(usage) => format!("usage:{}", usage.tokens()),
        Event::ModelStepCompleted { text, calls, .. } => {
            format!("completed:{text}:[{}]", ids(calls))
        }
        Event::ModelAttemptAbandoned { step } => format!("abandoned:{step}"),
        Event::ToolStarted { call, .. } => format!("started:{call}"),
        Event::ToolProgress { call, label } => format!("progress:{call}:{label}"),
        Event::ToolsExposed { tools, .. } => format!(
            "exposed:[{}]",
            tools
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Event::ToolFinished {
            call,
            outcome: result,
            ..
        } => format!("finished:{call}:{}", outcome(*result)),
        Event::ApprovalRequested { call, .. } => format!("approval:{call}"),
        Event::AutoApproved { call, .. } => format!("auto_approved:{call}"),
        Event::Question { call, text } => format!("question:{call}:{text}"),
        Event::ClientToolRequested { call, tool, .. } => format!("client_tool:{call}:{tool}"),
        Event::ClientToolResult {
            call,
            outcome: result,
            ..
        } => format!("client_tool_result:{call}:{}", outcome(*result)),
        Event::Compaction { summary, .. } => format!("compaction:{summary}"),
        Event::Final { text } => format!("final:{text}"),
        Event::Error { code, message } => format!("error:{}:{message}", code.as_str()),
        Event::Interrupted => "interrupted".into(),
    }
}

/// A compact form of the model's view.
pub fn view(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::User { text, .. } => format!("user:{text}"),
            Message::Assistant { text, calls, .. } => {
                format!("assistant:{text}:[{}]", ids(calls))
            }
            Message::Tool {
                call,
                outcome: result,
                output,
                ..
            } => {
                let output = match output {
                    Output::Ref(reference) => reference.to_string(),
                    Output::Text(text) => text.clone(),
                };
                format!("tool:{call}:{}:{output}", outcome(*result))
            }
            Message::Summary { text } => format!("summary:{text}"),
        })
        .collect()
}

pub fn history(ctx: &Context) -> Vec<String> {
    view(
        &ctx.history()
            .iter()
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>(),
    )
}

pub fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

pub fn approval(id: &str) -> Verdict {
    Verdict::NeedsApproval {
        approval: ApprovalId::new(id),
        summary: format!("Approve {id}"),
    }
}

/// Log rows for a turn that crashed after `ToolStarted` for `call`.
pub fn crashed_after_start(log: &FakeLog, call: &ProposedCall) {
    for event in [
        Event::UserMessage {
            turn: TurnId::new("t1"),
            message_id: None,
            principal: alice(),
            text: "do it".into(),
            attachments: vec![],
            client_tools: vec![],
            authorized_tools: Vec::new(),
            approval_mode: dex_loop::ApprovalMode::Interactive,
        },
        Event::StepStarted {
            step: 1,
            control_through: Cursor::START,
        },
        Event::ModelStepCompleted {
            step: 1,
            text: String::new(),
            calls: vec![call.clone()],
            reasoning: None,
        },
        Event::ToolStarted {
            call: call.id.clone(),
            tool: call.tool.clone(),
            label: format!("Label for {}", call.tool),
            principal: call.principal.clone(),
        },
    ] {
        log.host_append(event);
    }
}
