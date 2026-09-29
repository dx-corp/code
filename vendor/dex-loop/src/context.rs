//! The in-memory state of one thread, derived from its log.
//!
//! `Context::observe` is the only state transition. The engine calls it for
//! every event it appends and every control event it reads; `rehydrate` calls
//! it for every event in the log. The same events therefore always produce
//! the same context, whether the actor stayed warm or restarted.

use crate::event::{
    ApprovalId, ApprovalMode, ArtifactRef, CallId, ClientToolSpec, Cursor, Event, MessageId,
    Outcome, Output, PrincipalId, ProposedCall, ProviderReasoning, ThreadId, ToolName, ToolResult,
    TurnId, Usage,
};

const NOT_RUN_NEW_TURN: &str = "not run: a new turn started first";
const UNKNOWN_NEW_TURN: &str = "outcome unknown: a new turn started before the result was recorded";

/// One message in the model's view of the thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    User {
        turn: TurnId,
        message_id: Option<MessageId>,
        principal: PrincipalId,
        text: String,
        attachments: Vec<ArtifactRef>,
    },
    Assistant {
        text: String,
        calls: Vec<ProposedCall>,
        /// The step's provider continuation state, as the `Model` port
        /// wrote it; `None` for steps logged without one.
        reasoning: Option<ProviderReasoning>,
    },
    Tool {
        call: CallId,
        name: ToolName,
        outcome: Outcome,
        output: Output,
    },
    /// A compaction summary of everything before it.
    Summary { text: String },
}

impl Message {
    /// A rough size in bytes, for compaction thresholds.
    pub fn size(&self) -> usize {
        match self {
            Message::User { text, .. } | Message::Summary { text } => text.len(),
            Message::Assistant { text, calls, .. } => {
                text.len()
                    + calls
                        .iter()
                        .map(|call| call.tool.as_str().len() + call.args.to_string().len())
                        .sum::<usize>()
            }
            Message::Tool { output, .. } => match output {
                Output::Text(text) => text.len(),
                Output::Ref(reference) => reference.as_str().len(),
            },
        }
    }
}

/// A history message and the cursor of the event that placed it. Cursors are
/// non-decreasing along the history.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub cursor: Cursor,
    pub message: Message,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Running,
    Done,
    Interrupted,
    Failed,
}

/// Where one call of the open step stands.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CallState {
    /// Not yet dispatched.
    Todo,
    /// `ToolStarted` is in the log; no `ToolFinished` yet.
    Started,
    Parked {
        approval: ApprovalId,
        decision: Option<Decision>,
    },
    Asked {
        answer: Option<String>,
    },
    /// `ClientToolRequested` is in the log; no `ClientToolResult` yet.
    AwaitingClient {
        result: Option<ToolResult>,
        /// Unix milliseconds after which the engine gives up on the wait,
        /// from `ClientToolRequested::deadline_ms`. Carried on the state
        /// itself, not read fresh from the log, so it is exactly the value
        /// the call was originally requested with, on a warm engine and on
        /// every rehydrate alike.
        deadline_ms: i64,
    },
    Done(ToolResult),
}

/// A recorded approval decision, checked again on resume.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Decision {
    pub(crate) approved: bool,
    pub(crate) args_digest: String,
}

/// The calls of the last `ModelStepCompleted` until every one has a result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenStep {
    pub(crate) calls: Vec<ProposedCall>,
    pub(crate) states: Vec<CallState>,
}

/// Everything the engine knows about a thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    thread: ThreadId,
    turn: Option<TurnId>,
    acting: Option<PrincipalId>,
    status: Status,
    history: Vec<Entry>,
    step: u32,
    usage: Usage,
    cursor: Cursor,
    control: Cursor,
    steers: Vec<(Cursor, PrincipalId, String)>,
    interrupt_requested: bool,
    attempt: Option<u32>,
    open_step: Option<OpenStep>,
    exposed: Vec<ToolName>,
    client_tools: Vec<ClientToolSpec>,
    authorized_tools: Vec<ToolName>,
    approval_mode: ApprovalMode,
    /// `UserMessage`s for a later turn that arrived (by log cursor) while an
    /// earlier turn was still running. FIFO: applied one at a time, each when
    /// the turn ahead of it reaches a terminal status, so the still-running
    /// turn's own later events (its `ModelStepCompleted`, `ToolFinished`,
    /// `Final`) are never attributed to the turn that raced in ahead of them.
    pending_turns: Vec<PendingTurn>,
    authorized_principal: Option<PrincipalId>,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingTurn {
    turn: TurnId,
    message_id: Option<MessageId>,
    principal: PrincipalId,
    text: String,
    attachments: Vec<ArtifactRef>,
    /// Resolved when this turn actually starts, so a message that raced in
    /// does not replace the still-running turn's client tools.
    client_tools: Vec<ClientToolSpec>,
    authorized_tools: Vec<ToolName>,
    approval_mode: ApprovalMode,
}

impl Context {
    /// An empty thread.
    pub fn new(thread: ThreadId) -> Self {
        Self {
            thread,
            turn: None,
            acting: None,
            status: Status::Idle,
            history: Vec::new(),
            step: 0,
            usage: Usage::default(),
            cursor: Cursor::START,
            control: Cursor::START,
            steers: Vec::new(),
            interrupt_requested: false,
            attempt: None,
            open_step: None,
            exposed: Vec::new(),
            client_tools: Vec::new(),
            authorized_tools: Vec::new(),
            approval_mode: ApprovalMode::Interactive,
            authorized_principal: None,
            pending_turns: Vec::new(),
        }
    }

    pub fn thread(&self) -> &ThreadId {
        &self.thread
    }

    /// The current (or last) turn.
    pub fn turn(&self) -> Option<&TurnId> {
        self.turn.as_ref()
    }

    /// The author of the most recent user input in history. Calls proposed
    /// by the next model step act under this principal.
    pub fn acting_principal(&self) -> Option<&PrincipalId> {
        self.acting.as_ref()
    }

    /// Tools `tools.search` exposed in this turn.
    pub fn exposed_tools(&self) -> &[ToolName] {
        &self.exposed
    }

    /// Tools this turn's client session declared on `Send`, exactly as
    /// logged (the client's own claims, unresolved). The host turns these
    /// into catalog entries per turn (see `dex_tools::client::declare`),
    /// tagged to the declaring session: the engine never offers them
    /// directly, so a host that ignores this returns to offering nothing
    /// client-declared, never a stale or duplicated set.
    pub fn client_tools(&self) -> &[ClientToolSpec] {
        &self.client_tools
    }

    /// The original admitted principal, unchanged by steering.
    pub fn authorized_principal(&self) -> Option<&PrincipalId> {
        self.authorized_principal.as_ref()
    }

    pub fn authorized_tools(&self) -> &[ToolName] {
        &self.authorized_tools
    }

    /// Who can answer this turn's approvals, as logged on its `UserMessage`.
    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode
    }

    /// The model's view of the thread.
    pub fn history(&self) -> &[Entry] {
        &self.history
    }

    /// Model calls started in the current turn.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// Model spend in the current turn.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The last event observed.
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// The last control event observed; `Log::control_since` reads after it.
    pub fn control_cursor(&self) -> Cursor {
        self.control
    }

    /// Raises the floor `control_cursor()` can never fall below, regardless
    /// of which control events this `Context` actually observes.
    ///
    /// `rehydrate` calls this with the first cursor of the log suffix it
    /// replays, before observing anything: a suffix chosen to start after
    /// some already-accounted-for point (a compaction boundary, most
    /// commonly) can easily contain zero control-kind events even though
    /// real ones exist earlier in the full log. Without a floor,
    /// `control_cursor()` would default to `Cursor::START` in that case, and
    /// the engine's first `Log::control_since(control_cursor())` call would
    /// re-fetch every control event the suffix deliberately left out --
    /// including an old, already-resolved `Interrupt` for a turn that ended
    /// before this one even started, observed again with the *new* turn
    /// `Running`, which interrupts it. The kernel contract this enforces:
    /// no control event with a cursor before the rehydrate point is ever
    /// re-applied, whether directly (during this replay) or indirectly
    /// (fetched again afterward because this replay under-reported where it
    /// left off).
    pub(crate) fn advance_control_floor(&mut self, floor: Cursor) {
        self.control = self.control.max(floor);
    }

    /// Puts the control cursor back after the engine appended a control event
    /// of its own, so a control event from another writer that landed just
    /// before it is still read.
    pub(crate) fn rewind_control(&mut self, to: Cursor) {
        self.control = to;
    }

    pub(crate) fn status(&self) -> Status {
        self.status
    }

    pub(crate) fn interrupt_requested(&self) -> bool {
        self.interrupt_requested
    }

    pub(crate) fn has_queued_steers(&self) -> bool {
        !self.steers.is_empty()
    }

    pub(crate) fn open_step(&self) -> Option<&OpenStep> {
        self.open_step.as_ref()
    }

    /// The step of a model attempt that started but never completed.
    pub(crate) fn open_attempt(&self) -> Option<u32> {
        self.attempt
    }

    /// Applies one log event. Events must be observed in log order.
    pub fn observe(&mut self, cursor: Cursor, event: &Event) {
        self.cursor = self.cursor.max(cursor);
        if event.is_control() {
            self.control = self.control.max(cursor);
        }
        match event {
            Event::UserMessage {
                turn,
                message_id,
                principal,
                text,
                attachments,
                client_tools,
                authorized_tools,
                approval_mode,
            } => {
                let next = PendingTurn {
                    turn: turn.clone(),
                    message_id: message_id.clone(),
                    principal: principal.clone(),
                    text: text.clone(),
                    attachments: attachments.clone(),
                    client_tools: client_tools.clone(),
                    authorized_tools: authorized_tools.clone(),
                    approval_mode: *approval_mode,
                };
                if self.status == Status::Running {
                    // This message's cursor landed while the current turn was
                    // still in flight (it was sent before that turn ended).
                    // The engine that ran the current turn never read it
                    // mid-flight, and the current turn's own later events
                    // (still to come, at cursors after this one) belong to
                    // the turn already running, not to this one. Queue it and
                    // start it once the running turn reaches a terminal
                    // status, so the log's cursor order can never misattach
                    // one turn's events to another.
                    self.pending_turns.push(next);
                } else {
                    self.begin_turn(cursor, next);
                }
            }
            Event::Steer { principal, text } => {
                // Only queue a steer once this replay has actually observed
                // a turn start. A `Steer` for a turn whose own `UserMessage`
                // is before the rehydrate point (excluded from `events`,
                // most often by a compaction boundary that lands between
                // that turn's start and one of its own not-yet-flushed
                // steers) has no turn in this replay to belong to; queuing
                // it anyway would only surface later, at the *next*
                // `begin_turn`'s unconditional flush, as a stray message
                // misattributed to whatever turn happens to start next --
                // possibly one with nothing to do with the principal who
                // sent it. `self.turn` is exactly this replay's marker of
                // "a turn has started here": `begin_turn` is the only place
                // that sets it, and a steer legitimately queued mid-turn
                // always arrives after its own turn's `UserMessage`.
                if self.turn.is_some() {
                    self.steers.push((cursor, principal.clone(), text.clone()));
                }
            }
            Event::Interrupt { .. } => {
                if self.status == Status::Running {
                    self.interrupt_requested = true;
                }
            }
            Event::ApprovalDecided {
                call,
                approval,
                args_digest,
                approved,
                ..
            } => {
                if let Some(CallState::Parked {
                    approval: parked,
                    decision,
                }) = self.state_mut(call)
                    && parked == approval
                    && decision.is_none()
                {
                    *decision = Some(Decision {
                        approved: *approved,
                        args_digest: args_digest.clone(),
                    });
                }
            }
            Event::Answer { call, text, .. } => {
                if let Some(CallState::Asked { answer }) = self.state_mut(call)
                    && answer.is_none()
                {
                    *answer = Some(text.clone());
                }
            }
            Event::ClientToolResult {
                call,
                outcome,
                output,
                ..
            } => {
                if let Some(CallState::AwaitingClient { result, .. }) = self.state_mut(call)
                    && result.is_none()
                {
                    *result = Some(ToolResult {
                        outcome: *outcome,
                        output: Output::Text(output.clone()),
                        receipt: None,
                    });
                }
            }
            Event::StepStarted {
                step,
                control_through,
            } => {
                self.step = *step;
                self.attempt = Some(*step);
                self.flush_steers(cursor, *control_through);
            }
            // Text reaches history through `ModelStepCompleted`.
            Event::TextDelta { .. } | Event::ToolProgress { .. } => {}
            Event::Usage(usage) => self.usage += *usage,
            Event::ModelStepCompleted {
                text,
                calls,
                reasoning,
                ..
            } => {
                self.attempt = None;
                self.push(
                    cursor,
                    Message::Assistant {
                        text: text.clone(),
                        calls: calls.clone(),
                        reasoning: reasoning.clone(),
                    },
                );
                if !calls.is_empty() {
                    self.open_step = Some(OpenStep {
                        calls: calls.clone(),
                        states: vec![CallState::Todo; calls.len()],
                    });
                }
            }
            Event::ModelAttemptAbandoned { .. } => self.attempt = None,
            Event::ToolStarted { call, .. } => {
                if let Some(state) = self.state_mut(call)
                    && !matches!(state, CallState::Done(_))
                {
                    *state = CallState::Started;
                }
            }
            Event::ToolsExposed { tools, .. } => {
                for tool in tools {
                    if !self.exposed.contains(tool) {
                        self.exposed.push(tool.clone());
                    }
                }
            }
            Event::ToolFinished {
                call,
                outcome,
                output,
                receipt,
            } => {
                if let Some(state) = self.state_mut(call) {
                    *state = CallState::Done(ToolResult {
                        outcome: *outcome,
                        output: output.clone(),
                        receipt: receipt.clone(),
                    });
                }
                self.close_step_if_resolved(cursor);
            }
            Event::ApprovalRequested { call, approval, .. } => {
                if let Some(state) = self.state_mut(call) {
                    *state = CallState::Parked {
                        approval: approval.clone(),
                        decision: None,
                    };
                }
            }
            Event::Question { call, .. } => {
                if let Some(state) = self.state_mut(call) {
                    *state = CallState::Asked { answer: None };
                }
            }
            Event::ClientToolRequested {
                call, deadline_ms, ..
            } => {
                if let Some(state) = self.state_mut(call) {
                    *state = CallState::AwaitingClient {
                        result: None,
                        deadline_ms: *deadline_ms,
                    };
                }
            }
            Event::Compaction {
                covers_to_cursor,
                summary,
            } => {
                self.history
                    .retain(|entry| entry.cursor > *covers_to_cursor);
                self.history.insert(
                    0,
                    Entry {
                        cursor: *covers_to_cursor,
                        message: Message::Summary {
                            text: summary.clone(),
                        },
                    },
                );
            }
            Event::Final { .. } => {
                self.status = Status::Done;
                self.begin_next_pending_turn(cursor);
            }
            Event::Error { .. } => {
                self.attempt = None;
                self.status = Status::Failed;
                self.begin_next_pending_turn(cursor);
            }
            Event::Interrupted => {
                self.attempt = None;
                self.status = Status::Interrupted;
                self.interrupt_requested = false;
                self.begin_next_pending_turn(cursor);
            }
        }
    }

    /// The common `UserMessage` transition: applied immediately when no turn
    /// is running, or deferred through `pending_turns` and applied here once
    /// the turn ahead of it ends.
    fn begin_turn(&mut self, cursor: Cursor, next: PendingTurn) {
        self.close_abandoned_step(cursor);
        // Flush before overwriting `self.turn`: any steer still queued here
        // is a straggler of the turn that was running (never picked up by
        // that turn's own `StepStarted` flush before it ended), so it is
        // attributed to that turn, not the one about to start.
        self.flush_steers(cursor, Cursor(i64::MAX));
        let PendingTurn {
            turn,
            message_id,
            principal,
            text,
            attachments,
            client_tools,
            authorized_tools,
            approval_mode,
        } = next;
        self.turn = Some(turn.clone());
        self.acting = Some(principal.clone());
        self.status = Status::Running;
        self.step = 0;
        self.usage = Usage::default();
        self.attempt = None;
        self.interrupt_requested = false;
        self.exposed.clear();
        self.client_tools = client_tools;
        self.authorized_tools = authorized_tools;
        self.approval_mode = approval_mode;
        self.authorized_principal = Some(principal.clone());
        self.push(
            cursor,
            Message::User {
                turn,
                message_id,
                principal,
                text,
                attachments,
            },
        );
    }

    /// Starts the oldest queued turn, if any, at `cursor`: the cursor of the
    /// terminal event (`Final`, `Error` or `Interrupted`) that just ended the
    /// turn ahead of it.
    fn begin_next_pending_turn(&mut self, cursor: Cursor) {
        if self.pending_turns.is_empty() {
            return;
        }
        let next = self.pending_turns.remove(0);
        self.begin_turn(cursor, next);
    }

    fn push(&mut self, cursor: Cursor, message: Message) {
        self.history.push(Entry { cursor, message });
    }

    fn state_mut(&mut self, call: &CallId) -> Option<&mut CallState> {
        let step = self.open_step.as_mut()?;
        let index = step.calls.iter().position(|c| &c.id == call)?;
        step.states.get_mut(index)
    }

    /// Steers the engine had read by `through` become user messages, placed
    /// at `cursor`. A steer carries no attachments of its own and no source
    /// message id: it is control text, not a `UserMessage` the host
    /// constructed from an inbound message.
    fn flush_steers(&mut self, cursor: Cursor, through: Cursor) {
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.steers)
            .into_iter()
            .partition(|(at, _, _)| *at <= through);
        self.steers = waiting;
        if ready.is_empty() {
            return;
        }
        // A `Steer` is only ever queued into `self.steers` while a turn is
        // already `self.turn` (see `Event::Steer` in `observe`), so this is
        // always `Some` by the time anything reaches `ready`; the fallback
        // exists only so this can never panic if that invariant is ever
        // violated.
        let turn = self
            .turn
            .clone()
            .unwrap_or_else(|| TurnId::new(String::new()));
        for (_, principal, text) in ready {
            self.acting = Some(principal.clone());
            self.push(
                cursor,
                Message::User {
                    turn: turn.clone(),
                    message_id: None,
                    principal,
                    text,
                    attachments: Vec::new(),
                },
            );
        }
    }

    /// When every call of the open step has a result, the results enter
    /// history in the model's call order.
    fn close_step_if_resolved(&mut self, cursor: Cursor) {
        let resolved = self.open_step.as_ref().is_some_and(|step| {
            step.states
                .iter()
                .all(|state| matches!(state, CallState::Done(_)))
        });
        if !resolved {
            return;
        }
        let Some(step) = self.open_step.take() else {
            return;
        };
        for (call, state) in step.calls.into_iter().zip(step.states) {
            if let CallState::Done(result) = state {
                self.push(
                    cursor,
                    Message::Tool {
                        call: call.id,
                        name: call.tool,
                        outcome: result.outcome,
                        output: result.output,
                    },
                );
            }
        }
    }

    /// A new turn over an unfinished step: every call needs a result for the
    /// model's history to stay well formed.
    fn close_abandoned_step(&mut self, cursor: Cursor) {
        if let Some(step) = &mut self.open_step {
            for state in &mut step.states {
                let result = match state {
                    CallState::Done(_) => continue,
                    CallState::Started => ToolResult::unknown(UNKNOWN_NEW_TURN),
                    _ => ToolResult::error(NOT_RUN_NEW_TURN),
                };
                *state = CallState::Done(result);
            }
        }
        self.close_step_if_resolved(cursor);
    }
}
