//! The loop.
//!
//! Per step: `StepStarted` → model stream (text to the log as it arrives) →
//! `ModelStepCompleted` (the commit point: every proposed call, with full
//! arguments) → per call, in order: policy → (auto-approval receipt) →
//! `ToolStarted` → effect → `ToolFinished`. The turn ends when a step
//! proposes no calls. No call ever parks for a human: a `NeedsApproval`
//! verdict is granted at once and recorded as `AutoApproved`.
//!
//! One exception to "after the commit point": a read-only call whose
//! arguments validate and whose policy allows it starts while the model is
//! still streaming (`ToolStarted` before `ModelStepCompleted`, the same
//! way Codex starts a call when its output item closes). A read has no
//! effect to wait for, so an attempt that never commits simply finishes
//! those calls as not run; a committed step adopts their results in place
//! of running them again.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio_util::sync::CancellationToken;

use crate::budget::{Budget, BudgetAxis};
use crate::compaction::{Compactor, NoCompaction};
use crate::context::{CallState, Context, Decision, Status};
use crate::event::{
    AUTO_APPROVER, ApprovalId, CallId, Cursor, ErrorCode, Event, Outcome, PrincipalId,
    ProposedCall, ToolName, ToolResult, TurnId,
};
use crate::ports::{
    Claim, Effects, ExecutorKind, Fenced, GovernanceClass, Log, Model, ModelChunk, ModelError,
    ToolSpec, Tools, Verdict,
};
use crate::sanitize::{DeltaFilter, Sanitizer};

/// The engine-owned discovery tool. Always offered to the model.
pub const TOOLS_SEARCH: &str = "tools.search";

const NOT_RUN_INTERRUPTED: &str = "not run: the turn was interrupted";
const UNKNOWN_INTERRUPTED: &str =
    "outcome unknown: the turn was interrupted before the result was recorded";
/// A call that already started (its tool vanished from the catalog, or the
/// ledger still shows it running) must never be told "unknown tool" or shown
/// a `Running` outcome that nothing will update: both invite the model to
/// retry, which mints a new `CallId` and can run the mutation twice.
const UNKNOWN_NO_RETRY: &str = "outcome unknown: this call already started; do not retry it without first checking whether it took effect";
const UNCERTAIN_REPEAT: &str = "not run: the same operation has an unknown outcome in this turn; check whether it took effect before trying again";
const APPROVER_DECLINED: &str = "denied: the approver declined this call";
const APPROVAL_MISMATCH: &str = "denied: the approval does not match this call's arguments";
const MISSING_QUESTION: &str = "invalid call: args.question must be a non-empty string";
const MISSING_QUERY: &str = "invalid call: args.query must be a non-empty string";
const CLIENT_TIMED_OUT_READ: &str =
    "not completed: the client did not report a result in time; it is safe to try again";
const CLIENT_TIMED_OUT_MUTATION: &str =
    "outcome unknown: the client did not report a result in time; check before trying again";
/// A read started while the model streamed, whose attempt never committed
/// (the stream failed or was cut): it is finished so its `ToolStarted` does
/// not dangle; the next attempt proposes and runs it afresh.
const NOT_RUN_ATTEMPT_ABANDONED: &str = "not run: the model attempt was abandoned";

/// How long a call waits for `Event::ClientToolResult` before the engine
/// gives up on it. Matches `dex_tools::Timeouts::default().client`; a host
/// that wants a different wait calls `Engine::with_client_timeout`.
const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long one `Tools::run` may take before the engine stops waiting and
/// finishes the call itself: `Failed` for a read (safe to retry), `Unknown`
/// for a mutation (recorded in the ledger, so a resume adopts the same
/// answer). The engine's own outer bound, whatever the executor: a tool port
/// that never returns (a stranded remote dispatch, a hung in-process HTTP
/// call) used to hold `Engine::run` open indefinitely, because `budget.wall`
/// is only checked between steps. The deadline is also clamped to the wall
/// budget's remaining time, so a turn never outlives `budget.wall` by more
/// than the time it takes to append the result. A host that wants a
/// different bound calls `Engine::with_tool_call_deadline`.
pub const DEFAULT_TOOL_CALL_DEADLINE: Duration = Duration::from_secs(5 * 60);
const DEADLINE_READ: &str =
    "not completed: the call did not finish within Dex's time limit; it is safe to try again";
const DEADLINE_MUTATION: &str = "outcome unknown: the call did not finish within Dex's time limit; check whether it took effect before trying again";

/// Why `Engine::run` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The model answered without tool calls.
    Done,
    /// Legacy: waiting for `Event::ApprovalDecided`. The engine no longer
    /// returns it (no call parks for a human); hosts keep the arm so an
    /// older binary's exit still matches.
    Parked(ApprovalId),
    /// Waiting for `Event::Answer` to this call; call `run` again after it lands.
    Asked(CallId),
    /// Waiting for `Event::ClientToolResult` for this call; call `run` again
    /// after it lands.
    AwaitingClientTool(CallId),
    Interrupted,
    /// `Event::Error` was appended.
    Failed,
}

/// What `dispatch_client_tool` did with the call it was given.
enum ClientToolOutcome {
    /// Resolved without effect (denied, mismatched digest): the loop over
    /// calls continues.
    Continue,
    /// The pending wave was flushed and an interrupt arrived: the loop over
    /// calls stops early, same as the other `flush`-checking branches.
    Break,
    /// The turn parked: return this from `dispatch` at once.
    Exit(Exit),
}

/// The current client call and any decision recorded before this run.
struct ClientCall<'a> {
    proposal: &'a ProposedCall,
    decision: Option<Decision>,
}

/// One result of racing the model stream against `cancel`, the wall budget
/// and the reads started during the stream in `model_step`.
enum StreamStep {
    Chunk(Result<ModelChunk, ModelError>),
    /// The stream ended on its own (a truncated or otherwise finite stream).
    Ended,
    Cancelled,
    /// `budget.wall` elapsed while waiting for the next chunk: the stream
    /// itself never errored or ended, so nothing else would have caught this.
    WallExceeded,
    /// A read started during the stream returned.
    Prefetched(usize, ToolResult),
}

/// One read-only call running ahead of its step's commit point, by index
/// in the step's calls.
type PrefetchFuture<'e> = Pin<Box<dyn Future<Output = (usize, ToolResult)> + Send + 'e>>;

/// The reads that are running or finished ahead of their step's commit.
struct Reads<'e> {
    /// Still running. Each carries its own deadline, the one a wave would
    /// have given it.
    pending: FuturesUnordered<PrefetchFuture<'e>>,
    /// Returned before the step committed.
    done: HashMap<usize, ToolResult>,
}

/// The reads' shared handle. A read is only polled while the engine polls
/// it, and a read can be suspended inside a log write of its own (a client
/// request, a progress label) holding the thread's row lock. If the engine
/// then awaited a log write without polling the reads, the two would wait on
/// each other forever. [`ReadsHandle::drive`] polls the reads alongside any
/// engine await, so that never happens.
#[derive(Clone)]
struct ReadsHandle<'e>(Arc<Mutex<Reads<'e>>>);

impl<'e> ReadsHandle<'e> {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Reads {
            pending: FuturesUnordered::new(),
            done: HashMap::new(),
        })))
    }

    /// Never held across an await: every use is one short synchronous step.
    fn lock(&self) -> std::sync::MutexGuard<'_, Reads<'e>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, read: PrefetchFuture<'e>) {
        self.lock().pending.push(read);
    }

    fn has_pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    fn take_done(&self, index: usize) -> Option<ToolResult> {
        self.lock().done.remove(&index)
    }

    fn take_pending(&self) -> FuturesUnordered<PrefetchFuture<'e>> {
        std::mem::take(&mut self.lock().pending)
    }

    /// The next read to return, if any is running. Cancel-safe: dropping it
    /// loses nothing, the read stays in the set.
    async fn next_done(&self) -> Option<(usize, ToolResult)> {
        std::future::poll_fn(|cx| self.lock().pending.poll_next_unpin(cx)).await
    }

    /// Awaits `work` while polling the running reads, so a read suspended in
    /// a log write can finish it. Results that arrive meanwhile are kept in
    /// `done`.
    async fn drive<T>(&self, work: impl Future<Output = T>) -> T {
        let mut work = pin!(work);
        std::future::poll_fn(|cx| {
            if let Poll::Ready(output) = work.as_mut().poll(cx) {
                return Poll::Ready(output);
            }
            let mut reads = self.lock();
            while let Poll::Ready(Some((index, result))) = reads.pending.poll_next_unpin(cx) {
                reads.done.insert(index, result);
            }
            Poll::Pending
        })
        .await
    }
}

/// Reads started while the model streamed (see the module doc). Lives in
/// `Engine::run` for one step: filled by `model_step`, drained by
/// `dispatch`, and dropped whole when the attempt does not commit.
struct Prefetch<'e> {
    reads: ReadsHandle<'e>,
    /// Indices whose `ToolStarted` is already on the log.
    started: HashSet<usize>,
    /// The `ToolStarted` rows appended mid-stream. The model stream borrows
    /// `ctx` until it is dropped, so they are observed then, in log order.
    unobserved: Vec<(Cursor, Event)>,
}

impl Prefetch<'_> {
    fn new() -> Self {
        Self {
            reads: ReadsHandle::new(),
            started: HashSet::new(),
            unobserved: Vec::new(),
        }
    }
}

/// The agent loop. Holds the host's ports and no state of its own, so any
/// replica can run any thread from its log.
pub struct Engine<L, M, T, E, S, C = NoCompaction> {
    log: L,
    model: M,
    tools: T,
    effects: E,
    sanitizer: S,
    compactor: C,
    budget: Budget,
    search: ToolSpec,
    client_timeout: Duration,
    tool_call_deadline: Duration,
}

impl<L, M, T, E, S> Engine<L, M, T, E, S, NoCompaction>
where
    L: Log,
    M: Model,
    T: Tools,
    E: Effects,
    S: Sanitizer,
{
    pub fn new(log: L, model: M, tools: T, effects: E, sanitizer: S, budget: Budget) -> Self {
        Self {
            log,
            model,
            tools,
            effects,
            sanitizer,
            compactor: NoCompaction,
            budget,
            search: search_spec(),
            client_timeout: DEFAULT_CLIENT_TIMEOUT,
            tool_call_deadline: DEFAULT_TOOL_CALL_DEADLINE,
        }
    }
}

impl<L, M, T, E, S, C> Engine<L, M, T, E, S, C>
where
    L: Log,
    M: Model,
    T: Tools,
    E: Effects,
    S: Sanitizer,
    C: Compactor,
{
    pub fn with_compactor<C2: Compactor>(self, compactor: C2) -> Engine<L, M, T, E, S, C2> {
        Engine {
            log: self.log,
            model: self.model,
            tools: self.tools,
            effects: self.effects,
            sanitizer: self.sanitizer,
            compactor,
            budget: self.budget,
            search: self.search,
            client_timeout: self.client_timeout,
            tool_call_deadline: self.tool_call_deadline,
        }
    }

    /// How long a `Client`-executor call waits for `Event::ClientToolResult`
    /// before the engine gives up on it and finishes the call as timed out.
    /// Defaults to `dex_tools::Timeouts::default().client` (120s).
    pub fn with_client_timeout(mut self, client_timeout: Duration) -> Self {
        self.client_timeout = client_timeout;
        self
    }

    /// How long one `Tools::run` may take before the engine finishes the
    /// call as timed out (see [`DEFAULT_TOOL_CALL_DEADLINE`]).
    pub fn with_tool_call_deadline(mut self, tool_call_deadline: Duration) -> Self {
        self.tool_call_deadline = tool_call_deadline;
        self
    }

    /// The time one call started now may run: the per-call deadline, or the
    /// wall budget's remaining time if that is shorter.
    fn call_deadline(&self, run_started: Instant) -> Duration {
        self.tool_call_deadline
            .min(self.budget.wall.saturating_sub(run_started.elapsed()))
    }

    /// Runs the current turn from wherever `ctx` stands until it finishes,
    /// parks, or is interrupted. `ctx` is usually fresh from `rehydrate`; the
    /// same call resumes after a restart, an approval, or an answer.
    ///
    /// The host appends `Interrupt` and then cancels `cancel`. `Err(Fenced)`
    /// means a write was refused and nothing more was appended.
    pub async fn run<'e>(
        &'e self,
        ctx: &mut Context,
        cancel: &'e CancellationToken,
    ) -> Result<Exit, Fenced> {
        let started = Instant::now();
        let mut prefetch = Prefetch::new();
        loop {
            self.read_control(ctx).await?;
            match ctx.status() {
                Status::Idle | Status::Done => return Ok(Exit::Done),
                Status::Interrupted => return Ok(Exit::Interrupted),
                Status::Failed => return Ok(Exit::Failed),
                Status::Running => {}
            }
            if let Some(step) = ctx.open_attempt() {
                // A crash mid-stream: the attempt's text never committed, and
                // neither did the reads it had started; close those so no
                // `ToolStarted` stays open.
                let mut events: Vec<Event> = ctx
                    .pre_started_calls()
                    .iter()
                    .map(|call| {
                        let result = ToolResult::error(NOT_RUN_ATTEMPT_ABANDONED);
                        Event::ToolFinished {
                            call: call.clone(),
                            outcome: result.outcome,
                            output: result.output,
                            receipt: result.receipt,
                        }
                    })
                    .collect();
                events.push(Event::ModelAttemptAbandoned { step });
                self.emit(ctx, events).await?;
            }
            if ctx.interrupt_requested() || cancel.is_cancelled() {
                return self.interrupt(ctx).await;
            }
            if ctx.open_step().is_some() {
                let reads = prefetch.reads.clone();
                if let Some(exit) = reads
                    .drive(self.dispatch(ctx, cancel, started, &mut prefetch))
                    .await?
                {
                    return Ok(exit);
                }
                continue;
            }
            if let Some(axis) = self
                .budget
                .exhausted(ctx.step(), ctx.usage(), started.elapsed())
            {
                let message = self.budget_message(ctx, axis);
                self.emit(
                    ctx,
                    vec![Event::Error {
                        code: ErrorCode::BudgetExhausted,
                        message,
                    }],
                )
                .await?;
                return Ok(Exit::Failed);
            }
            if let Some(plan) = self.compactor.plan(ctx).await {
                self.emit(
                    ctx,
                    vec![Event::Compaction {
                        covers_to_cursor: plan.covers_to,
                        summary: plan.summary,
                    }],
                )
                .await?;
            }
            // A step never inherits another step's reads.
            prefetch = Prefetch::new();
            if let Some(exit) = self.model_step(ctx, cancel, started, &mut prefetch).await? {
                return Ok(exit);
            }
        }
    }

    async fn read_control(&self, ctx: &mut Context) -> Result<(), Fenced> {
        for (cursor, event) in self.log.control_since(ctx.control_cursor()).await? {
            ctx.observe(cursor, &event);
        }
        Ok(())
    }

    /// One model attempt, ending in `ModelStepCompleted` (and `Final` when
    /// the turn is done), or `ModelAttemptAbandoned` + `Error` on failure
    /// (including a stream that never completes: `budget.wall` bounds the
    /// whole `Engine::run` call, streaming included, not just the time
    /// between steps).
    async fn model_step<'e>(
        &'e self,
        ctx: &mut Context,
        cancel: &'e CancellationToken,
        started: Instant,
        prefetch: &mut Prefetch<'e>,
    ) -> Result<Option<Exit>, Fenced> {
        let step = ctx.step().saturating_add(1);
        self.emit(
            ctx,
            vec![Event::StepStarted {
                step,
                control_through: ctx.control_cursor(),
            }],
        )
        .await?;
        // Read after `StepStarted`: it places queued steers, whose authors
        // become the acting principal.
        let (Some(turn), Some(principal)) = (ctx.turn().cloned(), ctx.acting_principal().cloned())
        else {
            return Ok(Some(Exit::Done));
        };

        let mut filter = self.sanitizer.filter();
        let mut thinking = Thinking::new(self.sanitizer.filter());
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut failure = None;
        let mut wall_exceeded = false;
        // Usage arrives mid-stream but is never written on its own: it is
        // batched into whichever terminal event ends this attempt
        // (`ModelStepCompleted` or `ModelAttemptAbandoned`), so one attempt
        // costs one append instead of one per usage chunk plus one for the
        // terminal event. `Context::observe` sums `Usage` regardless of
        // position in the batch, so budget accounting is unaffected.
        let mut pending_usage = Vec::new();
        // The step's provider continuation state, committed with the step
        // (the model sends it only after a clean terminal). A second chunk
        // replaces the first: one step has one.
        let mut reasoning = None;
        // The route and model that served this attempt, kept for every
        // `ModelStepCompleted` below, including a cut-off or cancelled one.
        let mut served = None;
        {
            // The answer-only call offers nothing, not even `tools.search`.
            let answer_only = self.budget.answer_only(step.saturating_sub(1));
            let owned = if answer_only {
                Vec::new()
            } else {
                self.offered(ctx)
            };
            let offered: Vec<&ToolSpec> = owned.iter().collect();
            let mut stream = pin!(self.model.stream(ctx, &offered));
            // Dropping the stream on cancel is safe: a model call has no
            // effects to wait for. A stream that never yields another chunk
            // and is never cancelled would otherwise hang here forever, past
            // `budget.wall`: race every chunk against the wall deadline too,
            // not only against `cancel`.
            loop {
                let remaining = self.budget.wall.saturating_sub(started.elapsed());
                // `FuturesUnordered::next` on an empty set resolves at once
                // with `None`; the guard keeps it out of the race until a
                // read is actually running.
                let has_pending = prefetch.reads.has_pending();
                let outcome = tokio::select! {
                    biased;
                    () = cancel.cancelled() => StreamStep::Cancelled,
                    () = tokio::time::sleep(remaining) => StreamStep::WallExceeded,
                    item = stream.next() => match item {
                        Some(chunk) => StreamStep::Chunk(chunk),
                        None => StreamStep::Ended,
                    },
                    done = prefetch.reads.next_done(), if has_pending => match done {
                        Some((index, result)) => StreamStep::Prefetched(index, result),
                        None => continue,
                    },
                };
                match outcome {
                    StreamStep::Chunk(Ok(ModelChunk::Thinking(delta))) => {
                        if let Some(summary) = thinking.push(&delta, !text.is_empty()) {
                            let event = [Event::ThinkingDelta { text: summary }];
                            prefetch.reads.drive(self.log.append(&event)).await?;
                        }
                    }
                    StreamStep::Chunk(Ok(ModelChunk::Text(delta))) => {
                        if let Some(summary) = thinking.flush() {
                            let event = [Event::ThinkingDelta { text: summary }];
                            prefetch.reads.drive(self.log.append(&event)).await?;
                        }
                        let safe = filter.push(&delta);
                        if !safe.is_empty() {
                            text.push_str(&safe);
                            prefetch.reads.drive(self.log.append_text(safe)).await?;
                        }
                    }
                    StreamStep::Chunk(Ok(ModelChunk::ToolCall { name, args })) => {
                        let index = calls.len();
                        let id = call_id(&turn, step, index);
                        let call = ProposedCall::new(id, name, args, principal.clone());
                        // The answer-only call offers no tools, so nothing may
                        // start on it.
                        if !answer_only {
                            self.prefetch(ctx, &call, index, cancel, started, prefetch)
                                .await?;
                        }
                        calls.push(call);
                    }
                    StreamStep::Prefetched(index, result) => {
                        prefetch.reads.lock().done.insert(index, result);
                    }
                    StreamStep::Chunk(Ok(ModelChunk::Usage(usage))) => {
                        pending_usage.push(Event::Usage(usage));
                    }
                    StreamStep::Chunk(Ok(ModelChunk::Reasoning(state))) => {
                        reasoning = Some(state);
                    }
                    StreamStep::Chunk(Ok(ModelChunk::Served(by))) => {
                        served = Some(by);
                    }
                    StreamStep::Chunk(Err(error)) => {
                        failure = Some(error.message);
                        break;
                    }
                    StreamStep::Ended | StreamStep::Cancelled => break,
                    StreamStep::WallExceeded => {
                        wall_exceeded = true;
                        break;
                    }
                }
            }
            if failure.is_none() && !wall_exceeded {
                if let Some(summary) = thinking.flush() {
                    let event = [Event::ThinkingDelta { text: summary }];
                    prefetch.reads.drive(self.log.append(&event)).await?;
                }
                let tail = filter.finish();
                if !tail.is_empty() {
                    text.push_str(&tail);
                    prefetch.reads.drive(self.log.append_text(tail)).await?;
                }
            }
        }

        for (cursor, event) in std::mem::take(&mut prefetch.unobserved) {
            ctx.observe(cursor, &event);
        }
        // Every path below that does not commit `calls` first closes the
        // reads that already started, so no `ToolStarted` dangles.
        let committing = failure.is_none() && !wall_exceeded && !cancel.is_cancelled();
        if !committing {
            pending_usage.splice(0..0, Self::abandon_prefetch(&calls, prefetch));
        }
        if wall_exceeded {
            let message = self.budget_message(ctx, BudgetAxis::Wall);
            let mut events = pending_usage;
            events.push(Event::ModelAttemptAbandoned { step });
            events.push(Event::Error {
                code: ErrorCode::BudgetExhausted,
                message,
            });
            self.emit(ctx, events).await?;
            return Ok(Some(Exit::Failed));
        }
        if failure.is_some() && !text.is_empty() && !cancel.is_cancelled() {
            // The customer already read this text. Keep it as the
            // answer, visibly marked as cut off, instead of withdrawing
            // it: a long answer that loses its stream near the end (a
            // provider or gateway limit) must not vanish.
            let mut text = text;
            let tail = filter.finish();
            if !tail.is_empty() {
                text.push_str(&tail);
                self.log.append_text(tail).await?;
            }
            self.log.append_text(CUT_OFF_NOTICE.to_owned()).await?;
            text.push_str(CUT_OFF_NOTICE);
            let mut events = pending_usage;
            events.push(Event::ModelStepCompleted {
                step,
                text: text.clone(),
                calls: Vec::new(),
                reasoning: None,
                served: served.clone(),
            });
            events.push(Event::Final { text });
            self.emit(ctx, events).await?;
            return Ok(Some(Exit::Done));
        }
        if let Some(message) = failure {
            let mut events = pending_usage;
            events.push(Event::ModelAttemptAbandoned { step });
            events.push(Event::Error {
                code: ErrorCode::ModelFailed,
                message,
            });
            self.emit(ctx, events).await?;
            return Ok(Some(Exit::Failed));
        }
        if cancel.is_cancelled() {
            // Keep what the customer saw; calls from a cut stream never run,
            // so their continuation state is not kept either.
            let mut events = pending_usage;
            events.push(Event::ModelStepCompleted {
                step,
                text,
                calls: Vec::new(),
                reasoning: None,
                served: served.clone(),
            });
            self.emit(ctx, events).await?;
            return self.interrupt(ctx).await.map(Some);
        }
        if !calls.is_empty() && self.budget.answer_only(step.saturating_sub(1)) {
            // Asked for a tool on the call that offered none. Nothing can run
            // it, so the turn ends here instead of looping. (Nothing was
            // prefetched: the answer-only call offers no tools.)
            let message = self.budget_message(ctx, BudgetAxis::Steps);
            let mut events = pending_usage;
            events.push(Event::ModelAttemptAbandoned { step });
            events.push(Event::Error {
                code: ErrorCode::BudgetExhausted,
                message,
            });
            self.emit(ctx, events).await?;
            return Ok(Some(Exit::Failed));
        }
        if calls.is_empty() {
            // A steer that arrived during the answer continues the turn.
            self.read_control(ctx).await?;
            let continues = ctx.has_queued_steers() && !ctx.interrupt_requested();
            let mut events = pending_usage;
            events.push(Event::ModelStepCompleted {
                step,
                text: text.clone(),
                calls: Vec::new(),
                reasoning,
                served,
            });
            if !continues {
                events.push(Event::Final { text });
            }
            self.emit(ctx, events).await?;
            return Ok((!continues).then_some(Exit::Done));
        }
        let mut events = pending_usage;
        events.push(Event::ModelStepCompleted {
            step,
            text,
            calls,
            reasoning,
            served,
        });
        // The reads started during the stream are still running: keep
        // polling them through the commit (see `ReadsHandle`).
        prefetch.reads.drive(self.emit(ctx, events)).await?;
        Ok(None)
    }

    /// Starts `call` now, ahead of its step's commit, when it is a read the
    /// model was offered, its arguments fit the tool's schema, its policy
    /// allows it, and it runs on the host (not a person, not a client
    /// session). Anything else waits for `dispatch`, which re-derives the
    /// same verdicts and finishes an invalid or denied call there.
    async fn prefetch<'e>(
        &'e self,
        ctx: &Context,
        call: &ProposedCall,
        index: usize,
        cancel: &'e CancellationToken,
        run_started: Instant,
        prefetch: &mut Prefetch<'e>,
    ) -> Result<(), Fenced> {
        // Only an unbroken run of reads from the first call on starts early.
        // `dispatch` keeps the model's order around effects, so a read that
        // follows a mutation (or any call held back) must not observe the
        // world before that call has run.
        if prefetch.started.len() != index || call.tool.as_str() == TOOLS_SEARCH {
            return Ok(());
        }
        let Some(spec) = self.offered_spec(ctx, &call.tool) else {
            return Ok(());
        };
        let eligible = spec.read_only
            && !matches!(spec.executor, ExecutorKind::User | ExecutorKind::Client)
            && validate_args(&spec, &call.args).is_ok()
            && !ctx.has_uncertain_call(call);
        if !eligible || self.tools.policy(ctx, call).await != Verdict::Allow {
            return Ok(());
        }
        // Appended without `ctx.observe`: the stream still borrows `ctx`.
        let event = started(call, &spec);
        let cursors = prefetch
            .reads
            .drive(self.log.append(std::slice::from_ref(&event)))
            .await?;
        let [cursor] = cursors[..] else {
            return Err(Fenced::new(format!(
                "log returned {} cursors for 1 event",
                cursors.len()
            )));
        };
        prefetch.unobserved.push((cursor, event));
        prefetch.started.insert(index);
        let deadline = self.call_deadline(run_started);
        let thread = ctx.thread().clone();
        let call = call.clone();
        prefetch.reads.push(Box::pin(async move {
            let run = self.tools.run(&thread, &call, cancel);
            let result = match tokio::time::timeout(deadline, run).await {
                Ok(result) => result,
                Err(_elapsed) => ToolResult::error(DEADLINE_READ),
            };
            (index, result)
        }));
        Ok(())
    }

    /// The `ToolFinished` rows for reads that started under an attempt
    /// that will not commit, in call order; the reads themselves are
    /// dropped (a read has no effect to wait for). Leaves `prefetch` empty.
    fn abandon_prefetch(calls: &[ProposedCall], prefetch: &mut Prefetch<'_>) -> Vec<Event> {
        let events = calls
            .iter()
            .enumerate()
            .filter(|(index, _)| prefetch.started.contains(index))
            .map(|(_, call)| finished(call, ToolResult::error(NOT_RUN_ATTEMPT_ABANDONED)))
            .collect();
        *prefetch = Prefetch::new();
        events
    }

    /// Dispatches the open step's calls in the model's order. Allowed
    /// read-only calls collect into a wave that runs in parallel; anything
    /// else runs the pending wave first, so effects keep the model's order.
    /// Reads that already started during the stream (`prefetch`) are
    /// adopted by the wave instead of running again.
    async fn dispatch<'e>(
        &'e self,
        ctx: &mut Context,
        cancel: &'e CancellationToken,
        run_started: Instant,
        prefetch: &mut Prefetch<'e>,
    ) -> Result<Option<Exit>, Fenced> {
        let Some(step) = ctx.open_step() else {
            return Ok(None);
        };
        let calls = step.calls.clone();
        let mut wave: Vec<usize> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            // Interrupt stops at the next effect boundary.
            if cancel.is_cancelled() {
                break;
            }
            let state = ctx
                .open_step()
                .and_then(|step| step.states.get(index))
                .cloned();
            let decision = match state {
                None | Some(CallState::Done(_)) => continue,
                Some(CallState::Asked { answer: None }) => {
                    if self
                        .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                        .await?
                    {
                        break;
                    }
                    return Ok(Some(Exit::Asked(call.id.clone())));
                }
                Some(CallState::Asked {
                    answer: Some(answer),
                }) => {
                    self.finish(ctx, call, ToolResult::text(answer)).await?;
                    continue;
                }
                Some(CallState::AwaitingClient {
                    result: None,
                    deadline_ms,
                }) => {
                    if now_ms() >= deadline_ms {
                        self.timeout_client_call(ctx, call).await?;
                        continue;
                    }
                    if self
                        .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                        .await?
                    {
                        break;
                    }
                    return Ok(Some(Exit::AwaitingClientTool(call.id.clone())));
                }
                Some(CallState::AwaitingClient {
                    result: Some(result),
                    ..
                }) => {
                    self.finish_client_result(ctx, call, result).await?;
                    continue;
                }
                Some(CallState::Parked {
                    approval,
                    decision: None,
                }) => {
                    // A call an older deploy parked for a human and nobody
                    // decided. No human decides any more: grant it now,
                    // under the same approval id, so the thread resumes
                    // instead of waiting forever.
                    if self
                        .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                        .await?
                    {
                        break;
                    }
                    let summary = self
                        .offered_spec(ctx, &call.tool)
                        .map_or_else(|| call.tool.to_string(), |spec| spec.label);
                    Some(self.auto_approve(ctx, call, approval, summary).await?)
                }
                Some(CallState::Parked {
                    decision: Some(decision),
                    ..
                }) => Some(decision),
                Some(CallState::Started) => {
                    // Started before a restart and never finished. Reads run
                    // again; mutations resolve through the ledger, which
                    // never dispatches a claimed call a second time.
                    if call.tool.as_str() == TOOLS_SEARCH {
                        self.search_tools(ctx, call).await?;
                        continue;
                    }
                    match self.offered_spec(ctx, &call.tool) {
                        Some(spec) if spec.read_only => wave.push(index),
                        Some(_) => {
                            if self
                                .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                                .await?
                            {
                                break;
                            }
                            self.run_mutation(ctx, call, cancel, run_started).await?;
                        }
                        // The tool is no longer offered (deploy, grant
                        // revoke), but this call already started: it may
                        // have run. Resolve through the ledger instead of
                        // telling the model to retry with a different tool,
                        // which would dispatch a new call id for the same
                        // mutation.
                        None => self.resolve_started(ctx, call).await?,
                    }
                    continue;
                }
                Some(CallState::Todo) => None,
            };

            if call.tool.as_str() == TOOLS_SEARCH {
                self.search_tools(ctx, call).await?;
                continue;
            }
            let Some(spec) = self.offered_spec(ctx, &call.tool) else {
                self.finish(ctx, call, unknown_tool(&call.tool)).await?;
                continue;
            };
            // Arguments that do not fit the tool's schema never reach an
            // executor: the model sees why and can call again. (A call the
            // stream already started passed this check before it ran.)
            if let Err(reason) = validate_args(&spec, &call.args) {
                self.finish(ctx, call, ToolResult::error(reason)).await?;
                continue;
            }
            // Client-executor tools are not in `self.tools`'s catalog and
            // carry their own governance (set by the host's allowlist when
            // it resolved the client's declaration), so they skip
            // `Tools::policy`: that port models the registry's grants,
            // guardrails and guardian, none of which apply to a tool an
            // ephemeral client session declared for this turn only.
            if spec.executor == ExecutorKind::Client {
                match self
                    .dispatch_client_tool(
                        ctx,
                        &calls,
                        &mut wave,
                        cancel,
                        run_started,
                        prefetch,
                        ClientCall {
                            proposal: call,
                            decision,
                        },
                    )
                    .await?
                {
                    ClientToolOutcome::Continue => continue,
                    ClientToolOutcome::Break => break,
                    ClientToolOutcome::Exit(exit) => return Ok(Some(exit)),
                }
            }
            // Current policy first, even for a decided call: a revoked
            // grant or changed policy denies it.
            let verdict = match self.tools.policy(ctx, call).await {
                Verdict::Deny(reason) => {
                    self.finish(ctx, call, ToolResult::error(format!("denied: {reason}")))
                        .await?;
                    continue;
                }
                verdict => verdict,
            };
            if let Some(decision) = decision.as_ref() {
                if decision.args_digest != call.args_digest {
                    self.finish(ctx, call, ToolResult::error(APPROVAL_MISMATCH))
                        .await?;
                    continue;
                }
                if !decision.approved {
                    self.finish(ctx, call, ToolResult::error(APPROVER_DECLINED))
                        .await?;
                    continue;
                }
            }
            if self.refuse_uncertain_repeat(ctx, call, &spec).await? {
                continue;
            }
            // Policy asked for approval: no human is asked. The call is
            // granted at once and the receipt goes to the log before the
            // effect; the pending wave runs first so effects keep order.
            if let (None, Verdict::NeedsApproval { approval, summary }) = (decision, verdict) {
                if self
                    .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                    .await?
                {
                    break;
                }
                self.auto_approve(ctx, call, approval, summary).await?;
            }

            if spec.executor == ExecutorKind::User {
                if self
                    .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                    .await?
                {
                    break;
                }
                let Some(question) = non_empty_str(call, "question") else {
                    self.finish(ctx, call, ToolResult::error(MISSING_QUESTION))
                        .await?;
                    continue;
                };
                self.emit(
                    ctx,
                    vec![Event::Question {
                        call: call.id.clone(),
                        text: question.to_owned(),
                    }],
                )
                .await?;
                return Ok(Some(Exit::Asked(call.id.clone())));
            }
            if spec.read_only {
                wave.push(index);
            } else {
                if self
                    .flush(ctx, &calls, &mut wave, cancel, run_started, prefetch)
                    .await?
                {
                    break;
                }
                self.run_mutation(ctx, call, cancel, run_started).await?;
            }
        }
        self.run_wave(ctx, &calls, wave, cancel, run_started, prefetch)
            .await?;
        if cancel.is_cancelled() {
            return self.interrupt(ctx).await.map(Some);
        }
        Ok(None)
    }

    /// Refusal does not claim or dispatch a second effect. Reads remain safe
    /// to retry; an existing call ID still resolves through its effect ledger.
    async fn refuse_uncertain_repeat(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
        spec: &ToolSpec,
    ) -> Result<bool, Fenced> {
        if spec.read_only || !ctx.has_uncertain_call(call) {
            return Ok(false);
        }
        self.finish(ctx, call, ToolResult::error(UNCERTAIN_REPEAT))
            .await?;
        Ok(true)
    }

    /// One call to a `Client`-executor tool: approval (if the host's
    /// allowlist marked it a mutation), then `ClientToolRequested`, mirroring
    /// how the main `dispatch` loop handles `NeedsApproval` and `User`.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_client_tool<'e>(
        &'e self,
        ctx: &mut Context,
        calls: &[ProposedCall],
        wave: &mut Vec<usize>,
        cancel: &'e CancellationToken,
        run_started: Instant,
        prefetch: &mut Prefetch<'e>,
        client_call: ClientCall<'_>,
    ) -> Result<ClientToolOutcome, Fenced> {
        let ClientCall {
            proposal: call,
            decision,
        } = client_call;
        let Some(spec) = self.offered_spec(ctx, &call.tool) else {
            self.finish(ctx, call, unknown_tool(&call.tool)).await?;
            return Ok(ClientToolOutcome::Continue);
        };
        if let Some(decision) = decision.as_ref() {
            if decision.args_digest != call.args_digest {
                self.finish(ctx, call, ToolResult::error(APPROVAL_MISMATCH))
                    .await?;
                return Ok(ClientToolOutcome::Continue);
            }
            if !decision.approved {
                self.finish(ctx, call, ToolResult::error(APPROVER_DECLINED))
                    .await?;
                return Ok(ClientToolOutcome::Continue);
            }
        }
        if self.refuse_uncertain_repeat(ctx, call, &spec).await? {
            return Ok(ClientToolOutcome::Continue);
        }
        if decision.is_none() && spec.governance == GovernanceClass::Approval {
            if self
                .flush(ctx, calls, wave, cancel, run_started, prefetch)
                .await?
            {
                return Ok(ClientToolOutcome::Break);
            }
            let approval = ApprovalId::new(format!("client-{}", call.id));
            let summary = format!("Run {} in your browser", spec.label);
            self.auto_approve(ctx, call, approval, summary).await?;
        }
        if self
            .flush(ctx, calls, wave, cancel, run_started, prefetch)
            .await?
        {
            return Ok(ClientToolOutcome::Break);
        }
        let deadline_ms = now_ms().saturating_add(self.client_timeout_millis());
        self.emit(
            ctx,
            vec![Event::ClientToolRequested {
                call: call.id.clone(),
                tool: call.tool.clone(),
                args: call.args.clone(),
                label: spec.label.clone(),
                principal: call.principal.clone(),
                target_session: call.principal.to_string(),
                deadline_ms,
            }],
        )
        .await?;
        Ok(ClientToolOutcome::Exit(Exit::AwaitingClientTool(
            call.id.clone(),
        )))
    }

    /// Runs the pending wave before a call that must not overlap it. Returns
    /// true when an interrupt arrived meanwhile: the next effect must not
    /// start.
    async fn flush<'e>(
        &'e self,
        ctx: &mut Context,
        calls: &[ProposedCall],
        wave: &mut Vec<usize>,
        cancel: &'e CancellationToken,
        run_started: Instant,
        prefetch: &mut Prefetch<'e>,
    ) -> Result<bool, Fenced> {
        self.run_wave(
            ctx,
            calls,
            std::mem::take(wave),
            cancel,
            run_started,
            prefetch,
        )
        .await?;
        Ok(cancel.is_cancelled())
    }

    /// Runs read-only calls concurrently. Each `ToolFinished` is appended as
    /// its call returns; history receives the results in call order when the
    /// step closes. A call that already started during the stream keeps its
    /// `ToolStarted` and its run: a finished result is adopted at once and a
    /// pending one joins the wave.
    async fn run_wave<'e>(
        &'e self,
        ctx: &mut Context,
        calls: &[ProposedCall],
        wave: Vec<usize>,
        cancel: &'e CancellationToken,
        run_started: Instant,
        prefetch: &mut Prefetch<'e>,
    ) -> Result<(), Fenced> {
        if wave.is_empty() || cancel.is_cancelled() {
            return Ok(());
        }
        let starts: Vec<Event> = wave
            .iter()
            .filter(|index| !prefetch.started.contains(index))
            .filter_map(|&index| {
                let call = &calls[index];
                let spec = self.offered_spec(ctx, &call.tool)?;
                Some(started(call, &spec))
            })
            .collect();
        self.emit(ctx, starts).await?;
        let thread = ctx.thread().clone();
        // One deadline for the wave: its reads run concurrently, so each
        // gets the full time. A read that overruns is dropped and finished
        // `Failed`; a read has no effect to wait for, so retrying is safe.
        let deadline = self.call_deadline(run_started);
        let mut running: FuturesUnordered<PrefetchFuture<'e>> = FuturesUnordered::new();
        for &index in &wave {
            if let Some(result) = prefetch.reads.take_done(index) {
                self.finish(ctx, &calls[index], result).await?;
                continue;
            }
            if prefetch.started.contains(&index) {
                // Still running from the stream; it arrives through
                // `prefetch.pending` below.
                continue;
            }
            let call = calls[index].clone();
            let thread = thread.clone();
            running.push(Box::pin(async move {
                let run = self.tools.run(&thread, &call, cancel);
                let result = match tokio::time::timeout(deadline, run).await {
                    Ok(result) => result,
                    Err(_elapsed) => ToolResult::error(DEADLINE_READ),
                };
                (index, result)
            }));
        }
        for future in prefetch.reads.take_pending() {
            running.push(future);
        }
        // On `Fenced` the remaining reads are dropped: a stale owner must not
        // append, and the new owner runs them again.
        while let Some((index, result)) = running.next().await {
            let still_open = ctx
                .open_step()
                .and_then(|step| step.states.get(index))
                .is_some_and(|state| !matches!(state, CallState::Done(_)));
            // A prefetched read whose call `dispatch` already finished
            // (policy denied it on re-check) has nothing left to report.
            if !still_open || !(wave.contains(&index) || prefetch.started.contains(&index)) {
                continue;
            }
            self.finish(ctx, &calls[index], result).await?;
        }
        Ok(())
    }

    /// Claim, dispatch, record. A claimed call is never dispatched again:
    /// its recorded outcome is adopted instead, with `Running` settled to
    /// `Unknown` first — nothing ever revisits a `Running` report, so
    /// showing it as final would leave the model unable to tell whether to
    /// retry. Interrupt reaches a running mutation through `cancel`, but the
    /// engine still awaits the run and records its result: the tool decides
    /// what cancel means, and the mutation's future is never dropped.
    async fn run_mutation(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
        cancel: &CancellationToken,
        run_started: Instant,
    ) -> Result<(), Fenced> {
        let Some(spec) = self.offered_spec(ctx, &call.tool) else {
            // Unreachable from today's two call sites, which already
            // checked `offered_spec` before calling in; kept correct in
            // case a future caller skips that check. A vanished tool for a
            // call that already started must resolve through the ledger,
            // never as "unknown tool".
            return self.resolve_started(ctx, call).await;
        };
        match self.effects.claim(call).await? {
            Claim::Existing(result) => {
                let result = self.settle_claim(&call.id, result).await?;
                self.finish(ctx, call, result).await
            }
            Claim::Granted => {
                self.emit(ctx, vec![started(call, &spec)]).await?;
                // A mutation that overruns its deadline is dropped, not
                // cancelled: the effect may still land. `Unknown` is recorded
                // under the claim, so a later resume of this call adopts it
                // instead of dispatching the mutation a second time. An
                // interrupt only fires `cancel`; the run is still awaited.
                let run = self.tools.run(ctx.thread(), call, cancel);
                let result = match tokio::time::timeout(self.call_deadline(run_started), run).await
                {
                    Ok(result) => result,
                    Err(_elapsed) => ToolResult::unknown(DEADLINE_MUTATION),
                };
                self.effects.record(&call.id, &result).await?;
                self.finish(ctx, call, result).await
            }
        }
    }

    /// A `Started` call resolved without dispatching anything: its tool
    /// vanished from the offered catalog (deploy, grant revoke). Never
    /// "unknown tool" for a call that already started — the model would be
    /// told to retry, minting a new `CallId` for a mutation that may have
    /// already run. A prior ledger claim is settled the same way
    /// `run_mutation` settles one; a call the ledger never saw (it may have
    /// been a read) is claimed now and recorded `Unknown`, so a later resume
    /// gets the same answer instead of claiming it again.
    async fn resolve_started(&self, ctx: &mut Context, call: &ProposedCall) -> Result<(), Fenced> {
        let result = match self.effects.claim(call).await? {
            Claim::Existing(result) => self.settle_claim(&call.id, result).await?,
            Claim::Granted => {
                let result = ToolResult::unknown(UNKNOWN_NO_RETRY);
                self.effects.record(&call.id, &result).await?;
                result
            }
        };
        self.finish(ctx, call, result).await
    }

    /// Settles a claimed call's recorded outcome for the model: `Running`
    /// becomes `Unknown` (and the ledger is updated to match, so a later
    /// resume adopts the same answer); every other outcome passes through.
    async fn settle_claim(
        &self,
        call_id: &CallId,
        result: ToolResult,
    ) -> Result<ToolResult, Fenced> {
        if result.outcome != Outcome::Running {
            return Ok(result);
        }
        let result = ToolResult::unknown(UNKNOWN_NO_RETRY);
        self.effects.record(call_id, &result).await?;
        Ok(result)
    }

    /// `tools.search`: matched catalog tools are offered from the next step.
    async fn search_tools(&self, ctx: &mut Context, call: &ProposedCall) -> Result<(), Fenced> {
        self.emit(ctx, vec![started(call, &self.search)]).await?;
        let Some(query) = non_empty_str(call, "query") else {
            return self
                .finish(ctx, call, ToolResult::error(MISSING_QUERY))
                .await;
        };
        let matches: Vec<ToolSpec> = self
            .tools
            .search(&call.principal, query)
            .await
            .iter()
            .filter_map(|name| self.tools.spec(name))
            .cloned()
            .collect();
        let listing = if matches.is_empty() {
            format!("no tools match {query:?}")
        } else {
            matches
                .iter()
                .map(|spec| format!("{}: {}", spec.name, spec.label))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut events = Vec::with_capacity(2);
        if !matches.is_empty() {
            events.push(Event::ToolsExposed {
                call: call.id.clone(),
                tools: matches.into_iter().map(|spec| spec.name).collect(),
            });
        }
        events.push(finished(call, ToolResult::text(listing)));
        self.emit(ctx, events).await
    }

    /// Every call without a result gets one, then `Interrupted`.
    async fn interrupt(&self, ctx: &mut Context) -> Result<Exit, Fenced> {
        let mut events: Vec<Event> = ctx
            .open_step()
            .map(|step| {
                step.calls
                    .iter()
                    .zip(&step.states)
                    .filter_map(|(call, state)| match state {
                        CallState::Done(_) => None,
                        CallState::Started => {
                            Some(finished(call, ToolResult::unknown(UNKNOWN_INTERRUPTED)))
                        }
                        _ => Some(finished(call, ToolResult::error(NOT_RUN_INTERRUPTED))),
                    })
                    .collect()
            })
            .unwrap_or_default();
        events.push(Event::Interrupted);
        self.emit(ctx, events).await?;
        Ok(Exit::Interrupted)
    }

    /// The specs the model sees this step: `tools.search`, core tools, and
    /// tools exposed earlier in the turn. A turn's client-declared tools
    /// reach the model only if the host's `Tools::catalog()` already
    /// includes them (see `Context::client_tools`, and
    /// `dex_tools::client::declare` for dex-runtime's host); the engine
    /// does not merge them in itself, so they are never offered twice.
    fn offered(&self, ctx: &Context) -> Vec<ToolSpec> {
        std::iter::once(self.search.clone())
            .chain(
                self.tools
                    .catalog()
                    .iter()
                    .filter(|spec| spec.core || ctx.exposed_tools().contains(&spec.name))
                    .cloned(),
            )
            .collect()
    }

    /// A tool the model was offered. Calls to anything else are unknown.
    fn offered_spec(&self, ctx: &Context, name: &ToolName) -> Option<ToolSpec> {
        if name == &self.search.name {
            return Some(self.search.clone());
        }
        self.tools
            .spec(name)
            .filter(|spec| spec.core || ctx.exposed_tools().contains(name))
            .cloned()
    }

    fn budget_message(&self, ctx: &Context, axis: BudgetAxis) -> String {
        let budget = &self.budget;
        match axis {
            BudgetAxis::Steps => format!(
                "step budget exhausted: {} steps and the answer-only step after them",
                budget.max_steps
            ),
            BudgetAxis::Tokens => format!(
                "token budget exhausted: {} of {} tokens",
                ctx.usage().tokens(),
                budget.max_tokens
            ),
            BudgetAxis::Cost => format!(
                "cost budget exhausted: {} of {} micros",
                ctx.usage().cost_micros,
                budget.max_cost_micros
            ),
            BudgetAxis::Wall => format!("wall budget exhausted: {:?}", budget.wall),
        }
    }

    async fn finish(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
        result: ToolResult,
    ) -> Result<(), Fenced> {
        self.emit(ctx, vec![finished(call, result)]).await
    }

    fn client_timeout_millis(&self) -> i64 {
        i64::try_from(self.client_timeout.as_millis()).unwrap_or(i64::MAX)
    }

    /// An `AwaitingClient` call whose `Event::ClientToolResult` is already in
    /// the log: wraps `raw` through the host (untrusted-content marking,
    /// output storage) and, for a mutation, through the effect ledger --
    /// exactly the shaping a live dispatch of any other executor gets, and
    /// exactly once per call id, whether this is the first time the result
    /// is seen or a replay after a restart lands on the same state. Without
    /// this, a crash between `ClientToolResult` and `ToolFinished` would
    /// finish the call with the client's raw, unwrapped text.
    async fn finish_client_result(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
        raw: ToolResult,
    ) -> Result<(), Fenced> {
        match self.offered_spec(ctx, &call.tool) {
            Some(spec) if !spec.read_only => {
                let result = match self.effects.claim(call).await? {
                    Claim::Existing(existing) => self.settle_claim(&call.id, existing).await?,
                    Claim::Granted => {
                        let wrapped = self.tools.wrap_client_result(ctx.thread(), call, raw).await;
                        self.effects.record(&call.id, &wrapped).await?;
                        wrapped
                    }
                };
                self.finish(ctx, call, result).await
            }
            Some(_) => {
                let wrapped = self.tools.wrap_client_result(ctx.thread(), call, raw).await;
                self.finish(ctx, call, wrapped).await
            }
            // The tool vanished from the offered catalog (deploy, grant
            // revoke) between the request and the result: finish with what
            // the client reported rather than lose it to "unknown tool".
            None => self.finish(ctx, call, raw).await,
        }
    }

    /// An `AwaitingClient` call whose deadline has passed with no
    /// `Event::ClientToolResult` in the log: finishes it as timed out,
    /// through the same ledger guard as `finish_client_result` for a
    /// mutation, so a later replay that lands on the same expired deadline
    /// adopts the one recorded outcome instead of manufacturing (and
    /// ledgering) a second one.
    async fn timeout_client_call(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
    ) -> Result<(), Fenced> {
        match self.offered_spec(ctx, &call.tool) {
            Some(spec) if !spec.read_only => {
                let result = match self.effects.claim(call).await? {
                    Claim::Existing(existing) => self.settle_claim(&call.id, existing).await?,
                    Claim::Granted => {
                        let timed_out = ToolResult::unknown(CLIENT_TIMED_OUT_MUTATION);
                        self.effects.record(&call.id, &timed_out).await?;
                        timed_out
                    }
                };
                self.finish(ctx, call, result).await
            }
            _ => {
                self.finish(ctx, call, ToolResult::error(CLIENT_TIMED_OUT_READ))
                    .await
            }
        }
    }

    /// Grants a call policy said must ask, at once, and writes the receipt.
    /// No human is ever asked: the `AutoApproved` row (call, approval id,
    /// argument digest, summary, `AUTO_APPROVER`) is the durable record of
    /// what ran, and a replay adopts it instead of asking policy to grant
    /// again. Only reached for a `NeedsApproval` verdict or a legacy parked
    /// call; `Deny` was handled before this point and stays denied.
    async fn auto_approve(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
        approval: ApprovalId,
        summary: String,
    ) -> Result<Decision, Fenced> {
        self.emit(
            ctx,
            vec![Event::AutoApproved {
                call: call.id.clone(),
                approval,
                args_digest: call.args_digest.clone(),
                summary,
                principal: PrincipalId::new(AUTO_APPROVER),
            }],
        )
        .await?;
        Ok(Decision {
            approved: true,
            args_digest: call.args_digest.clone(),
        })
    }

    async fn emit(&self, ctx: &mut Context, events: Vec<Event>) -> Result<(), Fenced> {
        if events.is_empty() {
            return Ok(());
        }
        let cursors = self.log.append(&events).await?;
        if cursors.len() != events.len() {
            return Err(Fenced::new(format!(
                "log returned {} cursors for {} events",
                cursors.len(),
                events.len()
            )));
        }
        for (cursor, event) in cursors.into_iter().zip(&events) {
            ctx.observe(cursor, event);
        }
        Ok(())
    }
}

fn search_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(TOOLS_SEARCH),
        label: "Finding the right tools".into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string", "description": "What you need to do"}},
            "required": ["query"],
        }),
        read_only: true,
        core: true,
        governance: GovernanceClass::Plain,
        executor: ExecutorKind::InProcess,
    }
}

/// Most thinking summary one attempt shows; the rest is dropped.
const MAX_THINKING_BYTES: usize = 16 * 1024;
/// Held thinking is written once it reaches this size (the first piece is
/// written at once, so progress appears as soon as the model starts).
const THINKING_FLUSH_BYTES: usize = 240;

/// One attempt's thinking summary on its way to the log: sanitized like
/// answer text, written in bounded pieces, and never after the answer began.
struct Thinking<F> {
    filter: F,
    held: String,
    written: usize,
}

impl<F: DeltaFilter> Thinking<F> {
    fn new(filter: F) -> Self {
        Self {
            filter,
            held: String::new(),
            written: 0,
        }
    }

    /// Takes a thinking delta; returns a piece to write now, if any.
    fn push(&mut self, delta: &str, answering: bool) -> Option<String> {
        if answering || self.written + self.held.len() >= MAX_THINKING_BYTES {
            return None;
        }
        self.held.push_str(&self.filter.push(delta));
        if self.written == 0 || self.held.len() >= THINKING_FLUSH_BYTES {
            return self.flush();
        }
        None
    }

    /// Whatever is held, bounded, once.
    fn flush(&mut self) -> Option<String> {
        if self.held.is_empty() {
            return None;
        }
        let room = MAX_THINKING_BYTES.saturating_sub(self.written);
        let mut piece = std::mem::take(&mut self.held);
        if piece.len() > room {
            let mut end = room;
            while !piece.is_char_boundary(end) {
                end -= 1;
            }
            piece.truncate(end);
        }
        self.written += piece.len();
        (!piece.is_empty()).then_some(piece)
    }
}

/// Appended to an answer whose model stream failed after text was shown.
pub const CUT_OFF_NOTICE: &str =
    "\n\n_This answer was cut off before it finished. Ask me to continue from here._";

fn call_id(turn: &TurnId, step: u32, index: usize) -> CallId {
    CallId(format!("{turn}-{step}-{index}"))
}

/// Unix milliseconds, for comparing against `ClientToolRequested::deadline_ms`.
/// A clock that cannot read (`UNIX_EPOCH` in the future) or a duration wider
/// than `i64` counts as "now is very late": both saturate toward always
/// expiring a deadline rather than panicking or parking forever.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX)
}

fn non_empty_str<'a>(call: &'a ProposedCall, key: &str) -> Option<&'a str> {
    call.args
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// `args` against `spec.schema`. A schema that is absent, not an object, or
/// does not compile validates nothing (the executor still checks what it
/// needs); a schema violation names the first error so the model can call
/// again with arguments that fit.
fn validate_args(spec: &ToolSpec, args: &serde_json::Value) -> Result<(), String> {
    if !spec.schema.is_object() {
        return Ok(());
    }
    let Ok(validator) = jsonschema::validator_for(&spec.schema) else {
        return Ok(());
    };
    match validator.iter_errors(args).next() {
        None => Ok(()),
        Some(error) => Err(format!("invalid arguments: {error}")),
    }
}

fn unknown_tool(name: &ToolName) -> ToolResult {
    ToolResult::error(format!(
        "unknown tool: {name}; use {TOOLS_SEARCH} to find tools"
    ))
}

fn started(call: &ProposedCall, spec: &ToolSpec) -> Event {
    Event::ToolStarted {
        call: call.id.clone(),
        tool: call.tool.clone(),
        label: spec.label.clone(),
        principal: call.principal.clone(),
    }
}

fn finished(call: &ProposedCall, result: ToolResult) -> Event {
    Event::ToolFinished {
        call: call.id.clone(),
        outcome: result.outcome,
        output: result.output,
        receipt: result.receipt,
    }
}
