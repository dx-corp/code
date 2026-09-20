//! Bounded, content-free handoff from runtime event delivery to turn telemetry.
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
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError, sync_channel},
};

const QUEUE_CAPACITY: usize = 64;
const WORKER_STACK_BYTES: usize = 256 * 1024;
const MAX_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct ToolReceiptObservation {
    call_id: String,
    tool_name: String,
    status: ExecutionStatus,
    duration_ms: Option<u64>,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "boxing would add producer-path allocation; the fixed queue is capped below 64 KiB"
)]
pub(crate) enum TurnObservation {
    Event(FromAgent),
    Output {
        bytes: u64,
        first_observed_at: std::time::Instant,
    },
    SideQuestionEnd {
        side_id: String,
        answer_bytes: u64,
        usage: Option<TokenUsage>,
        failed: bool,
    },
    ToolCall {
        call_id: String,
        tool: String,
        requires_approval: bool,
    },
    ToolEnd {
        call_id: String,
        success: bool,
        receipt: Option<ToolReceiptObservation>,
    },
    Session(Option<String>),
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
                FromAgent::TurnStarted
                    | FromAgent::OperationObservation { .. }
                    | FromAgent::SideQuestionStart { .. }
                    | FromAgent::ResponseStart { .. }
                    | FromAgent::ResponseEnd { .. }
            )
        )
    }

    fn refreshes_identity(&self) -> bool {
        matches!(self, Self::Event(FromAgent::ModelChanged { .. }))
    }

    fn refreshes_session(&self) -> bool {
        matches!(
            self,
            Self::Event(
                FromAgent::TurnStarted
                    | FromAgent::ResponseStart { .. }
                    | FromAgent::SideQuestionStart { .. }
                    | FromAgent::OperationObservation {
                        observation: maestro_runtime_contracts::operation_observation::OperationObservation::Admitted { .. }
                    }
            )
        )
    }
}

/// A tiny producer facade. Its only hot-path state is a byte accumulator and
/// the first-output instant, so token chunks never allocate or occupy queue
/// slots. All aggregation, serialization, locking, disk, and network work is
/// owned by the dedicated worker thread.
pub(crate) struct TurnTelemetryRelay {
    sender: Option<SyncSender<QueuedObservation>>,
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
                dropped: Arc::new(AtomicU64::new(0)),
                attempted: Arc::new(AtomicU64::new(0)),
                next_sequence: 0,
                pending_output_bytes: 0,
                pending_first_output_at: None,
            };
        }

        let (sender, receiver) = sync_channel::<QueuedObservation>(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let attempted = Arc::new(AtomicU64::new(0));
        let worker_attempted = Arc::clone(&attempted);
        let worker = std::thread::Builder::new()
            .name("maestro-telemetry".to_owned())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let mut tracker = TurnTracker::new(config);
                tracker.update_context(context);
                let mut journal = TurnJournal::open();
                let mut sequence_cursor = SequenceCursor::default();
                while let Ok(queued) = receiver.recv() {
                    recover_after_loss(
                        &mut tracker,
                        &mut journal,
                        sequence_cursor.observe(queued.sequence),
                    );
                    let observation = queued.observation;
                    let snapshot = observation.needs_pending_snapshot();
                    if observation.refreshes_identity() {
                        tracker.set_identity_scope(
                            identity_scope
                                .read()
                                .expect("telemetry identity scope lock poisoned")
                                .clone(),
                        );
                    }
                    if observation.refreshes_session() {
                        let session_id = telemetry_host
                            .as_ref()
                            .and_then(|host| futures::executor::block_on(host.hook_session_id()));
                        tracker.set_session_id(session_id.unwrap_or_default());
                    }
                    let completed = handle_observation(&mut tracker, observation);
                    if let Some(completed) = completed.as_ref() {
                        finish(&mut journal, completed);
                    }
                    if snapshot {
                        observe_pending(&mut journal, &tracker);
                    }
                }
                recover_after_loss(
                    &mut tracker,
                    &mut journal,
                    sequence_cursor.trailing_losses(worker_attempted.load(Ordering::Acquire)),
                );
            });

        Self {
            sender: worker.ok().map(|_| sender),
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
                session_id.as_deref().map(bounded_label),
            )),
            FromAgent::ToolCall {
                call_id,
                tool,
                requires_approval,
                ..
            } => Some(TurnObservation::ToolCall {
                call_id: bounded_label(call_id),
                tool: bounded_label(tool),
                requires_approval: *requires_approval,
            }),
            FromAgent::ToolEnd {
                call_id,
                success,
                receipt,
                ..
            } => Some(TurnObservation::ToolEnd {
                call_id: bounded_label(call_id),
                success: *success,
                receipt: receipt.as_ref().map(|receipt| ToolReceiptObservation {
                    call_id: bounded_label(&receipt.call_id),
                    tool_name: bounded_label(&receipt.tool_name),
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
                side_id: bounded_label(side_id),
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
        if let Err(error) = sender.try_send(QueuedObservation {
            sequence,
            observation,
        }) {
            self.record_drop(error);
        }
    }

    fn record_drop(&self, error: TrySendError<QueuedObservation>) {
        if matches!(error, TrySendError::Full(_)) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(crate) fn saturated_for_test() -> (Self, std::sync::mpsc::Receiver<QueuedObservation>) {
        let (sender, receiver) = sync_channel(1);
        sender
            .try_send(QueuedObservation {
                sequence: 0,
                observation: TurnObservation::Output {
                    bytes: 1,
                    first_observed_at: std::time::Instant::now(),
                },
            })
            .expect("test telemetry queue should start empty");
        (
            Self {
                sender: Some(sender),
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
        TurnObservation::Event(event) => tracker.handle_event(&event),
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
        } => tracker.finish_side_question(&side_id, answer_bytes, usage.as_ref(), failed),
        TurnObservation::ToolCall {
            call_id,
            tool,
            requires_approval,
        } => {
            tracker.record_tool_call(&call_id, &tool, None, requires_approval);
            None
        }
        TurnObservation::ToolEnd {
            call_id,
            success,
            receipt,
        } => {
            tracker.record_tool_end(
                &call_id,
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
            tracker.set_session_id(session_id.unwrap_or_default());
            None
        }
    }
}

fn sanitized_event(event: &FromAgent) -> Option<FromAgent> {
    Some(match event {
        FromAgent::Ready { model, provider } => FromAgent::Ready {
            model: bounded_label(model),
            provider: bounded_label(provider),
        },
        FromAgent::ModelChanged { model, provider } => FromAgent::ModelChanged {
            model: bounded_label(model),
            provider: bounded_label(provider),
        },
        FromAgent::BoostChanged { status, thinking } => FromAgent::BoostChanged {
            status: *status,
            thinking: *thinking,
        },
        FromAgent::ResponseStart { response_id } => FromAgent::ResponseStart {
            response_id: bounded_label(response_id),
        },
        FromAgent::ResponseEnd { response_id, usage } => FromAgent::ResponseEnd {
            response_id: bounded_label(response_id),
            usage: usage.clone(),
        },
        FromAgent::TurnCompleted { .. } => FromAgent::TurnCompleted {
            response_id: String::new(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        },
        FromAgent::TurnInterrupted { .. } => FromAgent::TurnInterrupted {
            response_id: String::new(),
            reason: String::new(),
        },
        FromAgent::SideQuestionStart {
            side_id,
            standalone,
            ..
        } => FromAgent::SideQuestionStart {
            side_id: bounded_label(side_id),
            question: String::new(),
            standalone: *standalone,
        },
        FromAgent::CodexUsageState { source, usage } => FromAgent::CodexUsageState {
            source: bounded_label(source),
            usage: usage.clone(),
        },
        FromAgent::StreamObservation { observation } => FromAgent::StreamObservation {
            observation: *observation,
        },
        FromAgent::ContextCalibration { observation } => FromAgent::ContextCalibration {
            observation: maestro_context::context_usage::ContextCalibration {
                request_id: bounded_label(&observation.request_id),
                generation: observation.generation,
                estimated_input_tokens: observation.estimated_input_tokens,
                observed_input_tokens: observation.observed_input_tokens,
            },
        },
        FromAgent::RequestRetryObservation => FromAgent::RequestRetryObservation,
        FromAgent::RequestContextPrepared { response_id } => FromAgent::RequestContextPrepared {
            response_id: bounded_label(response_id),
        },
        FromAgent::OperationObservation { observation } => FromAgent::OperationObservation {
            observation: sanitized_operation_observation(observation),
        },
        FromAgent::TurnStarted => FromAgent::TurnStarted,
        FromAgent::RequestRetryScheduled {
            attempt,
            delay_ms,
            rate_limited,
        } => FromAgent::RequestRetryScheduled {
            attempt: *attempt,
            delay_ms: *delay_ms,
            rate_limited: *rate_limited,
        },
        FromAgent::CompactionMeasured { duration_ms } => FromAgent::CompactionMeasured {
            duration_ms: *duration_ms,
        },
        FromAgent::ToolStart { call_id } => FromAgent::ToolStart {
            call_id: bounded_label(call_id),
        },
        FromAgent::Error {
            fatal,
            terminal,
            retryable,
            ..
        } => FromAgent::Error {
            message: String::new(),
            fatal: *fatal,
            terminal: *terminal,
            retryable: *retryable,
        },
        FromAgent::ProviderError { kind, .. } => FromAgent::ProviderError {
            kind: *kind,
            message: String::new(),
        },
        FromAgent::Compaction {
            first_kept_entry_index,
            tokens_before,
            auto,
            ..
        } => FromAgent::Compaction {
            summary: String::new(),
            first_kept_entry_index: *first_kept_entry_index,
            tokens_before: *tokens_before,
            auto: *auto,
            custom_instructions: None,
            continuation: None,
            timestamp: String::new(),
        },
        _ => return None,
    })
}

fn sanitized_operation_observation(
    observation: &maestro_runtime_contracts::operation_observation::OperationObservation,
) -> maestro_runtime_contracts::operation_observation::OperationObservation {
    use maestro_runtime_contracts::operation_observation::OperationObservation as O;
    match observation {
        O::Admitted {
            turn_id,
            thinking_level,
            experiment,
        } => O::Admitted {
            turn_id: bounded_label(turn_id),
            thinking_level: bounded_label(thinking_level),
            experiment: experiment.clone().filter(|value| value.is_valid()),
        },
        O::Prepared {
            response_id,
            model_id,
            model_provider,
            message_count,
            input_size_bytes,
        } => O::Prepared {
            response_id: bounded_label(response_id),
            model_id: bounded_label(model_id),
            model_provider: bounded_label(model_provider),
            message_count: *message_count,
            input_size_bytes: *input_size_bytes,
        },
        O::ReasoningUsage {
            response_id,
            tokens,
        } => O::ReasoningUsage {
            response_id: bounded_label(response_id),
            tokens: *tokens,
        },
        O::GatewayReceipt {
            response_id,
            request_id,
            record_id,
            lineage_id,
            provider_tools_sha256,
            provider_tool_count,
        } => O::GatewayReceipt {
            response_id: bounded_label(response_id),
            request_id: bounded_label(request_id),
            record_id: bounded_label(record_id),
            lineage_id: bounded_label(lineage_id),
            provider_tools_sha256: provider_tools_sha256.as_deref().map(bounded_label),
            provider_tool_count: *provider_tool_count,
        },
    }
}

fn bounded_label(value: &str) -> String {
    if value.len() <= MAX_LABEL_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_LABEL_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_content_becomes_one_counter() {
        let (sender, receiver) = sync_channel(2);
        let relay = TurnTelemetryRelay {
            sender: Some(sender),
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
    fn labels_are_utf8_safe_and_bounded() {
        let value = "🦀".repeat(MAX_LABEL_BYTES);
        let bounded = bounded_label(&value);
        assert!(bounded.len() <= MAX_LABEL_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[test]
    fn full_queue_is_nonblocking_and_counted() {
        let (sender, _receiver) = sync_channel(1);
        sender
            .try_send(QueuedObservation {
                sequence: 0,
                observation: TurnObservation::Output {
                    bytes: 1,
                    first_observed_at: std::time::Instant::now(),
                },
            })
            .unwrap();
        let relay = TurnTelemetryRelay {
            sender: Some(sender),
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(1)),
            next_sequence: 1,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        };
        relay.record_drop(TrySendError::Full(QueuedObservation {
            sequence: 1,
            observation: TurnObservation::Output {
                bytes: 2,
                first_observed_at: std::time::Instant::now(),
            },
        }));
        assert_eq!(relay.dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn queue_storage_stays_small() {
        assert!(
            std::mem::size_of::<QueuedObservation>() * QUEUE_CAPACITY <= 64 * 1024,
            "the preallocated observation queue must stay within 64 KiB"
        );
    }

    #[test]
    #[ignore = "local performance probe"]
    fn response_chunk_projection_p99_is_sub_microsecond() {
        const BATCH: u32 = 1_024;
        const SAMPLES: usize = 256;
        let mut relay = TurnTelemetryRelay {
            sender: None,
            dropped: Arc::new(AtomicU64::new(0)),
            attempted: Arc::new(AtomicU64::new(0)),
            next_sequence: 0,
            pending_output_bytes: 0,
            pending_first_output_at: None,
        };
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
    fn sequence_gaps_are_attributed_before_the_following_observation() {
        let mut cursor = SequenceCursor::default();
        assert_eq!(cursor.observe(0), 0);
        assert_eq!(cursor.observe(2), 1);
        assert_eq!(cursor.trailing_losses(4), 1);
    }
}
