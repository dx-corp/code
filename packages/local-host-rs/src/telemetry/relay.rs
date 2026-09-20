//! Process-wide, bounded, content-free handoff from runtime events to telemetry.
//!
//! The producer never waits and never performs telemetry I/O. Rich runtime
//! payloads remain on their existing consumer path; this queue receives only
//! the fields the turn collector needs. Streaming content is reduced to byte
//! counts before it can enter the queue.

use super::{
    TelemetryIdentityScope, TurnJournal, TurnTracker, TurnTrackerConfig, TurnTrackerContext,
    record_canonical_turn_event,
};
use crate::agent::{FromAgent, TokenUsage};
use maestro_runtime::ExecutionStatus;
use std::collections::HashMap;
use std::sync::{
    Arc, OnceLock, RwLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel},
};

const QUEUE_CAPACITY: usize = 256;
const WORKER_STACK_BYTES: usize = 256 * 1024;
const INLINE_LABEL_BYTES: usize = 80;
const MAX_BATCH: usize = 256;

static TELEMETRY_HUB: OnceLock<TelemetryHub> = OnceLock::new();
static NEXT_RELAY_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static WORKER_STARTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InlineLabel {
    len: u8,
    bytes: [u8; INLINE_LABEL_BYTES],
}

impl InlineLabel {
    fn new(value: &str) -> Self {
        let mut len = value.len().min(INLINE_LABEL_BYTES);
        while !value.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0; INLINE_LABEL_BYTES];
        bytes[..len].copy_from_slice(&value.as_bytes()[..len]);
        Self {
            len: len as u8,
            bytes,
        }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("inline telemetry labels are copied from valid UTF-8")
    }
}

struct TelemetryHub {
    sender: SyncSender<HubMessage>,
    control_sender: Sender<ControlMessage>,
    worker: std::thread::Thread,
}

struct RelayRegistration {
    relay_id: u64,
    config: TurnTrackerConfig,
    context: TurnTrackerContext,
    identity_scope: Arc<RwLock<Option<TelemetryIdentityScope>>>,
    telemetry_host: Option<maestro_runtime::agent::NativeExecutionHostHandle>,
}

pub(crate) enum HubMessage {
    Observe {
        relay_id: u64,
        queued: QueuedObservation,
    },
}

enum ControlMessage {
    Register(Box<RelayRegistration>),
    Unregister { relay_id: u64, attempted: u64 },
}

struct RelayWorkerState {
    tracker: TurnTracker,
    identity_scope: Arc<RwLock<Option<TelemetryIdentityScope>>>,
    telemetry_host: Option<maestro_runtime::agent::NativeExecutionHostHandle>,
    sequence_cursor: SequenceCursor,
    closing_attempted: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct DrainReport {
    observations: usize,
    closed: Vec<(u64, u32)>,
}

fn telemetry_hub() -> &'static TelemetryHub {
    TELEMETRY_HUB.get_or_init(|| {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let (control_sender, control_receiver) = channel();
        let worker = std::thread::Builder::new()
            .name("maestro-telemetry".to_owned())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || run_hub(receiver, control_receiver))
            .expect("spawn process telemetry worker");
        #[cfg(test)]
        WORKER_STARTS.fetch_add(1, Ordering::Relaxed);
        TelemetryHub {
            sender,
            control_sender,
            worker: worker.thread().clone(),
        }
    })
}

fn run_hub(receiver: Receiver<HubMessage>, control_receiver: Receiver<ControlMessage>) {
    let mut states = HashMap::<u64, RelayWorkerState>::new();
    let mut journal = TurnJournal::open();
    loop {
        drain_hub_once(&receiver, &control_receiver, &mut states, &mut journal);
        std::thread::park();
    }
}

fn drain_control_messages(
    control_receiver: &Receiver<ControlMessage>,
    states: &mut HashMap<u64, RelayWorkerState>,
) {
    for message in control_receiver.try_iter() {
        handle_control_message(message, states);
    }
}

fn drain_hub_once(
    receiver: &Receiver<HubMessage>,
    control_receiver: &Receiver<ControlMessage>,
    states: &mut HashMap<u64, RelayWorkerState>,
    journal: &mut Option<TurnJournal>,
) -> DrainReport {
    // Control is sent before data by every relay. Drain it again after the
    // data wakeup so a newly registered relay cannot lose its first event.
    drain_control_messages(control_receiver, states);
    let mut processed = 0;
    for message in receiver.try_iter().take(MAX_BATCH) {
        handle_hub_message(message, states, journal);
        processed += 1;
    }
    let closed = finalize_closed_relays(states, journal);
    DrainReport {
        observations: processed,
        closed,
    }
}

fn finalize_closed_relays(
    states: &mut HashMap<u64, RelayWorkerState>,
    journal: &mut Option<TurnJournal>,
) -> Vec<(u64, u32)> {
    let closing = states
        .iter()
        .filter_map(|(relay_id, state)| {
            state
                .closing_attempted
                .map(|attempted| (*relay_id, attempted))
        })
        .collect::<Vec<_>>();
    let mut closed = Vec::with_capacity(closing.len());
    for (relay_id, attempted) in closing {
        let mut state = states
            .remove(&relay_id)
            .expect("closing telemetry relay must still be registered");
        recover_after_loss(
            &mut state.tracker,
            journal,
            state.sequence_cursor.trailing_losses(attempted),
        );
        closed.push((relay_id, state.tracker.turn_number()));
    }
    closed
}

fn handle_hub_message(
    message: HubMessage,
    states: &mut HashMap<u64, RelayWorkerState>,
    journal: &mut Option<TurnJournal>,
) {
    let HubMessage::Observe { relay_id, queued } = message;
    let Some(state) = states.get_mut(&relay_id) else {
        return;
    };
    recover_after_loss(
        &mut state.tracker,
        journal,
        state.sequence_cursor.observe(queued.sequence),
    );
    let observation = queued.observation;
    let snapshot = observation.needs_pending_snapshot();
    if observation.refreshes_identity() {
        state.tracker.set_identity_scope(
            state
                .identity_scope
                .read()
                .expect("telemetry identity scope lock poisoned")
                .clone(),
        );
    }
    if observation.refreshes_session() {
        let session_id = state
            .telemetry_host
            .as_ref()
            .and_then(|host| futures::executor::block_on(host.hook_session_id()));
        state.tracker.set_session_id(session_id.unwrap_or_default());
    }
    if let Some(completed) = handle_observation(&mut state.tracker, observation).as_ref() {
        finish(journal, completed);
    }
    if snapshot {
        observe_pending(journal, &state.tracker);
    }
}

fn handle_control_message(message: ControlMessage, states: &mut HashMap<u64, RelayWorkerState>) {
    match message {
        ControlMessage::Register(registration) => {
            let mut tracker = TurnTracker::new(registration.config);
            tracker.update_context(registration.context);
            states.insert(
                registration.relay_id,
                RelayWorkerState {
                    tracker,
                    identity_scope: registration.identity_scope,
                    telemetry_host: registration.telemetry_host,
                    sequence_cursor: SequenceCursor::default(),
                    closing_attempted: None,
                },
            );
        }
        ControlMessage::Unregister {
            relay_id,
            attempted,
        } => {
            if let Some(state) = states.get_mut(&relay_id) {
                state.closing_attempted = Some(attempted);
            }
        }
    }
}

#[cfg(test)]
fn worker_start_count_for_test() -> u64 {
    WORKER_STARTS.load(Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub(crate) struct ToolReceiptObservation {
    call_id: InlineLabel,
    tool_name: InlineLabel,
    status: ExecutionStatus,
    duration_ms: Option<u64>,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "boxing would allocate on the producer path; the process queue has a tested 128 KiB cap"
)]
pub(crate) enum TurnObservation {
    Event(TelemetryEvent),
    Output {
        bytes: u64,
        first_observed_at: std::time::Instant,
    },
    SideQuestionEnd {
        side_id: InlineLabel,
        answer_bytes: u64,
        usage: Option<TokenUsage>,
        failed: bool,
    },
    ToolCall {
        call_id: InlineLabel,
        tool: InlineLabel,
        requires_approval: bool,
    },
    ToolEnd {
        call_id: InlineLabel,
        success: bool,
        receipt: Option<ToolReceiptObservation>,
    },
    Session(Option<InlineLabel>),
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "boxing would allocate on the producer path; the process queue has a tested 128 KiB cap"
)]
pub(crate) enum TelemetryEvent {
    Ready {
        model: InlineLabel,
        provider: InlineLabel,
    },
    ModelChanged {
        model: InlineLabel,
        provider: InlineLabel,
    },
    BoostChanged {
        status: maestro_runtime::agent::BoostStatus,
        thinking: Option<maestro_runtime::agent::ThinkingLevel>,
    },
    ResponseStart {
        response_id: InlineLabel,
    },
    ResponseEnd {
        response_id: InlineLabel,
        usage: Option<TokenUsage>,
    },
    TurnCompleted,
    TurnInterrupted,
    SideQuestionStart {
        side_id: InlineLabel,
        standalone: bool,
    },
    CodexUsageState {
        source: InlineLabel,
        usage: Option<TokenUsage>,
    },
    StreamObservation(SafeStreamObservation),
    ContextCalibration {
        request_id: InlineLabel,
        generation: u64,
        estimated_input_tokens: u64,
        observed_input_tokens: u64,
    },
    RequestRetryObservation,
    RequestContextPrepared {
        response_id: InlineLabel,
    },
    OperationObservation(CompactOperationObservation),
    TurnStarted,
    RequestRetryScheduled {
        attempt: u32,
        delay_ms: u64,
        rate_limited: bool,
    },
    CompactionMeasured {
        duration_ms: u64,
    },
    ToolStart {
        call_id: InlineLabel,
    },
    Error {
        fatal: bool,
        terminal: bool,
        retryable: bool,
    },
    ProviderError {
        kind: maestro_ai::ProviderStreamErrorKind,
    },
    Compaction {
        first_kept_entry_index: usize,
        tokens_before: u64,
        auto: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SafeStreamObservation {
    Observed,
    OpenFailed,
    IdleTimeout,
    Disconnect,
    Retry,
    Recovery,
}

#[derive(Debug)]
pub(crate) enum CompactOperationObservation {
    Admitted {
        turn_id: InlineLabel,
        thinking_level: InlineLabel,
        experiment: Option<maestro_runtime_contracts::experiments::ExperimentObservation>,
    },
    Prepared {
        response_id: InlineLabel,
        model_id: InlineLabel,
        model_provider: InlineLabel,
        message_count: u32,
        input_size_bytes: Option<u64>,
    },
    ReasoningUsage {
        response_id: InlineLabel,
        tokens: u64,
    },
    GatewayReceipt {
        response_id: InlineLabel,
        request_id: InlineLabel,
        record_id: InlineLabel,
        lineage_id: InlineLabel,
        provider_tools_sha256: Option<InlineLabel>,
        provider_tool_count: Option<u32>,
    },
}

#[derive(Debug)]
pub(crate) struct QueuedObservation {
    sequence: u64,
    observation: TurnObservation,
}

#[derive(Default)]
struct SequenceCursor {
    next: u64,
}

impl SequenceCursor {
    fn observe(&mut self, sequence: u64) -> u64 {
        let missed = sequence.saturating_sub(self.next);
        self.next = sequence.saturating_add(1);
        missed
    }

    fn trailing_losses(&self, attempted: u64) -> u64 {
        attempted.saturating_sub(self.next)
    }
}

impl TurnObservation {
    fn needs_pending_snapshot(&self) -> bool {
        matches!(
            self,
            Self::Event(
                TelemetryEvent::TurnStarted
                    | TelemetryEvent::OperationObservation(_)
                    | TelemetryEvent::SideQuestionStart { .. }
                    | TelemetryEvent::ResponseStart { .. }
                    | TelemetryEvent::ResponseEnd { .. }
            )
        )
    }

    fn refreshes_identity(&self) -> bool {
        matches!(self, Self::Event(TelemetryEvent::ModelChanged { .. }))
    }

    fn refreshes_session(&self) -> bool {
        matches!(
            self,
            Self::Event(
                TelemetryEvent::TurnStarted
                    | TelemetryEvent::ResponseStart { .. }
                    | TelemetryEvent::SideQuestionStart { .. }
                    | TelemetryEvent::OperationObservation(
                        CompactOperationObservation::Admitted { .. }
                    )
            )
        )
    }
}

/// A tiny producer facade. Its only hot-path state is a byte accumulator and
/// the first-output instant, so token chunks never allocate or occupy queue
/// slots. All agents share one parked worker and one journal; aggregation,
/// serialization, locking, disk, and network work stay off the event relay.
pub(crate) struct TurnTelemetryRelay {
    sender: Option<SyncSender<HubMessage>>,
    control_sender: Option<Sender<ControlMessage>>,
    worker: Option<std::thread::Thread>,
    relay_id: u64,
    dropped: Arc<AtomicU64>,
    attempted: Arc<AtomicU64>,
    next_sequence: u64,
    pending_output_bytes: u64,
    pending_first_output_at: Option<std::time::Instant>,
}

impl TurnTelemetryRelay {
    pub(crate) fn spawn(
        config: TurnTrackerConfig,
        context: TurnTrackerContext,
        identity_scope: Arc<RwLock<Option<TelemetryIdentityScope>>>,
        telemetry_host: Option<maestro_runtime::agent::NativeExecutionHostHandle>,
    ) -> Self {
        if super::experiments_telemetry_disabled() {
            return Self {
                sender: None,
                control_sender: None,
                worker: None,
                relay_id: 0,
                dropped: Arc::new(AtomicU64::new(0)),
                attempted: Arc::new(AtomicU64::new(0)),
                next_sequence: 0,
                pending_output_bytes: 0,
                pending_first_output_at: None,
            };
        }

        let sender = telemetry_hub().sender.clone();
        let control_sender = telemetry_hub().control_sender.clone();
        let worker = telemetry_hub().worker.clone();
        let relay_id = NEXT_RELAY_ID.fetch_add(1, Ordering::Relaxed);
        let dropped = Arc::new(AtomicU64::new(0));
        let attempted = Arc::new(AtomicU64::new(0));
        let registered = control_sender
            .send(ControlMessage::Register(Box::new(RelayRegistration {
                relay_id,
                config,
                context,
                identity_scope,
                telemetry_host,
            })))
            .is_ok();
        if registered {
            worker.unpark();
        }

        Self {
            sender: registered.then_some(sender),
            control_sender: registered.then_some(control_sender),
            worker: registered.then_some(worker),
            relay_id,
            dropped,
            attempted,
            next_sequence: 0,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        }
    }

    /// Project before the rich event is moved to its consumer, then enqueue
    /// only after that consumer has been notified. Saturation is intentionally
    /// lossy and never delays event delivery.
    pub(crate) fn project(&mut self, event: &FromAgent) -> Option<TurnObservation> {
        match event {
            FromAgent::ResponseChunk {
                content,
                is_thinking,
                ..
            } => {
                if !is_thinking && !content.is_empty() {
                    self.pending_first_output_at
                        .get_or_insert_with(std::time::Instant::now);
                    self.pending_output_bytes = self
                        .pending_output_bytes
                        .saturating_add(content.len() as u64);
                }
                None
            }
            FromAgent::ToolOutput { .. }
            | FromAgent::LocalAssistantContent { .. }
            | FromAgent::ConversationSnapshot { .. }
            | FromAgent::ManagedAuthorizationRequest { .. }
            | FromAgent::ManagedGatewayReceipt { .. }
            | FromAgent::ModelChangeFailed { .. }
            | FromAgent::SideQuestionChunk { .. }
            | FromAgent::CodexSessionState { .. }
            | FromAgent::CodexTurnState { .. }
            | FromAgent::CodexCompatibility { .. }
            | FromAgent::CodexNativeOperation { .. }
            | FromAgent::CodexNativeDecision { .. }
            | FromAgent::CodexTransportReceipt { .. }
            | FromAgent::BatchStart { .. }
            | FromAgent::BatchEnd { .. }
            | FromAgent::Status { .. }
            | FromAgent::HookBlocked { .. } => None,
            FromAgent::SessionInfo { session_id, .. } => Some(TurnObservation::Session(
                session_id.as_deref().map(InlineLabel::new),
            )),
            FromAgent::ToolCall {
                call_id,
                tool,
                requires_approval,
                ..
            } => Some(TurnObservation::ToolCall {
                call_id: InlineLabel::new(call_id),
                tool: InlineLabel::new(tool),
                requires_approval: *requires_approval,
            }),
            FromAgent::ToolEnd {
                call_id,
                success,
                receipt,
                ..
            } => Some(TurnObservation::ToolEnd {
                call_id: InlineLabel::new(call_id),
                success: *success,
                receipt: receipt.as_ref().map(|receipt| ToolReceiptObservation {
                    call_id: InlineLabel::new(&receipt.call_id),
                    tool_name: InlineLabel::new(&receipt.tool_name),
                    status: receipt.status,
                    duration_ms: receipt.duration_ms,
                }),
            }),
            FromAgent::SideQuestionEnd {
                side_id,
                answer,
                usage,
                error,
                ..
            } => Some(TurnObservation::SideQuestionEnd {
                side_id: InlineLabel::new(side_id),
                answer_bytes: answer.len() as u64,
                usage: usage.clone(),
                failed: error.is_some(),
            }),
            _ => sanitized_event(event).map(TurnObservation::Event),
        }
    }

    pub(crate) fn submit(&mut self, observation: Option<TurnObservation>) {
        if self.sender.is_none() {
            return;
        }
        let Some(observation) = observation else {
            return;
        };

        if self.pending_output_bytes > 0 {
            let bytes = std::mem::take(&mut self.pending_output_bytes);
            let first_observed_at = self
                .pending_first_output_at
                .take()
                .expect("nonzero output bytes require an observation time");
            self.try_submit(TurnObservation::Output {
                bytes,
                first_observed_at,
            });
        }
        self.try_submit(observation);
    }

    fn try_submit(&mut self, observation: TurnObservation) {
        let Some(sender) = &self.sender else {
            return;
        };
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.attempted.store(self.next_sequence, Ordering::Release);
        match sender.try_send(HubMessage::Observe {
            relay_id: self.relay_id,
            queued: QueuedObservation {
                sequence,
                observation,
            },
        }) {
            Ok(()) => {
                if let Some(worker) = &self.worker {
                    worker.unpark();
                }
            }
            Err(error) => self.record_drop(error),
        }
    }

    fn record_drop(&self, _error: TrySendError<HubMessage>) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn saturated_for_test() -> (Self, std::sync::mpsc::Receiver<HubMessage>) {
        let (sender, receiver) = sync_channel(1);
        let (control_sender, _control_receiver) = channel();
        sender
            .try_send(HubMessage::Observe {
                relay_id: 1,
                queued: QueuedObservation {
                    sequence: 0,
                    observation: TurnObservation::Output {
                        bytes: 1,
                        first_observed_at: std::time::Instant::now(),
                    },
                },
            })
            .expect("test telemetry queue should start empty");
        (
            Self {
                sender: Some(sender),
                control_sender: Some(control_sender),
                worker: None,
                relay_id: 1,
                dropped: Arc::new(AtomicU64::new(0)),
                attempted: Arc::new(AtomicU64::new(1)),
                next_sequence: 1,
                pending_output_bytes: 0,
                pending_first_output_at: None,
            },
            receiver,
        )
    }
}

impl Drop for TurnTelemetryRelay {
    fn drop(&mut self) {
        if let Some(sender) = self.control_sender.take() {
            let _ = sender.send(ControlMessage::Unregister {
                relay_id: self.relay_id,
                attempted: self.attempted.load(Ordering::Acquire),
            });
            if let Some(worker) = &self.worker {
                worker.unpark();
            }
        }
    }
}

fn recover_after_loss(tracker: &mut TurnTracker, journal: &mut Option<TurnJournal>, count: u64) {
    if count == 0 {
        return;
    }
    tracing::warn!(
        dropped = count,
        "telemetry queue saturated; recording turn as incomplete"
    );
    for event in tracker.abandon_incomplete() {
        finish(journal, &event);
    }
}

fn finish(journal: &mut Option<TurnJournal>, event: &super::CanonicalTurnEvent) {
    if let Some(journal) = journal {
        journal.finish(event);
    } else {
        record_canonical_turn_event(event);
    }
}

fn observe_pending(journal: &mut Option<TurnJournal>, tracker: &TurnTracker) {
    if let Some(journal) = journal {
        journal.observe(&tracker.pending_snapshots());
    }
}

fn handle_observation(
    tracker: &mut TurnTracker,
    observation: TurnObservation,
) -> Option<super::CanonicalTurnEvent> {
    match observation {
        TurnObservation::Event(event) => tracker.handle_event(&event.into_agent_event()),
        TurnObservation::Output {
            bytes,
            first_observed_at,
        } => {
            tracker.record_output(bytes, first_observed_at);
            None
        }
        TurnObservation::SideQuestionEnd {
            side_id,
            answer_bytes,
            usage,
            failed,
        } => tracker.finish_side_question(side_id.as_str(), answer_bytes, usage.as_ref(), failed),
        TurnObservation::ToolCall {
            call_id,
            tool,
            requires_approval,
        } => {
            tracker.record_tool_call(call_id.as_str(), tool.as_str(), None, requires_approval);
            None
        }
        TurnObservation::ToolEnd {
            call_id,
            success,
            receipt,
        } => {
            tracker.record_tool_end(
                call_id.as_str(),
                success,
                receipt.as_ref().map(|receipt| {
                    (
                        receipt.call_id.as_str(),
                        receipt.tool_name.as_str(),
                        receipt.status,
                        receipt.duration_ms,
                    )
                }),
            );
            None
        }
        TurnObservation::Session(session_id) => {
            tracker.set_session_id(
                session_id.map_or_else(String::new, |value| value.as_str().to_owned()),
            );
            None
        }
    }
}

impl TelemetryEvent {
    fn into_agent_event(self) -> FromAgent {
        match self {
            Self::Ready { model, provider } => FromAgent::Ready {
                model: model.as_str().to_owned(),
                provider: provider.as_str().to_owned(),
            },
            Self::ModelChanged { model, provider } => FromAgent::ModelChanged {
                model: model.as_str().to_owned(),
                provider: provider.as_str().to_owned(),
            },
            Self::BoostChanged { status, thinking } => FromAgent::BoostChanged { status, thinking },
            Self::ResponseStart { response_id } => FromAgent::ResponseStart {
                response_id: response_id.as_str().to_owned(),
            },
            Self::ResponseEnd { response_id, usage } => FromAgent::ResponseEnd {
                response_id: response_id.as_str().to_owned(),
                usage,
            },
            Self::TurnCompleted => FromAgent::TurnCompleted {
                response_id: String::new(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
            Self::TurnInterrupted => FromAgent::TurnInterrupted {
                response_id: String::new(),
                reason: String::new(),
            },
            Self::SideQuestionStart {
                side_id,
                standalone,
            } => FromAgent::SideQuestionStart {
                side_id: side_id.as_str().to_owned(),
                question: String::new(),
                standalone,
            },
            Self::CodexUsageState { source, usage } => FromAgent::CodexUsageState {
                source: source.as_str().to_owned(),
                usage,
            },
            Self::StreamObservation(observation) => FromAgent::StreamObservation {
                observation: observation.into_runtime(),
            },
            Self::ContextCalibration {
                request_id,
                generation,
                estimated_input_tokens,
                observed_input_tokens,
            } => FromAgent::ContextCalibration {
                observation: maestro_context::context_usage::ContextCalibration {
                    request_id: request_id.as_str().to_owned(),
                    generation,
                    estimated_input_tokens,
                    observed_input_tokens,
                },
            },
            Self::RequestRetryObservation => FromAgent::RequestRetryObservation,
            Self::RequestContextPrepared { response_id } => FromAgent::RequestContextPrepared {
                response_id: response_id.as_str().to_owned(),
            },
            Self::OperationObservation(observation) => FromAgent::OperationObservation {
                observation: observation.into_runtime(),
            },
            Self::TurnStarted => FromAgent::TurnStarted,
            Self::RequestRetryScheduled {
                attempt,
                delay_ms,
                rate_limited,
            } => FromAgent::RequestRetryScheduled {
                attempt,
                delay_ms,
                rate_limited,
            },
            Self::CompactionMeasured { duration_ms } => {
                FromAgent::CompactionMeasured { duration_ms }
            }
            Self::ToolStart { call_id } => FromAgent::ToolStart {
                call_id: call_id.as_str().to_owned(),
            },
            Self::Error {
                fatal,
                terminal,
                retryable,
            } => FromAgent::Error {
                message: String::new(),
                fatal,
                terminal,
                retryable,
            },
            Self::ProviderError { kind } => FromAgent::ProviderError {
                kind,
                message: String::new(),
            },
            Self::Compaction {
                first_kept_entry_index,
                tokens_before,
                auto,
            } => FromAgent::Compaction {
                summary: String::new(),
                first_kept_entry_index,
                tokens_before,
                auto,
                custom_instructions: None,
                continuation: None,
                timestamp: String::new(),
            },
        }
    }
}

impl SafeStreamObservation {
    fn from_runtime(observation: maestro_ai::StreamObservation) -> Self {
        match observation {
            maestro_ai::StreamObservation::Observed => Self::Observed,
            maestro_ai::StreamObservation::OpenFailed => Self::OpenFailed,
            maestro_ai::StreamObservation::IdleTimeout => Self::IdleTimeout,
            maestro_ai::StreamObservation::Disconnect => Self::Disconnect,
            maestro_ai::StreamObservation::Retry => Self::Retry,
            maestro_ai::StreamObservation::Recovery => Self::Recovery,
        }
    }

    fn into_runtime(self) -> maestro_ai::StreamObservation {
        match self {
            Self::Observed => maestro_ai::StreamObservation::Observed,
            Self::OpenFailed => maestro_ai::StreamObservation::OpenFailed,
            Self::IdleTimeout => maestro_ai::StreamObservation::IdleTimeout,
            Self::Disconnect => maestro_ai::StreamObservation::Disconnect,
            Self::Retry => maestro_ai::StreamObservation::Retry,
            Self::Recovery => maestro_ai::StreamObservation::Recovery,
        }
    }
}

impl CompactOperationObservation {
    fn into_runtime(
        self,
    ) -> maestro_runtime_contracts::operation_observation::OperationObservation {
        use maestro_runtime_contracts::operation_observation::OperationObservation as O;
        match self {
            Self::Admitted {
                turn_id,
                thinking_level,
                experiment,
            } => O::Admitted {
                turn_id: turn_id.as_str().to_owned(),
                thinking_level: thinking_level.as_str().to_owned(),
                experiment,
            },
            Self::Prepared {
                response_id,
                model_id,
                model_provider,
                message_count,
                input_size_bytes,
            } => O::Prepared {
                response_id: response_id.as_str().to_owned(),
                model_id: model_id.as_str().to_owned(),
                model_provider: model_provider.as_str().to_owned(),
                message_count,
                input_size_bytes,
            },
            Self::ReasoningUsage {
                response_id,
                tokens,
            } => O::ReasoningUsage {
                response_id: response_id.as_str().to_owned(),
                tokens,
            },
            Self::GatewayReceipt {
                response_id,
                request_id,
                record_id,
                lineage_id,
                provider_tools_sha256,
                provider_tool_count,
            } => O::GatewayReceipt {
                response_id: response_id.as_str().to_owned(),
                request_id: request_id.as_str().to_owned(),
                record_id: record_id.as_str().to_owned(),
                lineage_id: lineage_id.as_str().to_owned(),
                provider_tools_sha256: provider_tools_sha256.map(|value| value.as_str().to_owned()),
                provider_tool_count,
            },
        }
    }
}

fn sanitized_event(event: &FromAgent) -> Option<TelemetryEvent> {
    Some(match event {
        FromAgent::Ready { model, provider } => TelemetryEvent::Ready {
            model: InlineLabel::new(model),
            provider: InlineLabel::new(provider),
        },
        FromAgent::ModelChanged { model, provider } => TelemetryEvent::ModelChanged {
            model: InlineLabel::new(model),
            provider: InlineLabel::new(provider),
        },
        FromAgent::BoostChanged { status, thinking } => TelemetryEvent::BoostChanged {
            status: *status,
            thinking: *thinking,
        },
        FromAgent::ResponseStart { response_id } => TelemetryEvent::ResponseStart {
            response_id: InlineLabel::new(response_id),
        },
        FromAgent::ResponseEnd { response_id, usage } => TelemetryEvent::ResponseEnd {
            response_id: InlineLabel::new(response_id),
            usage: usage.clone(),
        },
        FromAgent::TurnCompleted { .. } => TelemetryEvent::TurnCompleted,
        FromAgent::TurnInterrupted { .. } => TelemetryEvent::TurnInterrupted,
        FromAgent::SideQuestionStart {
            side_id,
            standalone,
            ..
        } => TelemetryEvent::SideQuestionStart {
            side_id: InlineLabel::new(side_id),
            standalone: *standalone,
        },
        FromAgent::CodexUsageState { source, usage } => TelemetryEvent::CodexUsageState {
            source: InlineLabel::new(source),
            usage: usage.clone(),
        },
        FromAgent::StreamObservation { observation } => {
            TelemetryEvent::StreamObservation(SafeStreamObservation::from_runtime(*observation))
        }
        FromAgent::ContextCalibration { observation } => TelemetryEvent::ContextCalibration {
            request_id: InlineLabel::new(&observation.request_id),
            generation: observation.generation,
            estimated_input_tokens: observation.estimated_input_tokens,
            observed_input_tokens: observation.observed_input_tokens,
        },
        FromAgent::RequestRetryObservation => TelemetryEvent::RequestRetryObservation,
        FromAgent::RequestContextPrepared { response_id } => {
            TelemetryEvent::RequestContextPrepared {
                response_id: InlineLabel::new(response_id),
            }
        }
        FromAgent::OperationObservation { observation } => {
            TelemetryEvent::OperationObservation(sanitized_operation_observation(observation))
        }
        FromAgent::TurnStarted => TelemetryEvent::TurnStarted,
        FromAgent::RequestRetryScheduled {
            attempt,
            delay_ms,
            rate_limited,
        } => TelemetryEvent::RequestRetryScheduled {
            attempt: *attempt,
            delay_ms: *delay_ms,
            rate_limited: *rate_limited,
        },
        FromAgent::CompactionMeasured { duration_ms } => TelemetryEvent::CompactionMeasured {
            duration_ms: *duration_ms,
        },
        FromAgent::ToolStart { call_id } => TelemetryEvent::ToolStart {
            call_id: InlineLabel::new(call_id),
        },
        FromAgent::Error {
            fatal,
            terminal,
            retryable,
            ..
        } => TelemetryEvent::Error {
            fatal: *fatal,
            terminal: *terminal,
            retryable: *retryable,
        },
        FromAgent::ProviderError { kind, .. } => TelemetryEvent::ProviderError { kind: *kind },
        FromAgent::Compaction {
            first_kept_entry_index,
            tokens_before,
            auto,
            ..
        } => TelemetryEvent::Compaction {
            first_kept_entry_index: *first_kept_entry_index,
            tokens_before: *tokens_before,
            auto: *auto,
        },
        _ => return None,
    })
}

fn sanitized_operation_observation(
    observation: &maestro_runtime_contracts::operation_observation::OperationObservation,
) -> CompactOperationObservation {
    use maestro_runtime_contracts::operation_observation::OperationObservation as O;
    match observation {
        O::Admitted {
            turn_id,
            thinking_level,
            experiment,
        } => CompactOperationObservation::Admitted {
            turn_id: InlineLabel::new(turn_id),
            thinking_level: InlineLabel::new(thinking_level),
            experiment: experiment.clone().filter(|value| value.is_valid()),
        },
        O::Prepared {
            response_id,
            model_id,
            model_provider,
            message_count,
            input_size_bytes,
        } => CompactOperationObservation::Prepared {
            response_id: InlineLabel::new(response_id),
            model_id: InlineLabel::new(model_id),
            model_provider: InlineLabel::new(model_provider),
            message_count: *message_count,
            input_size_bytes: *input_size_bytes,
        },
        O::ReasoningUsage {
            response_id,
            tokens,
        } => CompactOperationObservation::ReasoningUsage {
            response_id: InlineLabel::new(response_id),
            tokens: *tokens,
        },
        O::GatewayReceipt {
            response_id,
            request_id,
            record_id,
            lineage_id,
            provider_tools_sha256,
            provider_tool_count,
        } => CompactOperationObservation::GatewayReceipt {
            response_id: InlineLabel::new(response_id),
            request_id: InlineLabel::new(request_id),
            record_id: InlineLabel::new(record_id),
            lineage_id: InlineLabel::new(lineage_id),
            provider_tools_sha256: provider_tools_sha256.as_deref().map(InlineLabel::new),
            provider_tool_count: *provider_tool_count,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_content_becomes_one_counter() {
        let (sender, receiver) = sync_channel(2);
        let relay = TurnTelemetryRelay {
            sender: Some(sender),
            control_sender: None,
            worker: None,
            relay_id: 1,
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(0)),
            next_sequence: 0,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        };
        let mut relay = relay;
        assert!(
            relay
                .project(&FromAgent::ResponseChunk {
                    response_id: "response".into(),
                    content: "secret output".into(),
                    is_thinking: false,
                })
                .is_none()
        );
        assert_eq!(relay.pending_output_bytes, 13);
        relay.submit(None);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn projected_records_cannot_retain_tool_arguments_or_errors() {
        let mut relay = disabled_relay();
        let tool = relay
            .project(&FromAgent::ToolCall {
                call_id: "call-1".into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "super-secret-command"}),
                requires_approval: true,
                approval_inline_env: None,
            })
            .unwrap();
        let error = relay
            .project(&FromAgent::Error {
                message: "super-secret-error".into(),
                fatal: true,
                terminal: true,
                retryable: false,
            })
            .unwrap();
        let projected = format!("{tool:?} {error:?}");
        assert!(!projected.contains("super-secret"));
        assert!(!projected.contains("command"));
    }

    #[test]
    fn labels_are_utf8_safe_and_bounded() {
        let value = "🦀".repeat(INLINE_LABEL_BYTES);
        let bounded = InlineLabel::new(&value);
        assert!(bounded.as_str().len() <= INLINE_LABEL_BYTES);
        assert!(bounded.as_str().is_char_boundary(bounded.as_str().len()));
        assert!(!bounded.as_str().is_empty());
    }

    #[test]
    fn full_queue_is_nonblocking_and_counted() {
        let (sender, _receiver) = sync_channel(1);
        sender
            .try_send(HubMessage::Observe {
                relay_id: 1,
                queued: QueuedObservation {
                    sequence: 0,
                    observation: TurnObservation::Output {
                        bytes: 1,
                        first_observed_at: std::time::Instant::now(),
                    },
                },
            })
            .unwrap();
        let relay = TurnTelemetryRelay {
            sender: Some(sender),
            control_sender: None,
            worker: None,
            relay_id: 1,
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(1)),
            next_sequence: 1,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        };
        relay.record_drop(TrySendError::Full(HubMessage::Observe {
            relay_id: 1,
            queued: QueuedObservation {
                sequence: 1,
                observation: TurnObservation::Output {
                    bytes: 2,
                    first_observed_at: std::time::Instant::now(),
                },
            },
        }));
        assert_eq!(relay.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn disconnected_worker_is_counted_as_telemetry_loss() {
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        let mut relay = TurnTelemetryRelay {
            sender: Some(sender),
            control_sender: None,
            worker: None,
            relay_id: 1,
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(0)),
            next_sequence: 0,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        };
        relay.try_submit(TurnObservation::Event(TelemetryEvent::TurnStarted));
        assert_eq!(relay.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn queue_storage_stays_small() {
        let record_bytes = std::mem::size_of::<HubMessage>();
        let queue_bytes = record_bytes * QUEUE_CAPACITY;
        eprintln!("telemetry record: {record_bytes} bytes; queue: {queue_bytes} bytes");
        assert!(
            queue_bytes <= 128 * 1024,
            "the process-wide observation queue must stay within 128 KiB"
        );
    }

    #[test]
    fn every_relay_uses_the_single_process_worker() {
        let before = worker_start_count_for_test();
        let first = test_relay();
        let second = test_relay();
        assert_eq!(worker_start_count_for_test(), before.max(1));
        drop((first, second));
    }

    #[test]
    fn registration_is_applied_before_the_first_observation() {
        let (sender, receiver) = sync_channel(1);
        let (control_sender, control_receiver) = channel();
        control_sender
            .send(ControlMessage::Register(Box::new(RelayRegistration {
                relay_id: 41,
                config: test_config(),
                context: TurnTrackerContext::default(),
                identity_scope: Arc::new(RwLock::new(None)),
                telemetry_host: None,
            })))
            .unwrap();
        sender
            .send(HubMessage::Observe {
                relay_id: 41,
                queued: QueuedObservation {
                    sequence: 0,
                    observation: TurnObservation::Event(TelemetryEvent::TurnStarted),
                },
            })
            .unwrap();

        let mut states = HashMap::new();
        let mut journal = None;
        assert_eq!(
            drain_hub_once(&receiver, &control_receiver, &mut states, &mut journal).observations,
            1
        );

        assert_eq!(states.get(&41).unwrap().tracker.turn_number(), 1);
    }

    #[test]
    fn unregister_drains_the_final_observation_before_removing_state() {
        let (sender, receiver) = sync_channel(1);
        let (control_sender, control_receiver) = channel();
        control_sender
            .send(ControlMessage::Register(Box::new(RelayRegistration {
                relay_id: 42,
                config: test_config(),
                context: TurnTrackerContext::default(),
                identity_scope: Arc::new(RwLock::new(None)),
                telemetry_host: None,
            })))
            .unwrap();
        sender
            .send(HubMessage::Observe {
                relay_id: 42,
                queued: QueuedObservation {
                    sequence: 0,
                    observation: TurnObservation::Event(TelemetryEvent::TurnStarted),
                },
            })
            .unwrap();
        control_sender
            .send(ControlMessage::Unregister {
                relay_id: 42,
                attempted: 1,
            })
            .unwrap();

        let mut states = HashMap::new();
        let mut journal = None;
        let report = drain_hub_once(&receiver, &control_receiver, &mut states, &mut journal);

        assert_eq!(report.observations, 1);
        assert_eq!(report.closed, vec![(42, 1)]);
        assert!(!states.contains_key(&42));
    }

    #[test]
    #[ignore = "local performance probe"]
    fn response_chunk_projection_p99_is_sub_microsecond() {
        const BATCH: u32 = 1_024;
        const SAMPLES: usize = 256;
        let mut relay = disabled_relay();
        let event = FromAgent::ResponseChunk {
            response_id: "response".into(),
            content: "one streamed token".into(),
            is_thinking: false,
        };
        let mut nanos_per_event = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = std::time::Instant::now();
            for _ in 0..BATCH {
                std::hint::black_box(relay.project(std::hint::black_box(&event)));
            }
            nanos_per_event.push(started.elapsed().as_nanos() / u128::from(BATCH));
        }
        nanos_per_event.sort_unstable();
        let p99 = nanos_per_event[SAMPLES * 99 / 100];
        eprintln!("response chunk projection p99: {p99} ns");
        assert!(p99 < 1_000, "response chunk projection p99 was {p99} ns");
    }

    #[test]
    #[ignore = "local performance probe"]
    fn observation_submission_p99_is_sub_microsecond() {
        const BATCH: u32 = 128;
        const SAMPLES: usize = 256;
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let mut relay = TurnTelemetryRelay {
            sender: Some(sender),
            control_sender: None,
            worker: Some(std::thread::current()),
            relay_id: 1,
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(0)),
            next_sequence: 0,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        };
        let mut nanos_per_event = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = std::time::Instant::now();
            for _ in 0..BATCH {
                relay.try_submit(TurnObservation::Event(TelemetryEvent::TurnStarted));
            }
            nanos_per_event.push(started.elapsed().as_nanos() / u128::from(BATCH));
            assert_eq!(receiver.try_iter().count(), BATCH as usize);
        }
        nanos_per_event.sort_unstable();
        let p99 = nanos_per_event[SAMPLES * 99 / 100];
        eprintln!("observation submission p99: {p99} ns");
        assert!(p99 < 1_000, "observation submission p99 was {p99} ns");
    }

    #[test]
    fn sequence_gaps_are_attributed_before_the_following_observation() {
        let mut cursor = SequenceCursor::default();
        assert_eq!(cursor.observe(0), 0);
        assert_eq!(cursor.observe(2), 1);
        assert_eq!(cursor.trailing_losses(4), 1);
    }

    fn test_relay() -> TurnTelemetryRelay {
        TurnTelemetryRelay::spawn(
            test_config(),
            TurnTrackerContext::default(),
            Arc::new(RwLock::new(None)),
            None,
        )
    }

    fn test_config() -> TurnTrackerConfig {
        TurnTrackerConfig {
            session_id: String::new(),
            sampling_config: super::super::TailSamplingConfig::default(),
        }
    }

    fn disabled_relay() -> TurnTelemetryRelay {
        TurnTelemetryRelay {
            sender: None,
            control_sender: None,
            worker: None,
            relay_id: 0,
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(0)),
            next_sequence: 0,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        }
    }
}
