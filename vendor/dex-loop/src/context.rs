//! The in-memory state of one thread, derived from its log.
//!
//! `Context::observe` is the only state transition. The engine calls it for
//! every event it appends and every control event it reads; `rehydrate` calls
//! it for every event in the log. The same events therefore always produce
//! the same context, whether the actor stayed warm or restarted.

use crate::event::{
    ActionConfirmation, ApprovalId, ApprovalMode, ArtifactRef, CallId, ClientToolSpec,
    ConfirmationDecision, Cursor, Event, HEADLESS_AUTO_APPROVER, MessageId, Outcome, Output,
    PrincipalId, ProposedCall, ProviderReasoning, ServedBy, ThreadId, ToolName, ToolResult, TurnId,
    Usage,
};

const NOT_RUN_NEW_TURN: &str = "not run: a new turn started first";
const UNKNOWN_NEW_TURN: &str = "outcome unknown: a new turn started before the result was recorded";
const MAX_ACTION_RECORDS: usize = 128;
// A public question permits 4096 characters, each at most four UTF-8 bytes.
const MAX_ACTION_ARGUMENT_BYTES: usize = 16 * 1024;

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
        /// The provider and model that served the step, when known.
        served: Option<ServedBy>,
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
            Message::Assistant {
                text,
                calls,
                reasoning,
                ..
            } => {
                text.len()
                    + reasoning
                        .as_ref()
                        .map_or(0, |state| state.payload.to_string().len())
                    + calls
                        .iter()
                        .map(|call| call.tool.as_str().len() + call.args.to_string().len())
                        .sum::<usize>()
            }
            Message::Tool { output, .. } => match output {
                Output::Text(text) => text.len(),
                // A reference may render a preview as well as metadata. Dex's
                // host caps a preview at 16 KiB; count that conservative
                // allowance so tool-heavy histories compact before rendering.
                Output::Ref(reference) => reference.as_str().len().saturating_add(16 * 1024),
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

/// The most recent consecutive failed operation, derived from tool events.
#[derive(Clone, Debug, PartialEq)]
struct FailedCall {
    principal: PrincipalId,
    tool: ToolName,
    args: serde_json::Value,
    count: u32,
}

/// The latest owner tool completions retained independently of model history.
/// Proposals and results are paired only by the typed event fold.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolEvidence {
    pub cursor: Cursor,
    pub call: ProposedCall,
    pub result: ToolResult,
}

/// Evidence retention is bounded. Evicted results no longer authorize follow-up
/// verification; callers must run the tool again to establish fresh evidence.
pub const TOOL_EVIDENCE_LIMIT: usize = 128;

/// Maximum document references retained across accepted messages. The current
/// message remains exact so the document owner can reject invalid admissions.
pub const MAX_CONTEXT_ATTACHMENTS: usize = 20;

/// Typed provenance derived only from accepted user messages, independent of
/// summaries. These coordinates select evidence; the document owner still
/// verifies message admission, principal access and immutable versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentInput {
    pub turn: TurnId,
    pub message_id: Option<MessageId>,
    pub principal: PrincipalId,
    pub attachments: Vec<ArtifactRef>,
}

/// Everything the engine knows about a thread.
#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    thread: ThreadId,
    turn: Option<TurnId>,
    acting: Option<PrincipalId>,
    status: Status,
    history: Vec<Entry>,
    tool_evidence: Vec<ToolEvidence>,
    // Newest input first (even when it has no uploads), then bounded earlier
    // attachment batches. Compaction never manufactures or edits provenance.
    attachment_inputs: Vec<AttachmentInput>,
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
    /// Unknown call outcomes in this turn, derived from the durable log.
    /// Kept outside model history so compaction cannot permit a fresh retry.
    uncertain_calls: Vec<ProposedCall>,
    // Derived only from exact Question/Answer rows, retained outside summaries.
    action_confirmations: Vec<(CallId, ActionConfirmation, ConfirmationDecision, bool)>,
    // Exact owner policy previews survive model-history cuts. Expired refs fail closed.
    action_previews: Vec<ProposedCall>,
    /// Calls whose `ToolStarted` landed before their step's
    /// `ModelStepCompleted`: reads the engine started while the model was
    /// still streaming. They begin the step as `Started`, so a warm engine
    /// adopts their results and a rehydrated one runs them again, the same
    /// as any other read that started before a restart.
    pre_started: Vec<CallId>,
    /// Cursor of the latest `Compaction` event itself (not the cursor it
    /// covers to). Assistant entries at or before it were produced before
    /// the summary existed.
    last_compaction: Option<Cursor>,
    /// Derived from tool events, outside compactable history. This tracks a
    /// consecutive failure, not a permanent blacklist or an automatic retry.
    failed_call: Option<FailedCall>,
    /// Set only on the model-attempt copy, never on durable thread context.
    remaining_budget: Option<crate::RemainingBudget>,
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
            tool_evidence: Vec::new(),
            attachment_inputs: Vec::new(),
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
            uncertain_calls: Vec::new(),
            action_confirmations: Vec::new(),
            action_previews: Vec::new(),
            pending_turns: Vec::new(),
            pre_started: Vec::new(),
            last_compaction: None,
            failed_call: None,
            remaining_budget: None,
        }
    }

    /// Owner results survive summaries; model-authored summary text is never
    /// folded into this evidence. The records remain in event cursor order.
    pub fn tool_evidence(&self) -> &[ToolEvidence] {
        &self.tool_evidence
    }

    /// Assistant entries created before this event cannot reuse signed thinking.
    pub fn last_compaction_cursor(&self) -> Option<Cursor> {
        self.last_compaction
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

    /// Advisory capacity, refreshed before every model stream. It does not
    /// grant permission or change the engine's caps.
    pub fn remaining_budget(&self) -> Option<crate::RemainingBudget> {
        self.remaining_budget
    }

    pub(crate) fn for_model(&self, remaining: crate::RemainingBudget) -> Self {
        let mut ctx = self.clone();
        ctx.remaining_budget = Some(remaining);
        ctx
    }

    /// A fresh identical failed call needs new information or a new approach.
    pub fn recovery_guidance(&self) -> Option<String> {
        self.failed_call.as_ref().filter(|failure| failure.count >= 3).map(|failure| {
            format!("The same call to {} failed {} consecutive times without progress. Do not repeat the identical call. Inspect the failure, change the inputs or approach, use another available capability, or report the concrete blocker. A successful intervening call or new user input resets this guard. Never retry a mutation with an unknown outcome.", failure.tool, failure.count)
        })
    }

    pub(crate) fn has_stalled_call(&self, call: &ProposedCall) -> bool {
        self.failed_call.as_ref().is_some_and(|failure| {
            failure.count >= 3
                && failure.principal == call.principal
                && failure.tool == call.tool
                && failure.args == call.args
        })
    }

    /// The model's view of the thread.
    pub fn history(&self) -> &[Entry] {
        &self.history
    }

    pub fn proposed_call(&self, id: &CallId) -> Option<&ProposedCall> {
        self.action_preview(id).or_else(|| {
            self.history
                .iter()
                .rev()
                .filter_map(|entry| match &entry.message {
                    Message::Assistant { calls, .. } => Some(calls),
                    _ => None,
                })
                .flatten()
                .find(|call| &call.id == id)
        })
    }

    /// Render owner-produced preview details, rather than model-authored consent prose.
    pub fn action_question_text(&self, proposal: &CallId, action: &str) -> Option<String> {
        let mut arguments = self.action_preview(proposal)?.args.clone();
        if let Some(arguments) = arguments.as_object_mut() {
            arguments.remove("confirmation");
        }
        let details = serde_json::to_string_pretty(&arguments).ok()?;
        let text = format!("Confirm this action?\n{action}\n\n{details}");
        // The public question contract must show the complete preview.
        (text.chars().count() <= 4096).then_some(text)
    }

    pub fn is_unexecuted_preview(&self, call: &CallId) -> bool {
        self.action_preview(call).is_some()
    }

    /// An exact failed policy preview before dispatch, retained separately from summaries.
    pub fn action_preview(&self, call: &CallId) -> Option<&ProposedCall> {
        self.action_previews
            .iter()
            .find(|proposal| &proposal.id == call)
    }

    pub fn confirmation_question_exists(&self, proposal: &CallId) -> bool {
        self.action_confirmations
            .iter()
            .any(|(_, binding, _, _)| &binding.proposal_call_id == proposal)
    }

    /// A typed affirmative choice for this exact action, unused by any dispatch.
    pub fn confirmed_action(&self, call: &ProposedCall) -> bool {
        let Some(id) = call
            .args
            .get("confirmation")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        let mut args = call.args.clone();
        if let Some(args) = args.as_object_mut() {
            args.remove("confirmation");
        }
        let digest = crate::args_digest(&args);
        self.action_confirmations
            .iter()
            .any(|(_, binding, decision, used)| {
                !used
                    && *decision == ConfirmationDecision::Confirm
                    && binding.proposal_call_id.as_str() == id
                    && binding.tool == call.tool
                    && binding.principal_id == call.principal
                    && binding.args_digest == digest
            })
    }

    /// Newest accepted/applied input and bounded older attachment provenance.
    pub fn attachment_inputs(&self) -> &[AttachmentInput] {
        &self.attachment_inputs
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

    /// Whether a turn is running (or a queued one just began). A host that
    /// finished a turn checks this before doing work between turns.
    pub fn turn_running(&self) -> bool {
        self.status == Status::Running
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

    /// A fresh ID does not make an unresolved operation safe to repeat.
    pub(crate) fn has_uncertain_call(&self, call: &ProposedCall) -> bool {
        self.uncertain_calls.iter().any(|prior| {
            prior.id != call.id
                && prior.tool == call.tool
                && prior.args_digest == call.args_digest
                && prior.principal == call.principal
        })
    }

    /// Reads a cut attempt started ahead of its step's commit and never
    /// finished, in log order.
    pub(crate) fn pre_started_calls(&self) -> &[CallId] {
        &self.pre_started
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
                principal,
            } => {
                // Older engines wrote synthetic approvals in headless mode.
                // Retain those rows as history, but they cannot authorize an
                // unstarted parked call after an upgrade. Later ToolStarted /
                // ToolFinished rows still reconstruct effects already run.
                if let Some(CallState::Parked {
                    approval: parked,
                    decision,
                }) = self.state_mut(call)
                    && principal.as_str() != HEADLESS_AUTO_APPROVER
                    && parked == approval
                    && decision.is_none()
                {
                    *decision = Some(Decision {
                        approved: *approved,
                        args_digest: args_digest.clone(),
                    });
                }
            }
            Event::Answer {
                call,
                principal,
                text,
                confirmation_decision,
                args_digest,
            } => {
                for (question, binding, decision, used) in &mut self.action_confirmations {
                    if question == call
                        && &binding.principal_id == principal
                        && binding.args_digest == *args_digest
                        && *decision == ConfirmationDecision::Unspecified
                        && !*used
                    {
                        *decision = *confirmation_decision;
                        // A normal answer closes the question without granting consent.
                        if *confirmation_decision == ConfirmationDecision::Unspecified {
                            *used = true;
                        }
                    }
                }
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
            Event::TextDelta { .. }
            | Event::ThinkingDelta { .. }
            | Event::ModelAttemptFailed { .. }
            | Event::ToolProgress { .. } => {}
            Event::Usage(usage) => self.usage += *usage,
            Event::ModelStepCompleted {
                text,
                calls,
                reasoning,
                served,
                ..
            } => {
                self.attempt = None;
                self.push(
                    cursor,
                    Message::Assistant {
                        text: text.clone(),
                        calls: calls.clone(),
                        reasoning: reasoning.clone(),
                        served: served.clone(),
                    },
                );
                if !calls.is_empty() {
                    let pre_started = std::mem::take(&mut self.pre_started);
                    self.open_step = Some(OpenStep {
                        calls: calls.clone(),
                        states: calls
                            .iter()
                            .map(|call| {
                                if pre_started.contains(&call.id) {
                                    CallState::Started
                                } else {
                                    CallState::Todo
                                }
                            })
                            .collect(),
                    });
                }
                self.pre_started.clear();
            }
            Event::ModelAttemptAbandoned { .. } => {
                self.attempt = None;
                self.pre_started.clear();
            }
            Event::ToolStarted { call, .. } => {
                if let Some(proposal) = self.proposed_call(call).cloned()
                    && self.confirmed_action(&proposal)
                {
                    let id = proposal
                        .args
                        .get("confirmation")
                        .and_then(serde_json::Value::as_str);
                    for (_, binding, _, used) in &mut self.action_confirmations {
                        if Some(binding.proposal_call_id.as_str()) == id {
                            *used = true;
                        }
                    }
                }
                if let Some(state) = self.state_mut(call) {
                    if !matches!(state, CallState::Done(_)) {
                        *state = CallState::Started;
                    }
                } else if self.open_step.is_none() && !self.pre_started.contains(call) {
                    // Started ahead of its step's commit point.
                    self.pre_started.push(call.clone());
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
                if let Some(proposal) = self
                    .open_step
                    .as_ref()
                    .and_then(|step| step.calls.iter().find(|proposal| &proposal.id == call))
                    .cloned()
                    && !self
                        .tool_evidence
                        .iter()
                        .any(|record| record.call.id == *call)
                {
                    self.tool_evidence.push(ToolEvidence {
                        cursor,
                        call: proposal,
                        result: ToolResult {
                            outcome: *outcome,
                            output: output.clone(),
                            receipt: receipt.clone(),
                        },
                    });
                    if self.tool_evidence.len() > TOOL_EVIDENCE_LIMIT {
                        self.tool_evidence.remove(0);
                    }
                }
                // A confirmation preview is an owner policy refusal before the
                // dispatch boundary. External error prose cannot manufacture one.
                let unstarted = self.open_step.as_ref().and_then(|step| {
                    step.calls
                        .iter()
                        .zip(&step.states)
                        .find(|(proposal, state)| {
                            &proposal.id == call && matches!(state, CallState::Todo)
                        })
                        .map(|(proposal, _)| proposal.clone())
                });
                if let Some(proposal) = unstarted
                    && *outcome == Outcome::Failed
                    && let Output::Text(text) = output
                    && let Ok(document) = serde_json::from_str::<serde_json::Value>(text)
                    && document.get("status").and_then(serde_json::Value::as_str)
                        == Some("needs_confirmation")
                {
                    let mut args = proposal.args.clone();
                    if let Some(args) = args.as_object_mut() {
                        args.remove("confirmation");
                    }
                    if document
                        .get("args_digest")
                        .and_then(serde_json::Value::as_str)
                        == Some(crate::args_digest(&args).as_str())
                        && serde_json::to_string_pretty(&args)
                            .is_ok_and(|text| text.len() <= MAX_ACTION_ARGUMENT_BYTES)
                        && self.action_preview(call).is_none()
                    {
                        self.action_previews.push(ProposedCall::new(
                            proposal.id,
                            proposal.tool,
                            args,
                            proposal.principal,
                        ));
                        if self.action_previews.len() > MAX_ACTION_RECORDS {
                            let expired = self.action_previews.remove(0);
                            self.action_confirmations.retain(|(_, binding, _, _)| {
                                binding.proposal_call_id != expired.id
                            });
                        }
                    }
                }
                self.uncertain_calls.retain(|prior| &prior.id != call);
                // A pre-committed read finished by an abandoned attempt.
                self.pre_started.retain(|started| started != call);
                if *outcome == Outcome::Unknown
                    && let Some(proposal) = self
                        .open_step
                        .as_ref()
                        .and_then(|step| step.calls.iter().find(|proposal| &proposal.id == call))
                {
                    self.uncertain_calls.push(proposal.clone());
                }
                if let Some(proposal) = self
                    .open_step
                    .as_ref()
                    .and_then(|step| step.calls.iter().find(|proposal| &proposal.id == call))
                {
                    if *outcome == Outcome::Failed {
                        let count = self
                            .failed_call
                            .as_ref()
                            .filter(|failure| {
                                failure.principal == proposal.principal
                                    && failure.tool == proposal.tool
                                    && failure.args == proposal.args
                            })
                            .map_or(1, |failure| failure.count.saturating_add(1));
                        self.failed_call = Some(FailedCall {
                            principal: proposal.principal.clone(),
                            tool: proposal.tool.clone(),
                            args: proposal.args.clone(),
                            count,
                        });
                    } else {
                        self.failed_call = None;
                    }
                }
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
            Event::AutoApproved {
                call,
                approval,
                args_digest,
                ..
            } => {
                // The receipt is the decision: the call is dispatchable at
                // once, and a replay adopts the same digest the engine bound.
                if let Some(state) = self.state_mut(call) {
                    *state = CallState::Parked {
                        approval: approval.clone(),
                        decision: Some(Decision {
                            approved: true,
                            args_digest: args_digest.clone(),
                        }),
                    };
                }
            }
            Event::Question {
                call, confirmation, ..
            } => {
                if let Some(binding) = confirmation
                    && !self
                        .action_confirmations
                        .iter()
                        .any(|(question, _, _, _)| question == call)
                {
                    self.action_confirmations.push((
                        call.clone(),
                        binding.clone(),
                        ConfirmationDecision::Unspecified,
                        false,
                    ));
                    if self.action_confirmations.len() > MAX_ACTION_RECORDS {
                        let (_, expired, _, _) = self.action_confirmations.remove(0);
                        self.action_previews
                            .retain(|proposal| proposal.id != expired.proposal_call_id);
                    }
                }
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
                self.last_compaction = Some(cursor);
                // Input and applied steers of the current turn retain their
                // exact text, principal, and attachments across compaction.
                // Covered inputs precede the new summary, keeping history
                // cursors non-decreasing even after repeated compaction.
                self.history.retain(|entry| {
                    entry.cursor > *covers_to_cursor
                        || matches!(&entry.message, Message::User { turn, .. } if self.status == Status::Running && Some(turn) == self.turn.as_ref())
                });
                let position = self
                    .history
                    .partition_point(|entry| entry.cursor <= *covers_to_cursor);
                self.history.insert(
                    position,
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
        self.uncertain_calls.clear();
        self.failed_call = None;
        self.pre_started.clear();
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
        if let Message::User {
            turn,
            message_id,
            principal,
            attachments,
            ..
        } = &message
        {
            let current = AttachmentInput {
                turn: turn.clone(),
                message_id: message_id.clone(),
                principal: principal.clone(),
                attachments: attachments.clone(),
            };
            let mut seen: std::collections::BTreeSet<_> = attachments.iter().cloned().collect();
            let mut budget = MAX_CONTEXT_ATTACHMENTS.saturating_sub(attachments.len());
            let mut retained = vec![current];
            for mut previous in std::mem::take(&mut self.attachment_inputs) {
                if budget == 0 {
                    break;
                }
                previous
                    .attachments
                    .retain(|reference| seen.insert(reference.clone()));
                previous.attachments.truncate(budget);
                if !previous.attachments.is_empty() {
                    budget -= previous.attachments.len();
                    retained.push(previous);
                }
            }
            self.attachment_inputs = retained;
        }
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
        self.failed_call = None;
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
