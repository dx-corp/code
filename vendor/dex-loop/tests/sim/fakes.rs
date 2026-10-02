//! Seeded, adversarial in-memory ports.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dex_loop::{
    CallId, CancellationToken, Claim, Context, Cursor, Effects, Event, Fenced, Log, Model,
    ModelChunk, ModelError, Outcome, Output, OutputRef, PrincipalId, ProposedCall, ThreadId,
    ToolName, ToolResult, ToolSpec, Tools, Verdict,
};
use futures_util::{Stream, stream};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

// --------------------------------------------------------------- Fence

/// A lease-like generation counter shared by every simulated replica of one
/// thread. Advancing it fences every older handle at once: the same CAS
/// dex-runtime's real lease does with `lease_generation` in Postgres
/// (`lease::acquire`/`lease::finish`), modeled here without a database.
#[derive(Clone, Default)]
pub struct Fence(Arc<AtomicU64>);

impl Fence {
    pub fn generation(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// A new replica takes the lease: bumps the generation and returns it.
    pub fn steal(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
}

// --------------------------------------------------------------- CrashBudget

/// Lets a seed pick one operation, across a whole simulated `Engine::run`
/// attempt, to hang forever: every port operation calls `checkpoint()` once
/// it has made whatever mutation it makes durable, so hanging there models
/// the process dying with that mutation already landed and nothing that
/// would have happened afterward (including the engine's own continuation)
/// ever happening. The caller races the whole `Engine::run` against a wall
/// budget (paused virtual time; free in real time either way) and treats a
/// timeout exactly like a crash: drop the future, rehydrate, continue with a
/// fresh `Engine`.
#[derive(Clone)]
pub struct CrashBudget {
    counter: Arc<AtomicU64>,
    crash_at: Option<u64>,
}

impl CrashBudget {
    pub fn none() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
            crash_at: None,
        }
    }

    pub fn at(n: u64) -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
            crash_at: Some(n),
        }
    }

    pub async fn checkpoint(&self) {
        let seen = self.counter.fetch_add(1, Ordering::SeqCst);
        if self.crash_at == Some(seen) {
            std::future::pending::<()>().await;
        }
    }
}

// --------------------------------------------------------------- SimLog

#[derive(Default)]
struct LogState {
    events: Vec<(Cursor, Event)>,
}

impl LogState {
    fn push(&mut self, event: Event) -> Cursor {
        let cursor = Cursor(self.events.len() as i64 + 1);
        self.events.push((cursor, event));
        cursor
    }
}

/// The thread's event log: durable, cursor-ordered, and fenced once a later
/// replica has stolen the lease (see `Fence`).
#[derive(Clone)]
pub struct SimLog {
    state: Arc<Mutex<LogState>>,
    fence: Fence,
    generation: u64,
    crash: CrashBudget,
}

impl SimLog {
    pub fn new(crash: CrashBudget) -> Self {
        Self {
            state: Arc::default(),
            fence: Fence::default(),
            generation: 0,
            crash,
        }
    }

    /// A handle for a replica running at `generation`: shares the same
    /// durable state as every other handle from this log, but every write
    /// through it is refused the instant the fence moves past `generation`.
    pub fn for_replica(&self, generation: u64, crash: CrashBudget) -> Self {
        Self {
            state: Arc::clone(&self.state),
            fence: self.fence.clone(),
            generation,
            crash,
        }
    }

    pub fn fence(&self) -> &Fence {
        &self.fence
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn fenced(&self) -> bool {
        self.fence.generation() != self.generation
    }

    /// A host-side ingress write (`send`/`steer`/`interrupt`/`approve`/
    /// `answer`): never fenced, since ingress in dex-runtime writes through
    /// a thread-row lock, not the actor's lease.
    pub fn host_append(&self, event: Event) -> Cursor {
        lock(&self.state).push(event)
    }

    pub fn entries(&self) -> Vec<(Cursor, Event)> {
        lock(&self.state).events.clone()
    }

    /// Every event with a cursor greater than `after`, in log order: the
    /// same contract a `Watch` resumption offers a subscriber.
    pub fn since(&self, after: Cursor) -> Vec<(Cursor, Event)> {
        lock(&self.state)
            .events
            .iter()
            .filter(|(cursor, _)| *cursor > after)
            .cloned()
            .collect()
    }
}

impl Log for SimLog {
    async fn append(&self, events: &[Event]) -> Result<Vec<Cursor>, Fenced> {
        if self.fenced() {
            return Err(Fenced::new("lease generation moved"));
        }
        let cursors: Vec<Cursor> = {
            let mut state = lock(&self.state);
            events
                .iter()
                .map(|event| state.push(event.clone()))
                .collect()
        };
        self.crash.checkpoint().await;
        Ok(cursors)
    }

    async fn append_text(&self, text: String) -> Result<(), Fenced> {
        if self.fenced() {
            return Err(Fenced::new("lease generation moved"));
        }
        {
            let mut state = lock(&self.state);
            if let Some((_, Event::TextDelta { text: row })) = state.events.last_mut() {
                row.push_str(&text);
            } else {
                state.push(Event::TextDelta { text });
            }
        }
        self.crash.checkpoint().await;
        Ok(())
    }

    async fn control_since(&self, after: Cursor) -> Result<Vec<(Cursor, Event)>, Fenced> {
        // Reads are never fenced: a stale replica may still read (it just
        // cannot act on what it reads without a write landing), matching
        // `Log`'s doc ("the log or the effect ledger refused *a write*").
        Ok(lock(&self.state)
            .events
            .iter()
            .filter(|(cursor, event)| *cursor > after && event.is_control())
            .cloned()
            .collect())
    }
}

// --------------------------------------------------------------- SimEffects

/// The durable effect ledger: `CallId` to its recorded outcome, or `None`
/// while claimed but not yet recorded (the "crashed mid-dispatch" state that
/// settles to `Outcome::Unknown`).
#[derive(Clone)]
pub struct SimEffects {
    ledger: Arc<Mutex<HashMap<CallId, Option<ToolResult>>>>,
    fence: Fence,
    generation: u64,
    crash: CrashBudget,
}

impl SimEffects {
    pub fn new(crash: CrashBudget) -> Self {
        Self {
            ledger: Arc::default(),
            fence: Fence::default(),
            generation: 0,
            crash,
        }
    }

    pub fn for_replica(&self, generation: u64, crash: CrashBudget) -> Self {
        Self {
            ledger: Arc::clone(&self.ledger),
            fence: self.fence.clone(),
            generation,
            crash,
        }
    }

    pub fn fence(&self) -> &Fence {
        &self.fence
    }

    fn fenced(&self) -> bool {
        self.fence.generation() != self.generation
    }
}

impl Effects for SimEffects {
    async fn claim(&self, call: &ProposedCall) -> Result<Claim, Fenced> {
        if self.fenced() {
            return Err(Fenced::new("lease generation moved"));
        }
        let claim = {
            let mut ledger = lock(&self.ledger);
            match ledger.get(&call.id) {
                Some(Some(result)) => Claim::Existing(result.clone()),
                Some(None) => Claim::Existing(ToolResult {
                    outcome: Outcome::Running,
                    output: Output::Text("dispatched; no outcome recorded yet".into()),
                    receipt: None,
                }),
                None => {
                    ledger.insert(call.id.clone(), None);
                    Claim::Granted
                }
            }
        };
        self.crash.checkpoint().await;
        Ok(claim)
    }

    async fn record(&self, call: &CallId, result: &ToolResult) -> Result<(), Fenced> {
        if self.fenced() {
            return Err(Fenced::new("lease generation moved"));
        }
        lock(&self.ledger).insert(call.clone(), Some(result.clone()));
        self.crash.checkpoint().await;
        Ok(())
    }
}

// --------------------------------------------------------------- SimModel

/// What the model does on one step, chosen deterministically from
/// `(seed, turn, step)`. A pure function, never a shared mutable RNG: the
/// same seed produces the same script regardless of how concurrent work is
/// scheduled around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepScript {
    /// A plain answer with no tool calls; ends the turn.
    PlainAnswer,
    /// One call to the first read-only tool offered (falls back to a plain
    /// answer if none is offered).
    OneRead,
    /// One call to the first mutation tool offered.
    OneMutation,
    /// One call to the first `ExecutorKind::Client` tool offered.
    OneClientTool,
    /// The same call (same tool, same args) proposed twice in one step.
    DuplicateCalls,
    /// A call to a tool name that is not offered.
    UnknownTool,
    /// One chunk of text, then the stream ends with no `Err` and no more
    /// chunks -- truncated mid-answer, not abandoned.
    Truncated,
    /// One chunk of text, then the stream errors.
    Abandoned,
    /// The stream never yields anything at all (nor ends, nor errors).
    Stalled,
    /// A plain answer carrying an adversarial payload: NUL bytes, a huge
    /// string, or non-ASCII text with colon-bearing content.
    AdversarialPayload,
}

const STEP_SCRIPTS: [StepScript; 10] = [
    StepScript::PlainAnswer,
    StepScript::OneRead,
    StepScript::OneMutation,
    StepScript::OneClientTool,
    StepScript::DuplicateCalls,
    StepScript::UnknownTool,
    StepScript::Truncated,
    StepScript::Abandoned,
    StepScript::Stalled,
    StepScript::AdversarialPayload,
];

pub fn adversarial_payload(seed: u64) -> String {
    match seed % 4 {
        0 => "plain reply".to_owned(),
        1 => format!("has a NUL\u{0}byte and a colon: {seed}"),
        2 => "🜁 unicode 漢字 café \u{200b} zero-width".to_owned(),
        _ => "x".repeat(200_000),
    }
}

/// One `Model` whose per-step behavior is a pure function of `(seed, turn,
/// step)`: a fresh `SimModel` re-synthesizes the same sequence of steps a
/// crashed one already committed, so replays after a crash line up with what
/// the log actually recorded, but a *new* step (one that was never
/// committed) can still land differently across attempts, as a real
/// non-deterministic model would.
#[derive(Clone)]
pub struct SimModel {
    seed: u64,
    only: Option<StepScript>,
}

impl SimModel {
    pub fn new(seed: u64) -> Self {
        Self { seed, only: None }
    }

    /// Always the same script, ignoring the seed's choice of variant (its
    /// payload/tool selection still varies by seed). Used by properties that
    /// isolate one dimension (e.g. the lease-fencing test, which wants a
    /// plain mutation every turn, not an adversarial stream on top of it).
    pub fn fixed(seed: u64, script: StepScript) -> Self {
        Self {
            seed,
            only: Some(script),
        }
    }

    /// `fixed()`'s override applies only until history already shows a
    /// committed step that proposed a call, so a fixed "one mutation" model
    /// still lets the turn conclude afterward instead of proposing the same
    /// call forever and running the budget out. Gated on committed history
    /// rather than the step *number*: a crash can abandon an attempt (no
    /// `ModelStepCompleted`, so nothing enters history) and force a retry at
    /// a higher step number for what is still, from the model's point of
    /// view, its first real turn at bat.
    fn script_for(&self, ctx: &Context, key: u64) -> StepScript {
        let already_proposed = ctx.history().iter().any(|entry| {
            matches!(&entry.message, dex_loop::Message::Assistant { calls, .. } if !calls.is_empty())
        });
        match self.only {
            Some(script) if !already_proposed => script,
            Some(_) => StepScript::PlainAnswer,
            None => STEP_SCRIPTS[(key % STEP_SCRIPTS.len() as u64) as usize],
        }
    }
}

impl Model for SimModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let turn = ctx.turn().map(ToString::to_string).unwrap_or_default();
        // `ctx.step()` already reflects the step this call is for: the
        // engine appends and observes `StepStarted { step, .. }` before
        // calling `Model::stream` (see `Engine::model_step`), so this must
        // not add 1 again -- doing so put every real first step at "step 2"
        // here, so `fixed()`'s "only on step 1" override never fired.
        let step = ctx.step();
        let seed_s = self.seed.to_string();
        let step_s = step.to_string();
        let key = hash_of(&[&seed_s, &turn, &step_s]);
        let script = self.script_for(ctx, key);
        let read = tools
            .iter()
            .find(|spec| spec.read_only && spec.name.as_str() != "tools.search");
        let mutation = tools.iter().find(|spec| {
            !spec.read_only
                && spec.name.as_str() != dex_loop::CODEMODE
                && spec.executor != dex_loop::ExecutorKind::Client
        });
        let client = tools
            .iter()
            .find(|spec| spec.executor == dex_loop::ExecutorKind::Client);
        let text_reply = |text: String| vec![Ok(ModelChunk::Text(text))];
        let call = |spec: &&ToolSpec| {
            vec![Ok(ModelChunk::ToolCall {
                name: spec.name.clone(),
                args: serde_json::json!({"key": spec.name.as_str(), "seed": key}),
            })]
        };
        let script_chunks: Vec<Result<ModelChunk, ModelError>> = match script {
            StepScript::PlainAnswer => text_reply(format!("done ({key})")),
            StepScript::OneRead => read
                .map(call)
                .unwrap_or_else(|| text_reply("no read tool".into())),
            StepScript::OneMutation => mutation
                .map(call)
                .unwrap_or_else(|| text_reply("no mutation tool".into())),
            StepScript::OneClientTool => client
                .map(call)
                .unwrap_or_else(|| text_reply("no client tool".into())),
            StepScript::DuplicateCalls => match read.or(mutation) {
                Some(spec) => {
                    let mut chunks = call(spec);
                    chunks.extend(call(spec));
                    chunks
                }
                None => text_reply("no tool to duplicate".into()),
            },
            StepScript::UnknownTool => vec![Ok(ModelChunk::ToolCall {
                name: dex_loop::ToolName::new(format!("nonexistent.tool.{key}")),
                args: serde_json::json!({}),
            })],
            StepScript::Truncated => vec![Ok(ModelChunk::Text("truncated mid".into()))],
            StepScript::Abandoned => vec![
                Ok(ModelChunk::Text("about to fail".into())),
                Err(ModelError {
                    class: dex_loop::ErrorClass::Unknown,
                    message: format!("upstream reset ({key})"),
                }),
            ],
            StepScript::Stalled => Vec::new(),
            StepScript::AdversarialPayload => text_reply(adversarial_payload(key)),
        };
        // `Stalled` never yields and never ends -- a genuinely different
        // shape from "a short `script_chunks`", which properly ends. Boxing
        // is the simplest way to return either shape from one opaque
        // `impl Stream` return type.
        let stalled = matches!(script, StepScript::Stalled);
        let boxed: std::pin::Pin<
            Box<dyn Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a>,
        > = if stalled {
            Box::pin(stream::pending())
        } else {
            Box::pin(stream::iter(script_chunks))
        };
        boxed
    }
}

// --------------------------------------------------------------- SimTools

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeHint {
    Succeed,
    Fail,
    Unknown,
}

/// The tool registry: a fixed catalog (mirroring real hosts, whose registry
/// does not change tool-by-tool at runtime) plus a `vanished` set that hides
/// specific tools from `spec()` -- and therefore from dispatch -- without
/// touching `catalog()`, modeling a tool that a deploy or a grant revoke
/// removed mid-turn (the model was already offered it and proposed a call;
/// the offer is gone by the time the engine goes to run it).
#[derive(Clone)]
pub struct SimTools {
    catalog: Arc<[ToolSpec]>,
    vanished: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Tools policy denies regardless of governance, set by an action that
    /// simulates a grant revoke or a policy change while a call sits parked:
    /// `dispatch` always re-runs `Tools::policy` on resume, so a call already
    /// approved must still be denied once this is set.
    denied: Arc<Mutex<std::collections::HashSet<String>>>,
    seed: u64,
    crash: CrashBudget,
    dispatches: Arc<Mutex<Vec<CallId>>>,
}

impl SimTools {
    pub fn new(catalog: Vec<ToolSpec>, seed: u64, crash: CrashBudget) -> Self {
        Self {
            catalog: catalog.into(),
            vanished: Arc::default(),
            denied: Arc::default(),
            seed,
            crash,
            dispatches: Arc::default(),
        }
    }

    /// The same catalog, ledger and dispatch history, but a fresh crash
    /// budget: used to inject a crash into exactly one `Engine::run` attempt
    /// without disturbing anything else about the simulated tool registry.
    pub fn with_crash(&self, crash: CrashBudget) -> Self {
        Self {
            crash,
            ..self.clone()
        }
    }

    pub fn vanish(&self, name: &str) {
        lock(&self.vanished).insert(name.to_owned());
    }

    pub fn restore(&self, name: &str) {
        lock(&self.vanished).remove(name);
    }

    pub fn force_deny(&self, name: &str) {
        lock(&self.denied).insert(name.to_owned());
    }

    pub fn clear_deny(&self, name: &str) {
        lock(&self.denied).remove(name);
    }

    /// Every `CallId` this instance actually dispatched (`Tools::run`
    /// returned for it), in dispatch order. Used to check that a mutation is
    /// never dispatched twice.
    pub fn dispatches(&self) -> Vec<CallId> {
        lock(&self.dispatches).clone()
    }

    fn outcome_hint(&self, call: &CallId) -> OutcomeHint {
        let seed_s = self.seed.to_string();
        match hash_of(&[&seed_s, call.as_str()]) % 10 {
            0..=6 => OutcomeHint::Succeed,
            7..=8 => OutcomeHint::Fail,
            _ => OutcomeHint::Unknown,
        }
    }
}

impl Tools for SimTools {
    fn catalog(&self) -> &[ToolSpec] {
        &self.catalog
    }

    fn spec(&self, name: &ToolName) -> Option<&ToolSpec> {
        if lock(&self.vanished).contains(name.as_str()) {
            return None;
        }
        self.catalog.iter().find(|spec| &spec.name == name)
    }

    async fn search(&self, _principal: &PrincipalId, query: &str) -> Vec<ToolName> {
        self.catalog
            .iter()
            .filter(|spec| spec.name.as_str().contains(query) && self.spec(&spec.name).is_some())
            .map(|spec| spec.name.clone())
            .collect()
    }

    async fn policy(&self, _ctx: &Context, call: &ProposedCall) -> Verdict {
        if lock(&self.denied).contains(call.tool.as_str()) {
            return Verdict::Deny("policy changed while this call was pending".into());
        }
        match self.spec(&call.tool) {
            Some(spec) if spec.governance == dex_loop::GovernanceClass::Approval => {
                Verdict::NeedsApproval {
                    approval: dex_loop::ApprovalId::new(format!("approval-{}", call.id)),
                    summary: format!("Approve {}", spec.label),
                }
            }
            _ => Verdict::Allow,
        }
    }

    async fn run(
        &self,
        _thread: &ThreadId,
        call: &ProposedCall,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        // The dispatch itself is the irreversible side effect: record it
        // before the crash checkpoint, so a crash that lands *inside* this
        // call still shows up as "dispatched", exactly like a real mutation
        // whose network call went out before the process died.
        lock(&self.dispatches).push(call.id.clone());
        self.crash.checkpoint().await;
        match self.outcome_hint(&call.id) {
            OutcomeHint::Succeed => {
                ToolResult::stored(OutputRef::new(format!("out/{}", call.id)), None)
            }
            OutcomeHint::Fail => ToolResult::error(format!("tool {} failed", call.id)),
            OutcomeHint::Unknown => {
                ToolResult::unknown(format!("tool {} timed out; effect unknown", call.id))
            }
        }
    }
}
