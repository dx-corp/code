//! The loop.
//!
//! Per step: `StepStarted` → model stream (text to the log as it arrives) →
//! `ModelStepCompleted` (the commit point: every proposed call, with full
//! arguments) → per call, in order: policy → (auto-approval receipt) →
//! `ToolStarted` → effect → `ToolFinished`. The turn ends when a step
//! proposes no calls. No call ever parks for a human: a `NeedsApproval`
//! verdict is granted at once and recorded as `AutoApproved`.

use std::pin::pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio_util::sync::CancellationToken;

use crate::budget::{Budget, BudgetAxis};
use crate::compaction::{Compactor, NoCompaction};
use crate::context::{CallState, Context, Decision, Status};
use crate::event::{
    AUTO_APPROVER, ApprovalId, CallId, ErrorCode, Event, Outcome, PrincipalId, ProposedCall,
    ToolName, ToolResult, TurnId,
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

/// One result of racing the model stream against `cancel` and the wall
/// budget in `model_step`.
enum StreamStep {
    Chunk(Result<ModelChunk, ModelError>),
    /// The stream ended on its own (a truncated or otherwise finite stream).
    Ended,
    Cancelled,
    /// `budget.wall` elapsed while waiting for the next chunk: the stream
    /// itself never errored or ended, so nothing else would have caught this.
    WallExceeded,
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
    pub async fn run(&self, ctx: &mut Context, cancel: &CancellationToken) -> Result<Exit, Fenced> {
        let started = Instant::now();
        loop {
            self.read_control(ctx).await?;
            match ctx.status() {
                Status::Idle | Status::Done => return Ok(Exit::Done),
                Status::Interrupted => return Ok(Exit::Interrupted),
                Status::Failed => return Ok(Exit::Failed),
                Status::Running => {}
            }
            if let Some(step) = ctx.open_attempt() {
                // A crash mid-stream: the attempt's text never committed.
                self.emit(ctx, vec![Event::ModelAttemptAbandoned { step }])
                    .await?;
            }
            if ctx.interrupt_requested() || cancel.is_cancelled() {
                return self.interrupt(ctx).await;
            }
            if ctx.open_step().is_some() {
                if let Some(exit) = self.dispatch(ctx, cancel, started).await? {
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
            if let Some(exit) = self.model_step(ctx, cancel, started).await? {
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
    async fn model_step(
        &self,
        ctx: &mut Context,
        cancel: &CancellationToken,
        started: Instant,
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
        {
            // The answer-only call offers nothing, not even `tools.search`.
            let owned = if self.budget.answer_only(step.saturating_sub(1)) {
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
                let outcome = tokio::select! {
                    biased;
                    () = cancel.cancelled() => StreamStep::Cancelled,
                    () = tokio::time::sleep(remaining) => StreamStep::WallExceeded,
                    item = stream.next() => match item {
                        Some(chunk) => StreamStep::Chunk(chunk),
                        None => StreamStep::Ended,
                    },
                };
                match outcome {
                    StreamStep::Chunk(Ok(ModelChunk::Text(delta))) => {
                        let safe = filter.push(&delta);
                        if !safe.is_empty() {
                            text.push_str(&safe);
                            self.log.append_text(safe).await?;
                        }
                    }
                    StreamStep::Chunk(Ok(ModelChunk::ToolCall { name, args })) => {
                        let id = call_id(&turn, step, calls.len());
                        calls.push(ProposedCall::new(id, name, args, principal.clone()));
                    }
                    StreamStep::Chunk(Ok(ModelChunk::Usage(usage))) => {
                        pending_usage.push(Event::Usage(usage));
                    }
                    StreamStep::Chunk(Ok(ModelChunk::Reasoning(state))) => {
                        reasoning = Some(state);
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
                let tail = filter.finish();
                if !tail.is_empty() {
                    text.push_str(&tail);
                    self.log.append_text(tail).await?;
                }
            }
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
            });
            self.emit(ctx, events).await?;
            return self.interrupt(ctx).await.map(Some);
        }
        if !calls.is_empty() && self.budget.answer_only(step.saturating_sub(1)) {
            // Asked for a tool on the call that offered none. Nothing can run
            // it, so the turn ends here instead of looping.
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
        });
        self.emit(ctx, events).await?;
        Ok(None)
    }

    /// Dispatches the open step's calls in the model's order. Allowed
    /// read-only calls collect into a wave that runs in parallel; anything
    /// else runs the pending wave first, so effects keep the model's order.
    async fn dispatch(
        &self,
        ctx: &mut Context,
        cancel: &CancellationToken,
        run_started: Instant,
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
                        .flush(ctx, &calls, &mut wave, cancel, run_started)
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
                        .flush(ctx, &calls, &mut wave, cancel, run_started)
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
                        .flush(ctx, &calls, &mut wave, cancel, run_started)
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
                                .flush(ctx, &calls, &mut wave, cancel, run_started)
                                .await?
                            {
                                break;
                            }
                            self.run_mutation(ctx, call, run_started).await?;
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
                    .flush(ctx, &calls, &mut wave, cancel, run_started)
                    .await?
                {
                    break;
                }
                self.auto_approve(ctx, call, approval, summary).await?;
            }

            if spec.executor == ExecutorKind::User {
                if self
                    .flush(ctx, &calls, &mut wave, cancel, run_started)
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
                    .flush(ctx, &calls, &mut wave, cancel, run_started)
                    .await?
                {
                    break;
                }
                self.run_mutation(ctx, call, run_started).await?;
            }
        }
        self.run_wave(ctx, &calls, wave, cancel, run_started)
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
    async fn dispatch_client_tool(
        &self,
        ctx: &mut Context,
        calls: &[ProposedCall],
        wave: &mut Vec<usize>,
        cancel: &CancellationToken,
        run_started: Instant,
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
            if self.flush(ctx, calls, wave, cancel, run_started).await? {
                return Ok(ClientToolOutcome::Break);
            }
            let approval = ApprovalId::new(format!("client-{}", call.id));
            let summary = format!("Run {} in your browser", spec.label);
            self.auto_approve(ctx, call, approval, summary).await?;
        }
        if self.flush(ctx, calls, wave, cancel, run_started).await? {
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
    async fn flush(
        &self,
        ctx: &mut Context,
        calls: &[ProposedCall],
        wave: &mut Vec<usize>,
        cancel: &CancellationToken,
        run_started: Instant,
    ) -> Result<bool, Fenced> {
        self.run_wave(ctx, calls, std::mem::take(wave), cancel, run_started)
            .await?;
        Ok(cancel.is_cancelled())
    }

    /// Runs read-only calls concurrently. Each `ToolFinished` is appended as
    /// its call returns; history receives the results in call order when the
    /// step closes.
    async fn run_wave(
        &self,
        ctx: &mut Context,
        calls: &[ProposedCall],
        wave: Vec<usize>,
        cancel: &CancellationToken,
        run_started: Instant,
    ) -> Result<(), Fenced> {
        if wave.is_empty() || cancel.is_cancelled() {
            return Ok(());
        }
        let starts: Vec<Event> = wave
            .iter()
            .filter_map(|&index| {
                let call = &calls[index];
                let spec = self.offered_spec(ctx, &call.tool)?;
                Some(started(call, &spec))
            })
            .collect();
        self.emit(ctx, starts).await?;
        let thread = ctx.thread().clone();
        let thread = &thread;
        // One deadline for the wave: its reads run concurrently, so each
        // gets the full time. A read that overruns is dropped and finished
        // `Failed`; a read has no effect to wait for, so retrying is safe.
        let deadline = self.call_deadline(run_started);
        let mut running: FuturesUnordered<_> = wave
            .iter()
            .map(|&index| {
                let call = &calls[index];
                async move {
                    let run = self.tools.run(thread, call, cancel);
                    let result = match tokio::time::timeout(deadline, run).await {
                        Ok(result) => result,
                        Err(_elapsed) => ToolResult::error(DEADLINE_READ),
                    };
                    (call, result)
                }
            })
            .collect();
        // On `Fenced` the remaining reads are dropped: a stale owner must not
        // append, and the new owner runs them again.
        while let Some((call, result)) = running.next().await {
            self.finish(ctx, call, result).await?;
        }
        Ok(())
    }

    /// Claim, dispatch, record. A claimed call is never dispatched again:
    /// its recorded outcome is adopted instead, with `Running` settled to
    /// `Unknown` first — nothing ever revisits a `Running` report, so
    /// showing it as final would leave the model unable to tell whether to
    /// retry. Interrupt does not cancel a mutation that has started.
    async fn run_mutation(
        &self,
        ctx: &mut Context,
        call: &ProposedCall,
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
                let never = CancellationToken::new();
                // A mutation that overruns its deadline is dropped, not
                // cancelled: the effect may still land. `Unknown` is recorded
                // under the claim, so a later resume of this call adopts it
                // instead of dispatching the mutation a second time.
                let run = self.tools.run(ctx.thread(), call, &never);
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
