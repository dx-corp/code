use super::manifests::*;
use super::*;
use maestro_runtime_contracts::tool_wire;

const MAX_UNVERIFIED_PENDING_CONTROLLER_EVENTS: usize = 1024;
const INVALID_RUNTIME_RECEIPT_IDENTITY: &str = "invalid_runtime_receipt_identity";
type LoadedTerminalReceipt = (
    RuntimeReceiptKind,
    Option<RuntimeTerminalClassification>,
    Option<String>,
    Option<String>,
);

fn drain_message_replay_key(message: &FromAgentMessage) -> Option<Vec<u8>> {
    let candidate = crate::transcript::redact_agent_message(message.clone());
    serde_json::to_vec(&candidate).ok()
}

fn drain_replay_message_counts(state: &RunnerState, replay_cursor: u64) -> HashMap<Vec<u8>, usize> {
    state
        .envelopes
        .iter()
        .filter_map(|envelope| {
            let StreamEnvelope::Message {
                cursor,
                message: existing,
                ..
            } = envelope
            else {
                return None;
            };
            if *cursor <= replay_cursor {
                return None;
            }
            drain_message_replay_key(existing)
        })
        .fold(HashMap::new(), |mut counts, key| {
            *counts.entry(key).or_default() += 1;
            counts
        })
}

/// Keep only a complete, bounded producer binding in durable runtime state.
/// A rejected live Ready must not be recoverable from a snapshot, journal
/// metadata field, or replayed Ready envelope on the next process start.
fn accepted_runtime_binding(
    model: Option<String>,
    provider: Option<String>,
) -> (Option<String>, Option<String>) {
    match (model, provider) {
        (Some(model), Some(provider))
            if HostedRunnerConfig::validate_live_runtime_receipt_binding(&model, &provider)
                .is_ok() =>
        {
            (Some(model), Some(provider))
        }
        (None, None) => (None, None),
        _ => (None, None),
    }
}

struct LifecycleRollback {
    cursor: u64,
    ready: bool,
    runtime_failed: bool,
    restore_incomplete: bool,
    last_status: Option<String>,
    last_error: Option<String>,
    last_error_type: Option<String>,
    provider_error_kind: Option<maestro_ai::ProviderStreamErrorKind>,
    runtime_model_binding: Option<String>,
    runtime_provider_binding: Option<String>,
    active_response_ids: HashSet<String>,
    pending_controller_events: VecDeque<FromAgentMessage>,
    thread: ThreadProtocolState,
    envelopes: VecDeque<StreamEnvelope>,
    controller_envelopes: VecDeque<StreamEnvelope>,
}

impl LifecycleRollback {
    fn capture(state: &RunnerState) -> Self {
        Self {
            cursor: state.cursor,
            ready: state.ready,
            runtime_failed: state.runtime_failed,
            restore_incomplete: state.restore_incomplete,
            last_status: state.last_status.clone(),
            last_error: state.last_error.clone(),
            last_error_type: state.last_error_type.clone(),
            provider_error_kind: state.provider_error_kind,
            runtime_model_binding: state.runtime_model_binding.clone(),
            runtime_provider_binding: state.runtime_provider_binding.clone(),
            active_response_ids: state.active_response_ids.clone(),
            pending_controller_events: state.pending_controller_events.clone(),
            thread: state.thread.clone(),
            envelopes: state.envelopes.clone(),
            controller_envelopes: state.controller_envelopes.clone(),
        }
    }

    fn restore(self, state: &mut RunnerState) {
        state.cursor = self.cursor;
        state.ready = self.ready;
        state.runtime_failed = self.runtime_failed;
        state.restore_incomplete = self.restore_incomplete;
        state.last_status = self.last_status;
        state.last_error = self.last_error;
        state.last_error_type = self.last_error_type;
        state.provider_error_kind = self.provider_error_kind;
        state.runtime_model_binding = self.runtime_model_binding;
        state.runtime_provider_binding = self.runtime_provider_binding;
        state.active_response_ids = self.active_response_ids;
        state.pending_controller_events = self.pending_controller_events;
        state.thread = self.thread;
        state.envelopes = self.envelopes;
        state.controller_envelopes = self.controller_envelopes;
    }
}

/// Recover the producer binding from older journals that predate the
/// explicit model/provider fields. Durable Ready messages and snapshots are
/// the only replay evidence used; a newly constructed executor is not an
/// authoritative replacement for that binding.
fn loaded_runtime_binding(loaded_thread: &LoadedThreadJournal) -> (Option<String>, Option<String>) {
    let mut model = loaded_thread.persisted_runtime_model.clone();
    let mut provider = loaded_thread.persisted_runtime_provider.clone();
    for envelope in loaded_thread.events.iter().rev() {
        match envelope {
            StreamEnvelope::Message { message, .. } => {
                if let FromAgentMessage::Ready {
                    model: ready_model,
                    provider: ready_provider,
                    ..
                } = message.as_ref()
                {
                    model.get_or_insert_with(|| ready_model.clone());
                    provider.get_or_insert_with(|| ready_provider.clone());
                }
            }
            StreamEnvelope::Snapshot { snapshot } | StreamEnvelope::Reset { snapshot, .. } => {
                if model.is_none() {
                    model = snapshot.state.model.clone();
                }
                if provider.is_none() {
                    provider = snapshot.state.provider.clone();
                }
            }
            StreamEnvelope::Heartbeat { .. } => {}
        }
        if model.is_some() && provider.is_some() {
            break;
        }
    }
    accepted_runtime_binding(model, provider)
}

/// Recover the last durable terminal boundary when a process restarts against
/// an existing journal without a new snapshot restore. A runtime generation
/// is not reusable for a fresh execution-ready receipt after its journal has
/// already reached a terminal phase.
fn loaded_terminal_receipt(
    loaded_thread: &LoadedThreadJournal,
    runtime_generation: u64,
) -> Option<LoadedTerminalReceipt> {
    if loaded_thread.persisted_runtime_generation != Some(runtime_generation) {
        return None;
    }

    // Drain boundaries are generation-level state, not turn terminals. In
    // particular, an idle runtime can have no latest turn at all. The
    // draining snapshot is persisted before executor ownership is released;
    // the final drained snapshot is persisted after that hand-off completes.
    for envelope in loaded_thread.events.iter().rev() {
        match envelope {
            StreamEnvelope::Snapshot { snapshot } | StreamEnvelope::Reset { snapshot, .. } => {
                if snapshot.state.last_status.as_deref() == Some("Drained") {
                    return Some((
                        RuntimeReceiptKind::Drained,
                        None,
                        None,
                        loaded_thread.state.snapshot_lineage().map(str::to_string),
                    ));
                }
                if snapshot.state.last_status.as_deref()
                    == Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS)
                {
                    return Some((RuntimeReceiptKind::Draining, None, None, None));
                }
                if snapshot.state.last_status.as_deref() == Some("Draining") {
                    return Some((RuntimeReceiptKind::Draining, None, None, None));
                }
                if snapshot.state.last_status.as_deref() == Some("Runtime failed") {
                    return Some((
                        RuntimeReceiptKind::Failed,
                        None,
                        Some(
                            snapshot
                                .state
                                .last_error_type
                                .clone()
                                .or_else(|| {
                                    loaded_thread
                                        .state
                                        .runtime_failure_type()
                                        .map(str::to_string)
                                })
                                .unwrap_or_else(|| "fatal".to_string()),
                        ),
                        None,
                    ));
                }
            }
            StreamEnvelope::Message { message, .. } => {
                if matches!(
                    message.as_ref(),
                    FromAgentMessage::Error { fatal: true, .. }
                ) {
                    return Some((
                        RuntimeReceiptKind::Failed,
                        None,
                        Some(
                            loaded_thread
                                .state
                                .runtime_failure_type()
                                .map(str::to_string)
                                .unwrap_or_else(|| "fatal".to_string()),
                        ),
                        None,
                    ));
                }
            }
            StreamEnvelope::Heartbeat { .. } => {}
        }
    }

    for turn in loaded_thread
        .state
        .turns_for_generation(runtime_generation)
        .rev()
    {
        if let Some(classification) = turn.terminal_classification {
            return Some(match classification {
                RuntimeTerminalClassification::Fatal => (
                    RuntimeReceiptKind::Failed,
                    None,
                    Some(
                        loaded_thread
                            .state
                            .runtime_failure_type()
                            .map(str::to_string)
                            .unwrap_or_else(|| "fatal".to_string()),
                    ),
                    None,
                ),
                classification => (
                    RuntimeReceiptKind::Terminal,
                    Some(classification),
                    None,
                    None,
                ),
            });
        }
        match turn.phase {
            ThreadPhase::Completed => {
                return Some((
                    RuntimeReceiptKind::Terminal,
                    Some(RuntimeTerminalClassification::Completed),
                    None,
                    None,
                ));
            }
            ThreadPhase::Interrupted => {
                return Some((
                    RuntimeReceiptKind::Terminal,
                    Some(RuntimeTerminalClassification::Interrupted),
                    None,
                    None,
                ));
            }
            ThreadPhase::Failed if turn.provider_error_kind.is_some() => {
                return Some((
                    RuntimeReceiptKind::Terminal,
                    Some(RuntimeTerminalClassification::ProviderFailed),
                    None,
                    None,
                ));
            }
            // A failed steer does not replace the active user turn, so keep
            // scanning for that interrupted run. A failed user turn is the
            // newest independent execution attempt; stop here so restart
            // recovery cannot resurrect an older terminal receipt for it.
            ThreadPhase::Failed if turn.kind == ThreadTurnKind::Steer => continue,
            ThreadPhase::Failed => return None,
            _ => continue,
        }
    }
    None
}

/// Recover the persisted executor-complete drain phase separately from the
/// public Draining receipt. The marker is written after `drain()` succeeds and
/// before manifest/final-journal work begins, so a restart can resume
/// finalization without invoking the executor a second time.
fn loaded_drain_finalization_pending(
    loaded_thread: &LoadedThreadJournal,
    runtime_generation: u64,
) -> bool {
    if loaded_thread.persisted_runtime_generation != Some(runtime_generation) {
        return false;
    }
    for envelope in loaded_thread.events.iter().rev() {
        match envelope {
            StreamEnvelope::Snapshot { snapshot } | StreamEnvelope::Reset { snapshot, .. } => {
                match snapshot.state.last_status.as_deref() {
                    Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS) => return true,
                    Some("Draining" | "Drained" | "Runtime failed") => return false,
                    _ => {}
                }
            }
            StreamEnvelope::Message { .. } | StreamEnvelope::Heartbeat { .. } => {}
        }
    }
    false
}

impl SharedRunner {
    #[cfg(test)]
    pub(super) fn new(config: HostedRunnerConfig) -> Self {
        Self::new_with_message_executor_and_restore(
            config,
            Arc::new(TransportOnlyHostedRunnerMessageExecutor),
            None,
        )
    }

    #[cfg(test)]
    pub(super) fn new_with_message_executor_and_restore(
        config: HostedRunnerConfig,
        message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
        restore_manifest: Option<SnapshotManifest>,
    ) -> Self {
        Self::try_new_with_message_executor_and_restore(config, message_executor, restore_manifest)
            .expect("hosted runner fixture should load its durable thread journal")
    }

    pub(super) fn try_new_with_message_executor_and_restore(
        config: HostedRunnerConfig,
        message_executor: Arc<dyn HostedRunnerHeadlessMessageExecutor>,
        restore_manifest: Option<SnapshotManifest>,
    ) -> io::Result<Self> {
        validate_message_executor_startup_runtime_receipt_binding(message_executor.as_ref())?;
        let binding = HostedRunnerBinding::from_config(&config, restore_manifest.as_ref());
        let mut loaded_thread = ThreadJournal::load(
            &config.workspace_root,
            binding.maestro_session_id.as_str(),
            config.runtime_generation,
        )?;
        let replacement_runner_ownership_handoff =
            restore_manifest.as_ref().is_some_and(|manifest| {
                manifest.runner_session_id != config.runner_session_id
                    && loaded_thread.persisted_runner_session_id.as_deref()
                        != Some(config.runner_session_id.as_str())
            });
        if replacement_runner_ownership_handoff {
            // A same-generation restore is a new Runner Host owner. The
            // source journal may contain Drained/Failed/turn-terminal
            // envelopes that must not be replayed after the replacement's
            // Restored reset boundary. Keep the restore manifest as the
            // authoritative input and persist B's ownership below. The
            // private drain handoff is also source-owner evidence; delete it
            // before B can persist its new ownership so a later B restart
            // cannot reload A's consumed executor batch.
            clear_executor_drain_result(
                &config.workspace_root,
                binding.maestro_session_id.as_str(),
                config.runtime_generation,
            )?;
            loaded_thread.state.reset_for_replacement();
            loaded_thread.events.clear();
            loaded_thread.pending_executor_drain_result = None;
        }
        // A restore manifest is an input snapshot, not newer lifecycle
        // evidence when it belongs to this same Runner Host identity. Prefer
        // that current-generation journal boundary so a restart cannot
        // republish Restored after the journal has durably reached Drained,
        // Failed, or a turn terminal. A replacement runner session is a new
        // owner of the restored input, so it must not inherit terminal
        // evidence written by the source runner's journal.
        let journal_belongs_to_current_runner = match restore_manifest.as_ref() {
            None => true,
            Some(manifest) if manifest.runner_session_id == config.runner_session_id => true,
            Some(_) => {
                !replacement_runner_ownership_handoff
                    && loaded_thread.persisted_runner_session_id.as_deref()
                        == Some(config.runner_session_id.as_str())
            }
        };
        let startup_terminal_receipt = journal_belongs_to_current_runner
            .then(|| loaded_terminal_receipt(&loaded_thread, config.runtime_generation))
            .flatten();
        let startup_drained = startup_terminal_receipt
            .as_ref()
            .is_some_and(|(kind, _, _, _)| *kind == RuntimeReceiptKind::Drained);
        let startup_draining = startup_terminal_receipt
            .as_ref()
            .is_some_and(|(kind, _, _, _)| *kind == RuntimeReceiptKind::Draining);
        let pending_executor_drain_result = (journal_belongs_to_current_runner && !startup_drained)
            .then_some(loaded_thread.pending_executor_drain_result.clone())
            .flatten();
        let executor_drain_result_applied_count = pending_executor_drain_result
            .as_ref()
            .map(|(_, result)| {
                loaded_thread
                    .executor_drain_result_applied_count
                    .min(result.messages.len())
            })
            .unwrap_or_default();
        let executor_drain_result_applied =
            pending_executor_drain_result
                .as_ref()
                .is_some_and(|(_, result)| {
                    loaded_thread.executor_drain_result_applied_count >= result.messages.len()
                });
        let startup_drain_finalization_pending = journal_belongs_to_current_runner
            && (loaded_drain_finalization_pending(&loaded_thread, config.runtime_generation)
                || pending_executor_drain_result.is_some());
        let startup_drain_runtime_failed_before_finalization = journal_belongs_to_current_runner
            && startup_drain_finalization_pending
            && loaded_thread.persisted_drain_runtime_failed_before_finalization;
        let startup_failed = startup_terminal_receipt
            .as_ref()
            .is_some_and(|(kind, _, _, _)| *kind == RuntimeReceiptKind::Failed);
        let (events, _) = broadcast::channel(MAX_EVENTS);
        let (controller_events, _) = broadcast::channel(MAX_EVENTS);
        let restored_snapshot = restore_manifest
            .as_ref()
            .map(|manifest| manifest.snapshot.clone());
        let restored_snapshot_lineage = restore_manifest
            .as_ref()
            .map(|manifest| snapshot_lineage_from_created_at(&manifest.created_at))
            .transpose()
            .map_err(hosted_error_to_io)?;
        let (replayed_model, replayed_provider) = loaded_runtime_binding(&loaded_thread);
        let (restored_model, restored_provider) = restored_snapshot
            .as_ref()
            .map(|snapshot| {
                accepted_runtime_binding(
                    snapshot.state.model.clone(),
                    snapshot.state.provider.clone(),
                )
            })
            .unwrap_or((None, None));
        let restored_cursor = restore_manifest
            .as_ref()
            .and_then(|manifest| manifest.runtime.cursor)
            .or_else(|| restored_snapshot.as_ref().map(|snapshot| snapshot.cursor));
        let last_init = restored_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.last_init.as_ref())
            .and_then(RuntimeInitSnapshot::to_init_config)
            .or_else(|| loaded_thread.last_init.clone());
        let restore_status = restore_manifest
            .as_ref()
            .map(|manifest| manifest.runtime.flush_status);
        let restore_runtime_failed = restored_snapshot
            .as_ref()
            .is_some_and(runtime_snapshot_is_failed);
        let restore_ready = restore_status
            .map(RuntimeFlushStatus::is_completed)
            .unwrap_or(true)
            && !restore_runtime_failed;
        let restore_failed = restore_manifest.is_some() && !restore_ready;
        let restore_last_error = restore_manifest.as_ref().and_then(|manifest| {
            manifest
                .runtime
                .flush_status
                .restore_last_error(manifest.runtime.error.as_deref())
        });
        let restore_last_error_type = if restore_runtime_failed {
            restored_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.state.last_error_type.clone())
                .or_else(|| {
                    loaded_thread
                        .state
                        .runtime_failure_type()
                        .map(str::to_string)
                })
                .or_else(|| Some("fatal".to_string()))
        } else {
            restore_last_error.as_ref().map(|_| "protocol".to_string())
        };
        let restore_failure_error_type = if restore_failed {
            if restore_runtime_failed {
                restore_last_error_type.clone()
            } else {
                Some("restore_incomplete".to_string())
            }
        } else {
            None
        };
        let shared = Self {
            binding: binding.clone(),
            config: Arc::new(config),
            state: Arc::new(Mutex::new(RunnerState {
                ready: restore_ready && !startup_drained && !startup_draining && !startup_failed,
                draining: startup_drained || startup_draining,
                drain_finalization_pending: startup_drain_finalization_pending,
                drain_runtime_failed_before_finalization:
                    startup_drain_runtime_failed_before_finalization,
                // A persisted Draining boundary means executor ownership was
                // not durably handed off yet. The next request may retry it;
                // `handle_drain` stops the newly started pump before doing so.
                drain_executor_pending: startup_draining && !startup_drain_finalization_pending,
                pending_executor_drain_result: pending_executor_drain_result.map(
                    |(replay_cursor, result)| PendingExecutorDrainResult {
                        replay_cursor,
                        result,
                    },
                ),
                executor_drain_result_applied_count,
                executor_drain_result_applied,
                runtime_failed: restore_runtime_failed
                    || startup_failed
                    || startup_drain_runtime_failed_before_finalization,
                restore_incomplete: restore_failed,
                session_id: binding.maestro_session_id.as_str().to_string(),
                cursor: restored_cursor.unwrap_or(0).max(loaded_thread.cursor),
                last_init,
                last_status: Some(
                    if startup_drained {
                        "Drained"
                    } else if startup_failed {
                        "Runtime failed"
                    } else if startup_drain_finalization_pending {
                        HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS
                    } else if startup_draining {
                        "Draining"
                    } else {
                        restore_status
                            .map(RuntimeFlushStatus::restore_last_status)
                            .unwrap_or("Ready")
                    }
                    .to_string(),
                ),
                last_error: restore_last_error,
                last_error_type: startup_terminal_receipt
                    .as_ref()
                    .and_then(|(_, _, error_type, _)| error_type.clone())
                    .or(restore_last_error_type),
                provider_error_kind: restored_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.state.provider_error_kind),
                identity_binding_failures: loaded_thread.identity_binding_failures,
                restored_snapshot: restored_snapshot.clone(),
                restored_snapshot_lineage,
                runtime_model_binding: replayed_model.or(restored_model),
                runtime_provider_binding: replayed_provider.or(restored_provider),
                runtime_receipt: None,
                controller_binding: None,
                controller_connection_id: None,
                controller_stream_cancellation: CancellationToken::new(),
                connections: HashMap::new(),
                subscriptions: HashMap::new(),
                active_utility_commands: HashMap::new(),
                active_file_watches: HashMap::new(),
                active_response_ids: HashSet::new(),
                response_idempotency_keys: loaded_thread.response_idempotency_keys,
                response_idempotency_digests: loaded_thread.response_idempotency_digests,
                response_request_owners: loaded_thread.response_request_owners,
                pending_response_idempotency: loaded_thread.pending_response_idempotency,
                response_idempotency_order: loaded_thread.response_idempotency_order,
                pending_response_idempotency_order: loaded_thread
                    .pending_response_idempotency_order,
                envelopes: loaded_thread.events.clone(),
                controller_envelopes: loaded_thread.events,
                pending_controller_events: VecDeque::new(),
                thread: loaded_thread.state,
            })),
            events,
            controller_events,
            message_executor,
            rendezvous_outbound_authority: Arc::new(AtomicBool::new(false)),
            thread_journal: Arc::new(loaded_thread.journal),
            mutation_lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            thread_persistence_retry_pending: Arc::new(AtomicBool::new(false)),
            thread_persistence_recovery_running: Arc::new(AtomicBool::new(false)),
            thread_persistence_recovery_cancellation: CancellationToken::new(),
            thread_persistence_recovery_task: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            thread_persistence_failures: Arc::new(Mutex::new(0)),
            #[cfg(test)]
            executor_drain_result_persistence_failures: Arc::new(Mutex::new(0)),
            event_pump_cancellation: CancellationToken::new(),
            event_pump_task: Arc::new(tokio::sync::Mutex::new(None)),
        };
        if restore_manifest.is_some() {
            let envelope = shared.reset_envelope("restored_from_snapshot");
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.envelopes.push_back(envelope.clone());
            state.controller_envelopes.push_back(envelope);
            if restore_failed {
                // Keep the human-facing restore status on the live state, but
                // append a distinct failed lifecycle snapshot to the journal.
                // A restart without the consumed manifest must recover this
                // fence instead of treating the earlier restore reset as Ready.
                let mut failure_snapshot = shared.public_snapshot(&state);
                failure_snapshot.state.last_status = Some("Runtime failed".to_string());
                failure_snapshot.state.last_error_type = restore_failure_error_type.clone();
                let failure_envelope = StreamEnvelope::Snapshot {
                    snapshot: failure_snapshot,
                };
                state.envelopes.push_back(failure_envelope.clone());
                state.controller_envelopes.push_back(failure_envelope);
            }
        }
        {
            let state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Startup: failing construction is the correct response to an
            // unwritable journal, so the raw call is intentional here.
            #[allow(clippy::disallowed_methods)]
            shared.persist_thread(&state)?;
        }
        {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (receipt_kind, receipt_terminal, receipt_error_type, receipt_lineage) =
                if let Some(receipt) = startup_terminal_receipt.clone() {
                    receipt
                } else {
                    match restore_manifest.as_ref() {
                        Some(_) if restore_ready => {
                            (RuntimeReceiptKind::Restored, None, None, None)
                        }
                        Some(_) if restore_runtime_failed => (
                            RuntimeReceiptKind::Failed,
                            None,
                            Some(
                                state
                                    .last_error_type
                                    .clone()
                                    .unwrap_or_else(|| "fatal".to_string()),
                            ),
                            None,
                        ),
                        Some(_) => (
                            RuntimeReceiptKind::Failed,
                            None,
                            Some("restore_incomplete".to_string()),
                            None,
                        ),
                        None => (RuntimeReceiptKind::Ready, None, None, None),
                    }
                };
            shared.refresh_runtime_receipt(
                &mut state,
                receipt_kind,
                receipt_terminal,
                receipt_error_type.as_deref(),
                receipt_lineage,
            );
        }
        Ok(shared)
    }

    pub(super) fn set_rendezvous_outbound_authority(&self, enabled: bool) {
        self.rendezvous_outbound_authority
            .store(enabled, Ordering::Release);
    }

    pub(super) fn inbound_commands_enabled(&self) -> bool {
        !self.rendezvous_outbound_authority.load(Ordering::Acquire)
    }

    pub(super) fn identity(&self) -> HostedRunnerIdentity {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HostedRunnerIdentity {
            protocol_version: HOSTED_RUNNER_IDENTITY_PROTOCOL_VERSION.to_string(),
            runner_session_id: self.binding.runner_session_id.as_str().to_string(),
            owner_instance_id: self.config.owner_instance_id.clone(),
            ready: state.ready,
            draining: state.draining,
        }
    }

    pub(super) fn ensure_attachable(&self) -> HostedResult<()> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.ready || state.draining {
            return Err(runtime_availability_error(
                &state,
                "hosted runner is not accepting new attachments",
            ));
        }
        Ok(())
    }

    pub(super) fn snapshot(&self, state: &RunnerState) -> RuntimeSnapshot {
        let agent_state = self.message_executor.state().ok().flatten();
        let agent_state = agent_state.as_ref();
        let restored_state = state
            .restored_snapshot
            .as_ref()
            .map(|snapshot| &snapshot.state);
        let restored_pending_state = state
            .restored_snapshot
            .as_ref()
            .filter(|snapshot| state.cursor <= snapshot.cursor)
            .map(|snapshot| &snapshot.state);
        let prefer_restored_host_state = state.restored_snapshot.is_some();
        let host_ready = state.ready && !state.draining;
        let controller_subscription_id = state
            .controller_connection_id
            .as_ref()
            .and_then(|connection_id| state.connections.get(connection_id))
            .and_then(|connection| {
                connection
                    .subscription_ids
                    .iter()
                    .filter(|subscription_id| {
                        state
                            .subscriptions
                            .get(*subscription_id)
                            .map(|subscription| subscription.role == ConnectionRole::Controller)
                            .unwrap_or(false)
                    })
                    .min()
                    .cloned()
            });
        let preferred_connection = state
            .controller_connection_id
            .as_ref()
            .and_then(|connection_id| state.connections.get(connection_id))
            .or_else(|| state.connections.values().next());
        let connections = state
            .connections
            .values()
            .map(|connection| {
                let attached_subscription_count = connection
                    .subscription_ids
                    .iter()
                    .filter(|subscription_id| {
                        state
                            .subscriptions
                            .get(*subscription_id)
                            .map(|subscription| subscription.attached)
                            .unwrap_or(false)
                    })
                    .count();
                ConnectionState {
                    connection_id: connection.id.clone(),
                    role: connection.role,
                    client_protocol_version: connection.client_protocol_version.clone(),
                    client_info: connection.client_info.clone(),
                    capabilities: connection.capabilities.clone(),
                    opt_out_notifications: (!connection.opt_out_notifications.is_empty())
                        .then(|| connection.opt_out_notifications.clone()),
                    subscription_count: connection.subscription_ids.len(),
                    attached_subscription_count,
                    controller_lease_granted: state.controller_connection_id.as_deref()
                        == Some(connection.id.as_str()),
                    lease_expires_at: Some(lease_expires_at(connection)),
                }
            })
            .collect();
        let protocol_version = agent_state
            .and_then(|state| state.protocol_version.clone())
            .or_else(|| restored_state.and_then(|state| state.protocol_version.clone()))
            .unwrap_or_else(|| HEADLESS_PROTOCOL_VERSION.to_string());
        let git_branch = agent_state
            .and_then(|state| state.git_branch.clone())
            .or_else(|| restored_state.and_then(|state| state.git_branch.clone()))
            .or_else(|| crate::git::current_branch(&self.config.workspace_root));

        RuntimeSnapshot {
            protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
            session_id: state.session_id.clone(),
            cursor: state.cursor,
            last_init: state.last_init.as_ref().map(RuntimeInitSnapshot::from),
            state: RuntimeStateSnapshot {
                protocol_version: Some(protocol_version),
                client_protocol_version: preferred_connection
                    .and_then(|connection| connection.client_protocol_version.clone())
                    .or_else(|| agent_state.and_then(|state| state.client_protocol_version.clone()))
                    .or_else(|| {
                        restored_state.and_then(|state| state.client_protocol_version.clone())
                    }),
                client_info: preferred_connection
                    .and_then(|connection| connection.client_info.clone())
                    .or_else(|| agent_state.and_then(|state| state.client_info.clone()))
                    .or_else(|| restored_state.and_then(|state| state.client_info.clone())),
                capabilities: preferred_connection
                    .and_then(|connection| connection.capabilities.clone())
                    .or_else(|| agent_state.and_then(|state| state.capabilities.clone()))
                    .or_else(|| restored_state.and_then(|state| state.capabilities.clone())),
                server_capabilities: agent_state.map_or_else(
                    || {
                        restored_state
                            .and_then(|state| state.server_capabilities.clone())
                            .map(server_capabilities_without_governed_grants)
                            .or_else(|| Some(native_server_capabilities_without_governed_grants()))
                    },
                    |state| state.server_capabilities.clone(),
                ),
                opt_out_notifications: preferred_connection
                    .and_then(|connection| {
                        (!connection.opt_out_notifications.is_empty())
                            .then(|| connection.opt_out_notifications.clone())
                    })
                    .or_else(|| agent_state.and_then(|state| state.opt_out_notifications.clone()))
                    .or_else(|| {
                        restored_state.and_then(|state| state.opt_out_notifications.clone())
                    }),
                connection_role: preferred_connection
                    .map(|connection| connection.role)
                    .or_else(|| agent_state.and_then(|state| state.connection_role))
                    .or_else(|| restored_state.and_then(|state| state.connection_role)),
                connection_count: state.connections.len(),
                subscriber_count: state.subscriptions.len(),
                controller_subscription_id,
                controller_connection_id: state.controller_connection_id.clone(),
                connections,
                model: agent_state
                    .and_then(|state| state.model.clone())
                    .or_else(|| restored_state.and_then(|state| state.model.clone()))
                    .or_else(|| Some("rust-hosted-runner".to_string())),
                provider: agent_state
                    .and_then(|state| state.provider.clone())
                    .or_else(|| restored_state.and_then(|state| state.provider.clone()))
                    .or_else(|| Some("rust".to_string())),
                session_id: if prefer_restored_host_state {
                    Some(state.session_id.clone())
                } else {
                    agent_state
                        .and_then(|state| state.session_id.clone())
                        .or_else(|| restored_state.and_then(|state| state.session_id.clone()))
                        .or_else(|| Some(state.session_id.clone()))
                },
                cwd: agent_state
                    .and_then(|state| state.cwd.clone())
                    .or_else(|| restored_state.and_then(|state| state.cwd.clone()))
                    .or_else(|| Some(self.config.workspace_root.to_string_lossy().to_string())),
                git_branch,
                current_response: None,
                pending_approvals: redacted_pending_snapshot(
                    agent_state.map(|state| state.pending_approvals.as_slice()),
                    restored_pending_state.map(|state| state.pending_approvals.as_slice()),
                ),
                pending_client_tools: redacted_pending_snapshot(
                    agent_state.map(|state| state.pending_client_tools.as_slice()),
                    restored_pending_state.map(|state| state.pending_client_tools.as_slice()),
                ),
                governed_client_tool_bindings: agent_state
                    .map(|state| state.governed_client_tool_bindings.clone())
                    .or_else(|| {
                        restored_state.map(|state| state.governed_client_tool_bindings.clone())
                    })
                    .unwrap_or_default(),
                pending_mcp_elicitations: restored_pending_state
                    .map(|state| redacted_pending_request_values(&state.pending_mcp_elicitations))
                    .unwrap_or_default(),
                pending_user_inputs: redacted_pending_snapshot(
                    agent_state.map(|state| state.pending_user_inputs.as_slice()),
                    restored_pending_state.map(|state| state.pending_user_inputs.as_slice()),
                ),
                pending_tool_retries: redacted_pending_snapshot(
                    agent_state.map(|state| state.pending_tool_retries.as_slice()),
                    restored_pending_state.map(|state| state.pending_tool_retries.as_slice()),
                ),
                tracked_tools: Vec::new(),
                active_tools: redacted_active_tool_snapshot(agent_state, restored_pending_state),
                codex_subagent_edges: agent_state
                    .map(|state| state.codex_subagent_edges.clone())
                    .or_else(|| restored_state.map(|state| state.codex_subagent_edges.clone()))
                    .unwrap_or_default(),
                active_utility_commands: state.active_utility_commands.values().cloned().collect(),
                active_file_watches: state.active_file_watches.values().cloned().collect(),
                last_error: if host_ready {
                    agent_state
                        .and_then(|state| state.last_error.clone())
                        .or_else(|| state.last_error.clone())
                        .or_else(|| restored_state.and_then(|state| state.last_error.clone()))
                } else {
                    state
                        .last_error
                        .clone()
                        .or_else(|| agent_state.and_then(|state| state.last_error.clone()))
                        .or_else(|| restored_state.and_then(|state| state.last_error.clone()))
                },
                last_error_type: if host_ready {
                    agent_state
                        .and_then(|state| state.last_error_type)
                        .map(|error_type| json_string_value(&error_type))
                        .or_else(|| state.last_error_type.clone())
                        .or_else(|| restored_state.and_then(|state| state.last_error_type.clone()))
                } else {
                    state
                        .last_error_type
                        .clone()
                        .or_else(|| {
                            agent_state
                                .and_then(|state| state.last_error_type)
                                .map(|error_type| json_string_value(&error_type))
                        })
                        .or_else(|| restored_state.and_then(|state| state.last_error_type.clone()))
                },
                provider_error_kind: if let Some(agent_state) = agent_state {
                    agent_state.provider_error_kind
                } else {
                    state
                        .provider_error_kind
                        .or_else(|| restored_state.and_then(|state| state.provider_error_kind))
                },
                last_status: if host_ready && prefer_restored_host_state {
                    state
                        .last_status
                        .clone()
                        .or_else(|| agent_state.and_then(|state| state.last_status.clone()))
                        .or_else(|| restored_state.and_then(|state| state.last_status.clone()))
                } else if host_ready {
                    agent_state
                        .and_then(|state| state.last_status.clone())
                        .or_else(|| state.last_status.clone())
                        .or_else(|| restored_state.and_then(|state| state.last_status.clone()))
                } else {
                    state
                        .last_status
                        .clone()
                        .or_else(|| agent_state.and_then(|state| state.last_status.clone()))
                        .or_else(|| restored_state.and_then(|state| state.last_status.clone()))
                },
                last_response_duration_ms: agent_state
                    .and_then(|state| state.last_response_duration_ms)
                    .or_else(|| restored_state.and_then(|state| state.last_response_duration_ms)),
                last_ttft_ms: agent_state
                    .and_then(|state| state.last_ttft_ms)
                    .or_else(|| restored_state.and_then(|state| state.last_ttft_ms)),
                is_ready: host_ready
                    && agent_state
                        .map(|state| state.is_ready)
                        .or_else(|| restored_state.map(|state| state.is_ready))
                        .unwrap_or(true),
                is_responding: agent_state
                    .map(|state| state.is_responding)
                    .or_else(|| restored_state.map(|state| state.is_responding))
                    .unwrap_or(false),
            },
        }
    }

    pub(super) fn public_snapshot(&self, state: &RunnerState) -> RuntimeSnapshot {
        let mut snapshot = self.snapshot(state);
        snapshot.state.controller_subscription_id = None;
        let (model, provider) =
            accepted_runtime_binding(snapshot.state.model.take(), snapshot.state.provider.take());
        snapshot.state.model = model;
        snapshot.state.provider = provider;
        snapshot
    }

    pub(super) fn runtime_receipt(&self) -> Option<RuntimeReceipt> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_receipt
            .clone()
    }

    /// Derive the latest evidence object only after the hosted journal has
    /// successfully persisted the state represented by `state`.
    pub(super) fn refresh_runtime_receipt(
        &self,
        state: &mut RunnerState,
        kind: RuntimeReceiptKind,
        terminal: Option<RuntimeTerminalClassification>,
        error_type: Option<&str>,
        snapshot_lineage: Option<String>,
    ) {
        let snapshot = self.public_snapshot(state);
        let lifecycle_state = match kind {
            RuntimeReceiptKind::Ready | RuntimeReceiptKind::Restored => {
                RuntimeLifecycleState::ExecutionReady
            }
            RuntimeReceiptKind::Terminal => RuntimeLifecycleState::Active,
            RuntimeReceiptKind::Failed => RuntimeLifecycleState::Failed,
            RuntimeReceiptKind::Draining => RuntimeLifecycleState::Draining,
            RuntimeReceiptKind::Drained => RuntimeLifecycleState::Drained,
        };
        let snapshot_lineage = snapshot_lineage
            .or_else(|| state.restored_snapshot_lineage.clone())
            .or_else(|| {
                state
                    .restored_snapshot
                    .as_ref()
                    .map(|snapshot| format!("restore-cursor:{}", snapshot.cursor))
            });
        let binding_rejected =
            state.last_error_type.as_deref() == Some(INVALID_RUNTIME_RECEIPT_IDENTITY);
        let input = RuntimeReceiptInput {
            kind,
            lifecycle_state,
            runtime_generation: self.config.runtime_generation,
            runner_session_id: self.binding.runner_session_id.as_str().to_string(),
            maestro_session_id: state.session_id.clone(),
            workspace_id: self
                .config
                .workload_identity
                .as_ref()
                .map(|identity| identity.workspace_id.clone())
                .or_else(|| self.config.workspace_id.clone()),
            agent_run_id: self.config.agent_run_id.clone(),
            model: (!binding_rejected)
                .then(|| state.runtime_model_binding.clone().or(snapshot.state.model))
                .flatten(),
            provider: (!binding_rejected)
                .then(|| {
                    state
                        .runtime_provider_binding
                        .clone()
                        .or(snapshot.state.provider)
                })
                .flatten(),
            capability_digest: headless_protocol_capability_digest(),
            replay_cursor: state.cursor,
            flush_watermark: self.thread_journal.flush_watermark(),
            snapshot_lineage,
            terminal,
            error_type: error_type.map(str::to_string),
        };
        match RuntimeReceipt::derive(input) {
            Ok(receipt) => state.runtime_receipt = Some(receipt),
            Err(error) => tracing::warn!(
                event = "runtime_receipt_derivation_failed",
                kind = ?kind,
                error = %error,
                "durable hosted state did not produce a valid runtime receipt"
            ),
        }
    }

    /// Persist a drain boundary and publish its receipt when the local journal
    /// accepts the write. The caller is still holding the mutation lock and
    /// must keep the event pump running when this synchronous write fails so
    /// the drain request remains retryable.
    pub(super) fn persist_receipt_boundary(
        &self,
        state: &mut RunnerState,
        kind: RuntimeReceiptKind,
        site: &'static str,
    ) -> io::Result<()> {
        // Stage the generation-level Draining snapshot in the same journal
        // write as the receipt boundary. A process exit between receipt
        // derivation and the later final snapshot must restart as Draining,
        // never as a fresh attachable runtime. Restore the queues exactly if
        // the synchronous write fails so this request remains retryable.
        let staged_draining_snapshot = if kind == RuntimeReceiptKind::Draining {
            let previous_envelopes = state.envelopes.clone();
            let previous_controller_envelopes = state.controller_envelopes.clone();
            let envelope = StreamEnvelope::Snapshot {
                snapshot: self.public_snapshot(state),
            };
            state.envelopes.push_back(envelope.clone());
            state.controller_envelopes.push_back(envelope.clone());
            while state.envelopes.len() > MAX_EVENTS {
                state.envelopes.pop_front();
            }
            while state.controller_envelopes.len() > MAX_EVENTS {
                state.controller_envelopes.pop_front();
            }
            Some((previous_envelopes, previous_controller_envelopes, envelope))
        } else {
            None
        };
        #[allow(clippy::disallowed_methods)]
        match self.persist_thread(state) {
            Ok(()) => {
                self.refresh_runtime_receipt(state, kind, None, None, None);
                if let Some((_, _, envelope)) = staged_draining_snapshot {
                    let _ = self.events.send(envelope.clone());
                    let _ = self.controller_events.send(envelope);
                }
                Ok(())
            }
            Err(error) => {
                if let Some((previous_envelopes, previous_controller_envelopes, _)) =
                    staged_draining_snapshot
                {
                    state.envelopes = previous_envelopes;
                    state.controller_envelopes = previous_controller_envelopes;
                }
                tracing::warn!(
                    event = "thread_journal_persistence_failed",
                    site,
                    error = %error,
                    "runtime receipt boundary could not be persisted synchronously"
                );
                Err(error)
            }
        }
    }

    pub(super) fn record_identity_failure(
        &self,
        state: &mut RunnerState,
        operation: &'static str,
        requested_session_id: &str,
    ) -> HostedResult<IdentityBindingFailure> {
        let failure = IdentityBindingFailure::new(
            &self.binding,
            operation,
            requested_session_id,
            self.config.runtime_generation,
        );
        let evicted = if state.identity_binding_failures.len() >= MAX_IDENTITY_BINDING_FAILURES {
            state.identity_binding_failures.pop_front()
        } else {
            None
        };
        state.identity_binding_failures.push_back(failure.clone());
        if let Err(error) = self.persist_thread_for_request(state) {
            state.identity_binding_failures.pop_back();
            if let Some(evicted) = evicted {
                state.identity_binding_failures.push_front(evicted);
            }
            return Err(HostedError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                format!("failed to persist identity binding failure evidence: {error}"),
            ));
        }
        Ok(failure)
    }

    /// Raw thread-journal write. Do not call this directly outside the
    /// wrappers below and the startup/publish/retry paths: raw persistence
    /// errors have repeatedly wedged the runtime (dead event pump, drains
    /// permanently rejected as "already draining", leaked ownership slots).
    /// Pick the wrapper that states the caller's failure semantics:
    /// [`Self::persist_thread_or_defer`] for runtime-event paths,
    /// [`Self::persist_thread_for_request`] for request-scoped paths, and
    /// [`Self::persist_thread_best_effort`] for error-path cleanup.
    /// Enforced by `disallowed-methods` in `clippy.toml`.
    pub(super) fn persist_thread(&self, state: &RunnerState) -> io::Result<()> {
        #[cfg(test)]
        {
            let mut failures = self
                .thread_persistence_failures
                .lock()
                .expect("thread persistence failure counter");
            if *failures > 0 {
                *failures -= 1;
                return Err(io::Error::other(
                    "injected thread journal persistence failure",
                ));
            }
        }
        let snapshot = self.public_snapshot(state);
        self.thread_journal.persist(
            &state.thread,
            self.config.runtime_generation,
            state.cursor,
            &state.envelopes,
            ThreadJournalMetadataView {
                runner_session_id: self.config.runner_session_id.as_str(),
                runtime_model: state
                    .runtime_model_binding
                    .as_deref()
                    .or(snapshot.state.model.as_deref()),
                runtime_provider: state
                    .runtime_provider_binding
                    .as_deref()
                    .or(snapshot.state.provider.as_deref()),
                drain_runtime_failed_before_finalization: state
                    .drain_runtime_failed_before_finalization,
                executor_drain_result_applied_count: state.executor_drain_result_applied_count,
                last_init: state.last_init.as_ref(),
                keys: &state.response_idempotency_keys,
                digests: &state.response_idempotency_digests,
                request_owners: &state.response_request_owners,
                pending: &state.pending_response_idempotency,
                order: &state.response_idempotency_order,
                pending_order: &state.pending_response_idempotency_order,
            },
            &state.identity_binding_failures,
        )
    }

    /// Persist for a runtime-event path (event pump, executor drain, the
    /// `/drain` handler). The in-memory mutation has already happened; a
    /// journal write failure is deferred to the event pump's retry instead of
    /// propagated, because an error return on these paths wedges the runtime
    /// rather than the failing operation.
    pub(super) fn persist_thread_or_defer(&self, state: &RunnerState, site: &'static str) {
        #[allow(clippy::disallowed_methods)]
        if let Err(error) = self.persist_thread(state) {
            self.thread_persistence_retry_pending
                .store(true, std::sync::atomic::Ordering::Release);
            self.schedule_thread_persistence_recovery();
            tracing::warn!(
                event = "thread_journal_persistence_deferred",
                site,
                error = %error,
                "thread journal write failed; keeping live-process state and retrying from the event pump",
            );
        }
    }

    /// Persist the executor-complete drain phase before manifest generation or
    /// final journal work can fail. The marker remains a local Draining
    /// receipt, but gives restart recovery enough durable evidence to skip a
    /// second executor drain and resume only the finalization hand-off.
    pub(super) fn persist_drain_finalization_boundary(
        &self,
        state: &mut RunnerState,
    ) -> io::Result<()> {
        let previous_envelopes = state.envelopes.clone();
        let previous_controller_envelopes = state.controller_envelopes.clone();
        let envelope = StreamEnvelope::Snapshot {
            snapshot: self.public_snapshot(state),
        };
        state.envelopes.push_back(envelope.clone());
        state.controller_envelopes.push_back(envelope.clone());
        while state.envelopes.len() > MAX_EVENTS {
            state.envelopes.pop_front();
        }
        while state.controller_envelopes.len() > MAX_EVENTS {
            state.controller_envelopes.pop_front();
        }
        #[allow(clippy::disallowed_methods)]
        match self.persist_thread(state) {
            Ok(()) => {
                let _ = self.events.send(envelope.clone());
                let _ = self.controller_events.send(envelope);
                Ok(())
            }
            Err(error) => {
                state.envelopes = previous_envelopes;
                state.controller_envelopes = previous_controller_envelopes;
                Err(error)
            }
        }
    }

    /// Persist for a request-scoped path where failing the request is safe:
    /// the caller rolls back its own mutation or reports the error to the
    /// client, and nothing process-wide is left wedged.
    pub(super) fn persist_thread_for_request(&self, state: &RunnerState) -> io::Result<()> {
        #[allow(clippy::disallowed_methods)]
        self.persist_thread(state)
    }

    /// Retain the executor's consumed drain result in a private, atomic
    /// handoff file before the request advances to final journal work. The
    /// handoff is generation-bound and is removed only after the final
    /// `Drained` snapshot has been durably accepted.
    pub(super) fn persist_executor_drain_result(
        &self,
        state: &RunnerState,
        pending: &PendingExecutorDrainResult,
    ) -> io::Result<()> {
        #[cfg(test)]
        {
            let mut failures = self
                .executor_drain_result_persistence_failures
                .lock()
                .expect("executor drain result persistence failure counter");
            if *failures > 0 {
                *failures -= 1;
                return Err(io::Error::other(
                    "injected executor drain result persistence failure",
                ));
            }
        }
        let mut retained = pending.result.clone();
        retained.messages = retained
            .messages
            .into_iter()
            .map(crate::transcript::redact_agent_message)
            .collect();
        persist_executor_drain_result(
            &self.config.workspace_root,
            state.thread.thread_id().as_str(),
            self.config.runtime_generation,
            pending.replay_cursor,
            &retained,
        )
    }

    pub(super) fn clear_executor_drain_result(&self, state: &RunnerState) -> io::Result<()> {
        clear_executor_drain_result(
            &self.config.workspace_root,
            state.thread.thread_id().as_str(),
            self.config.runtime_generation,
        )
    }

    /// Apply a retained executor result exactly once per in-memory retry. The
    /// durable applied position skips messages already committed with the
    /// executor-complete boundary. Before that boundary exists, the replay
    /// cursor scopes fallback deduplication to messages emitted after this
    /// retained batch was captured; a pre-application multiset lets already
    /// persisted copies consume one matching position without allowing
    /// duplicate messages within the retained batch to suppress one another.
    /// The private handoff remains until the final `Drained` boundary succeeds.
    pub(super) fn apply_pending_executor_drain_result(&self, state: &mut RunnerState) -> bool {
        let Some(pending) = state.pending_executor_drain_result.clone() else {
            return true;
        };
        if state.executor_drain_result_applied {
            return true;
        }
        let message_count = pending.result.messages.len();
        let applied_count = state.executor_drain_result_applied_count.min(message_count);
        let mut replay_counts = drain_replay_message_counts(state, pending.replay_cursor);
        for (index, message) in pending.result.messages.into_iter().enumerate() {
            if index < applied_count {
                continue;
            }
            let already_applied = drain_message_replay_key(&message)
                .and_then(|key| replay_counts.get_mut(&key))
                .is_some_and(|count| {
                    if *count == 0 {
                        false
                    } else {
                        *count -= 1;
                        true
                    }
                });
            if !already_applied && !self.publish_message(state, message) {
                // A lifecycle persistence failure rolls the message back
                // and fences later nonfatal messages. Do not advance the
                // retained handoff past that position or let finalization
                // discard the failed message and its gated suffix.
                return false;
            }
            state.executor_drain_result_applied_count = index + 1;
        }
        self.finalize_consumed_response_keys(state, &pending.result.consumed_response_keys);
        self.rollback_rejected_response_keys(state, &pending.result.rejected_response_keys);
        state.executor_drain_result_applied_count = message_count;
        state.executor_drain_result_applied = true;
        true
    }

    /// Best-effort persist on a cleanup path that is already returning an
    /// error. The failure is logged, never propagated; the next successful
    /// journal write covers the missed one because the full state is
    /// serialized on every persist.
    pub(super) fn persist_thread_best_effort(&self, state: &RunnerState, site: &'static str) {
        #[allow(clippy::disallowed_methods)]
        if let Err(error) = self.persist_thread(state) {
            tracing::warn!(
                event = "thread_journal_persistence_skipped",
                site,
                error = %error,
                "best-effort thread journal write failed on a cleanup path",
            );
        }
    }

    #[cfg(test)]
    pub(super) fn fail_next_thread_persistences(&self, count: usize) {
        *self
            .thread_persistence_failures
            .lock()
            .expect("thread persistence failure counter") = count;
    }

    #[cfg(test)]
    pub(super) fn fail_next_executor_drain_result_persistences(&self, count: usize) {
        *self
            .executor_drain_result_persistence_failures
            .lock()
            .expect("executor drain result persistence failure counter") = count;
    }

    pub(super) fn controller_pending_events(
        &self,
        state: &mut RunnerState,
    ) -> Vec<FromAgentMessage> {
        let Some(agent_state) = self.prune_pending_controller_events(state) else {
            return Vec::new();
        };
        let mut events: Vec<_> = pending_controller_requests(&agent_state)
            .into_iter()
            .filter_map(|pending| {
                state
                    .pending_controller_events
                    .iter()
                    .rev()
                    .find(|message| pending_controller_event_matches(&pending, message))
                    .cloned()
            })
            .collect();
        events.extend(
            state
                .pending_controller_events
                .iter()
                .filter(|message| {
                    matches!(message, FromAgentMessage::ManagedAuthorizationRequest { request_id }
                if agent_state.pending_managed_authorizations.contains(request_id))
                })
                .cloned(),
        );
        events
    }

    pub(super) fn prune_pending_controller_events(
        &self,
        state: &mut RunnerState,
    ) -> Option<AgentState> {
        let agent_state = self.message_executor.state().ok().flatten()?;
        let live_pending = pending_controller_requests(&agent_state);
        let mut live_index = HashMap::new();
        for pending in &live_pending {
            let key = pending_controller_request_key(pending);
            live_index
                .entry(key)
                .or_insert_with(Vec::new)
                .push(*pending);
            if key.3.is_some() {
                live_index
                    .entry((key.0, key.1, key.2, None))
                    .or_insert_with(Vec::new)
                    .push(*pending);
            }
        }
        state.pending_controller_events.retain(|message| {
            if let FromAgentMessage::ManagedAuthorizationRequest { request_id } = message {
                return agent_state
                    .pending_managed_authorizations
                    .contains(request_id);
            }
            pending_controller_event_key(message)
                .and_then(|key| live_index.get(&key))
                .is_some_and(|pending| {
                    pending
                        .iter()
                        .any(|pending| pending_controller_event_matches(pending, message))
                })
        });
        Some(agent_state)
    }

    pub(super) fn controller_stream_is_authorized(
        &self,
        authorization: &ControllerStreamAuthorization,
    ) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.controller_connection_id.as_deref() != Some(authorization.connection_id.as_str()) {
            return false;
        }
        if authorization.cancellation.is_cancelled() {
            return false;
        }
        if !state
            .connections
            .get(&authorization.connection_id)
            .is_some_and(|connection| connection.role == ConnectionRole::Controller)
        {
            return false;
        }
        state
            .subscriptions
            .get(&authorization.subscription_id)
            .is_some_and(|subscription| {
                subscription.attached
                    && subscription.role == ConnectionRole::Controller
                    && subscription.connection_id == authorization.connection_id
            })
    }

    pub(super) fn publish_message(
        &self,
        state: &mut RunnerState,
        message: FromAgentMessage,
    ) -> bool {
        self.publish_message_with_fatal_category(state, message, None)
    }

    pub(super) fn publish_message_with_fatal_category(
        &self,
        state: &mut RunnerState,
        message: FromAgentMessage,
        fatal_error_type: Option<&str>,
    ) -> bool {
        // Once a lifecycle boundary has failed to persist, later messages in
        // the same drained batch are no longer authoritative. In particular,
        // a trailing terminal event must not replace failed evidence with a
        // terminal receipt or be replayed as if the failed boundary existed.
        // The dedicated fatal error path is still admitted so it can stage and
        // synchronously retry the failure boundary.
        if state.runtime_failed && !matches!(&message, FromAgentMessage::Error { fatal: true, .. })
        {
            return false;
        }
        let message = normalize_hosted_controller_request(message);
        let mut invalid_runtime_receipt_binding = false;
        if let FromAgentMessage::Ready {
            model, provider, ..
        } = &message
        {
            let terminal_binding_is_durable =
                state.runtime_receipt.as_ref().is_some_and(|receipt| {
                    matches!(
                        receipt.kind,
                        RuntimeReceiptKind::Terminal
                            | RuntimeReceiptKind::Failed
                            | RuntimeReceiptKind::Draining
                            | RuntimeReceiptKind::Drained
                    )
                });
            if !terminal_binding_is_durable {
                match HostedRunnerConfig::validate_live_runtime_receipt_binding(model, provider) {
                    Ok(()) => {
                        // A Ready event is the producer's explicit model/provider
                        // binding. Keep it alongside the durable journal so a later
                        // terminal receipt cannot be recomputed from a new
                        // executor's fallback state after restart. Once a terminal
                        // boundary is durable, a delayed Ready is stale and must not
                        // rewrite the binding represented by that receipt; a new
                        // turn clears the receipt before accepting a fresh binding.
                        state.runtime_model_binding = Some(model.clone());
                        state.runtime_provider_binding = Some(provider.clone());
                    }
                    Err(error) => {
                        // A live binding is part of the receipt identity contract.
                        // Do not retain an earlier execution-ready receipt when the
                        // producer reports metadata that cannot be represented.
                        // Preserve the prior bounded binding, if any, so failed
                        // evidence does not copy the invalid value into a receipt.
                        state.runtime_failed = true;
                        state.ready = false;
                        state.last_status = Some("Runtime failed".to_string());
                        state.last_error = Some(format!(
                            "runtime receipt identity validation failed: {error}"
                        ));
                        state.last_error_type = Some(INVALID_RUNTIME_RECEIPT_IDENTITY.to_string());
                        state.provider_error_kind = None;
                        state
                            .thread
                            .set_runtime_failure_type(INVALID_RUNTIME_RECEIPT_IDENTITY);
                        state.runtime_receipt = None;
                        invalid_runtime_receipt_binding = true;
                    }
                }
            }
        }
        // Stream chunks stay in the bounded in-memory replay buffer. Persist
        // only lifecycle boundaries so token streaming does not serialize and
        // fsync the entire journal for every chunk. A crash mid-response is
        // restored as interrupted; every terminal/waiting boundary is durable.
        let persist_lifecycle_boundary = matches!(
            &message,
            FromAgentMessage::Ready { .. }
                | FromAgentMessage::ResponseEnd { .. }
                | FromAgentMessage::TurnCompleted { .. }
                | FromAgentMessage::TurnInterrupted { .. }
                | FromAgentMessage::ServerRequest { .. }
                | FromAgentMessage::ManagedAuthorizationRequest { .. }
                | FromAgentMessage::ServerRequestResolved { .. }
                | FromAgentMessage::Error { .. }
                | FromAgentMessage::ProviderError { .. }
        );
        let mut lifecycle_rollback =
            persist_lifecycle_boundary.then(|| LifecycleRollback::capture(state));
        let agent_state = self.prune_pending_controller_events(state);
        let matching_pending = agent_state.as_ref().and_then(|agent_state| {
            pending_controller_requests(agent_state)
                .into_iter()
                .find(|pending| pending_controller_event_matches(pending, &message))
        });
        if let FromAgentMessage::ManagedAuthorizationRequest { request_id } = &message {
            if agent_state
                .as_ref()
                .is_some_and(|state| state.pending_managed_authorizations.contains(request_id))
            {
                state.pending_controller_events.retain(|existing| !matches!(existing,
                    FromAgentMessage::ManagedAuthorizationRequest { request_id: existing_id } if existing_id == request_id));
                state.pending_controller_events.push_back(message.clone());
            }
        } else if let Some(pending) = matching_pending {
            state
                .pending_controller_events
                .retain(|existing| !pending_controller_event_matches(&pending, existing));
            // Authoritative pending state is the bound: every live request keeps
            // exactly one raw event, regardless of the generic replay limit.
            state.pending_controller_events.push_back(message.clone());
        } else if agent_state.is_none() && pending_controller_event_key(&message).is_some() {
            // If authoritative state is temporarily unavailable, retain a
            // bounded best-effort copy until a later publish or attach can
            // reconcile it against the live pending set.
            state.pending_controller_events.retain(|existing| {
                pending_controller_event_key(existing) != pending_controller_event_key(&message)
            });
            if state.pending_controller_events.len() >= MAX_UNVERIFIED_PENDING_CONTROLLER_EVENTS {
                state.pending_controller_events.pop_front();
            }
            state.pending_controller_events.push_back(message.clone());
        }

        match &message {
            FromAgentMessage::ResponseStart { response_id } => {
                state.active_response_ids.insert(response_id.clone());
                state.provider_error_kind = None;
            }
            FromAgentMessage::ResponseEnd { response_id, .. } => {
                state.active_response_ids.remove(response_id);
            }
            FromAgentMessage::Error {
                fatal, terminal, ..
            } if *fatal || *terminal => state.active_response_ids.clear(),
            FromAgentMessage::TurnCompleted { .. }
            | FromAgentMessage::TurnInterrupted { .. }
            | FromAgentMessage::ProviderError { .. } => state.active_response_ids.clear(),
            _ => {}
        }
        // Reserve the cursor immediately before a response completion for the
        // coarse transcript's durable aggregate. This lets a reconnect resume
        // between the aggregate and ResponseEnd without suppressing either.
        state.cursor = state.cursor.saturating_add(
            if matches!(&message, FromAgentMessage::ResponseEnd { .. }) {
                2
            } else {
                1
            },
        );
        if let FromAgentMessage::Error {
            message,
            fatal: true,
            ..
        } = &message
        {
            state.runtime_failed = true;
            state.ready = false;
            state.last_status = Some("Runtime failed".to_string());
            state.last_error = Some(message.clone());
            state.last_error_type = Some(fatal_error_type.unwrap_or("fatal").to_string());
            state.provider_error_kind = None;
            if let Some(fatal_error_type) = fatal_error_type {
                state.thread.set_runtime_failure_type(fatal_error_type);
            }
        }
        if let FromAgentMessage::ProviderError { kind, message } = &message {
            state.last_error = Some(message.clone());
            state.last_error_type = Some("protocol".to_string());
            state.provider_error_kind = Some(*kind);
        }
        let thread_terminal_transition = state.thread.apply_agent_message(&message, state.cursor);
        let receipt_transition = match &message {
            FromAgentMessage::Ready { .. }
                if !state.runtime_failed
                    && !state.restore_incomplete
                    && !state.draining
                    && !state
                        .runtime_receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.kind == RuntimeReceiptKind::Terminal) =>
            {
                Some((RuntimeReceiptKind::Ready, None, None))
            }
            FromAgentMessage::TurnCompleted { .. }
                if !state.runtime_failed && !state.draining && thread_terminal_transition =>
            {
                Some((
                    RuntimeReceiptKind::Terminal,
                    Some(RuntimeTerminalClassification::Completed),
                    None,
                ))
            }
            FromAgentMessage::TurnInterrupted { .. }
                if !state.runtime_failed && !state.draining && thread_terminal_transition =>
            {
                Some((
                    RuntimeReceiptKind::Terminal,
                    Some(RuntimeTerminalClassification::Interrupted),
                    None,
                ))
            }
            FromAgentMessage::ProviderError { .. }
                if !state.runtime_failed && !state.draining && thread_terminal_transition =>
            {
                Some((
                    RuntimeReceiptKind::Terminal,
                    Some(RuntimeTerminalClassification::ProviderFailed),
                    None,
                ))
            }
            FromAgentMessage::Error { fatal: true, .. } => Some((
                RuntimeReceiptKind::Failed,
                None,
                Some(
                    state
                        .last_error_type
                        .clone()
                        .unwrap_or_else(|| "fatal".to_string()),
                ),
            )),
            FromAgentMessage::Error {
                fatal,
                terminal: true,
                error_type,
                ..
            } if !state.draining && thread_terminal_transition => Some((
                RuntimeReceiptKind::Terminal,
                Some(super::thread_protocol::runtime_terminal_classification(
                    *fatal,
                    *error_type,
                )),
                None,
            )),
            _ => None,
        };
        let controller_envelope = StreamEnvelope::Message {
            cursor: state.cursor,
            message: Box::new(crate::transcript::agent_message_for_controller(
                message.clone(),
            )),
        };
        let envelope = StreamEnvelope::Message {
            cursor: state.cursor,
            message: Box::new(crate::transcript::redact_agent_message(message)),
        };
        // The durable journal serializes the replay buffers as well as the
        // thread phase. Stage the current envelope before persistence so a
        // restart never observes a terminal phase without its terminal event.
        if !invalid_runtime_receipt_binding {
            state.envelopes.push_back(envelope.clone());
            state
                .controller_envelopes
                .push_back(controller_envelope.clone());
            while state.envelopes.len() > MAX_EVENTS {
                state.envelopes.pop_front();
            }
            while state.controller_envelopes.len() > MAX_EVENTS {
                state.controller_envelopes.pop_front();
            }
        }
        let failure_snapshot = if invalid_runtime_receipt_binding {
            let snapshot = StreamEnvelope::Snapshot {
                snapshot: self.public_snapshot(state),
            };
            state.envelopes.push_back(snapshot.clone());
            state.controller_envelopes.push_back(snapshot.clone());
            while state.envelopes.len() > MAX_EVENTS {
                state.envelopes.pop_front();
            }
            while state.controller_envelopes.len() > MAX_EVENTS {
                state.controller_envelopes.pop_front();
            }
            Some(snapshot)
        } else {
            None
        };
        if persist_lifecycle_boundary {
            let flush_before_receipt = receipt_transition.as_ref().is_some_and(|(kind, ..)| {
                matches!(
                    kind,
                    RuntimeReceiptKind::Terminal | RuntimeReceiptKind::Failed
                )
            });
            if flush_before_receipt {
                // The terminal message is already present in the executor's
                // session recorder. Flush it before the corresponding
                // lifecycle event enters the hosted journal, so a later
                // failure cannot leave a replayable terminal boundary whose
                // transcript is incomplete.
                if let Err(error) = self.message_executor.flush_session() {
                    lifecycle_rollback
                        .take()
                        .expect("lifecycle boundary has rollback state")
                        .restore(state);
                    self.persist_session_flush_failure(state, error);
                    return false;
                }
            }
            // Publish path: a failed boundary write deliberately degrades the
            // runner to not-ready instead of deferring, so controllers stop
            // sending work against a journal that is not durable.
            #[allow(clippy::disallowed_methods)]
            if let Err(error) = self.persist_thread(state) {
                lifecycle_rollback
                    .take()
                    .expect("lifecycle boundary has rollback state")
                    .restore(state);
                state.runtime_failed = true;
                state.ready = false;
                state.last_error = Some(format!("durable thread journal write failed: {error}"));
                state.last_status = Some("Runtime failed".to_string());
                state.last_error_type = Some("internal".to_string());
                state.provider_error_kind = None;
                state.runtime_receipt = None;
                state.thread.set_runtime_failure_type("internal");
                let failure_snapshot = StreamEnvelope::Snapshot {
                    snapshot: self.public_snapshot(state),
                };
                state.envelopes.push_back(failure_snapshot.clone());
                state.controller_envelopes.push_back(failure_snapshot);
                while state.envelopes.len() > MAX_EVENTS {
                    state.envelopes.pop_front();
                }
                while state.controller_envelopes.len() > MAX_EVENTS {
                    state.controller_envelopes.pop_front();
                }
                self.thread_persistence_retry_pending
                    .store(true, Ordering::Release);
                self.schedule_thread_persistence_recovery();
                return false;
            }
        }
        if let Some((kind, terminal, error_type)) = receipt_transition {
            self.refresh_runtime_receipt(state, kind, terminal, error_type.as_deref(), None);
        }
        if !invalid_runtime_receipt_binding {
            let _ = self.events.send(envelope);
            let _ = self.controller_events.send(controller_envelope);
        }
        if let Some(failure_snapshot) = failure_snapshot {
            let _ = self.events.send(failure_snapshot.clone());
            let _ = self.controller_events.send(failure_snapshot);
        }
        !invalid_runtime_receipt_binding
    }

    fn persist_session_flush_failure(&self, state: &mut RunnerState, error: HostedRunnerError) {
        const SESSION_FLUSH_FAILURE: &str = "session_recorder_flush_failed";

        state.runtime_failed = true;
        state.ready = false;
        state.last_status = Some("Runtime failed".to_string());
        state.last_error = Some(format!("session recorder flush failed: {error}"));
        state.last_error_type = Some(SESSION_FLUSH_FAILURE.to_string());
        state.provider_error_kind = None;
        state.runtime_receipt = None;
        state.thread.set_runtime_failure_type(SESSION_FLUSH_FAILURE);

        let failure_snapshot = StreamEnvelope::Snapshot {
            snapshot: self.public_snapshot(state),
        };
        state.envelopes.push_back(failure_snapshot.clone());
        state
            .controller_envelopes
            .push_back(failure_snapshot.clone());
        while state.envelopes.len() > MAX_EVENTS {
            state.envelopes.pop_front();
        }
        while state.controller_envelopes.len() > MAX_EVENTS {
            state.controller_envelopes.pop_front();
        }

        // The staged terminal message was rolled back before this helper ran.
        // Persist only the failed snapshot so restart recovery cannot derive
        // terminal evidence from an unflushed session result.
        #[allow(clippy::disallowed_methods)]
        match self.persist_thread(state) {
            Ok(()) => {
                self.refresh_runtime_receipt(
                    state,
                    RuntimeReceiptKind::Failed,
                    None,
                    Some(SESSION_FLUSH_FAILURE),
                    None,
                );
                let _ = self.events.send(failure_snapshot.clone());
                let _ = self.controller_events.send(failure_snapshot);
            }
            Err(persist_error) => {
                self.thread_persistence_retry_pending
                    .store(true, Ordering::Release);
                self.schedule_thread_persistence_recovery();
                tracing::error!(
                    event = "session_flush_failure_boundary_persistence_failed",
                    error = %persist_error,
                    "session recorder flush failure could not yet be durably fenced"
                );
            }
        }
    }

    pub(super) fn publish_snapshot(&self, state: &mut RunnerState) -> bool {
        let envelope = StreamEnvelope::Snapshot {
            snapshot: self.public_snapshot(state),
        };
        state.envelopes.push_back(envelope.clone());
        state.controller_envelopes.push_back(envelope.clone());
        while state.envelopes.len() > MAX_EVENTS {
            state.envelopes.pop_front();
        }
        while state.controller_envelopes.len() > MAX_EVENTS {
            state.controller_envelopes.pop_front();
        }
        let _ = self.events.send(envelope.clone());
        let _ = self.controller_events.send(envelope);
        // Snapshot publish: same deliberate not-ready degradation as
        // publish_message above.
        #[allow(clippy::disallowed_methods)]
        if let Err(error) = self.persist_thread(state) {
            let had_execution_ready_receipt =
                state.runtime_receipt.as_ref().is_some_and(|receipt| {
                    matches!(
                        receipt.kind,
                        RuntimeReceiptKind::Ready | RuntimeReceiptKind::Restored
                    )
                });
            state.runtime_failed = true;
            state.ready = false;
            state.last_error = Some(format!("durable thread journal write failed: {error}"));
            state.last_status = Some("Runtime failed".to_string());
            state.last_error_type = Some("internal".to_string());
            // A failed attachment snapshot must not leave an execution-ready
            // receipt visible while the durable state is unavailable. Keep a
            // previously persisted non-ready boundary (for example
            // Draining) because it remains the last authoritative lifecycle
            // phase and is still the correct recovery contract.
            if had_execution_ready_receipt
                || state
                    .runtime_receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.kind == RuntimeReceiptKind::Terminal)
            {
                state.runtime_receipt = None;
            }
            state.thread.set_runtime_failure_type("internal");
            self.thread_persistence_retry_pending
                .store(true, Ordering::Release);
            self.schedule_thread_persistence_recovery();
            // Replace the staged pre-failure snapshot so the retry records
            // the failed boundary rather than replaying the old Ready state.
            let failure_snapshot = self.public_snapshot(state);
            if let Some(StreamEnvelope::Snapshot { snapshot }) = state.envelopes.back_mut() {
                *snapshot = failure_snapshot.clone();
            }
            if let Some(StreamEnvelope::Snapshot { snapshot }) =
                state.controller_envelopes.back_mut()
            {
                *snapshot = failure_snapshot;
            }
            false
        } else {
            match state.last_status.as_deref() {
                Some("Draining") => {
                    self.refresh_runtime_receipt(
                        state,
                        RuntimeReceiptKind::Draining,
                        None,
                        None,
                        None,
                    );
                }
                Some("Drained") => {
                    self.refresh_runtime_receipt(
                        state,
                        RuntimeReceiptKind::Drained,
                        None,
                        None,
                        None,
                    );
                }
                _ => {}
            }
            true
        }
    }

    pub(super) fn reset_envelope(&self, reason: impl Into<String>) -> StreamEnvelope {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reset_envelope_from_state(&state, reason)
    }

    fn reset_envelope_from_state(
        &self,
        state: &RunnerState,
        reason: impl Into<String>,
    ) -> StreamEnvelope {
        StreamEnvelope::Reset {
            reason: reason.into(),
            snapshot: self.public_snapshot(state),
        }
    }

    pub(super) fn reset_and_subscribe(
        &self,
        reason: impl Into<String>,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        self.reset_and_subscribe_with(reason, false)
    }

    pub(super) fn reset_and_subscribe_controller(
        &self,
        reason: impl Into<String>,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        self.reset_and_subscribe_with(reason, true)
    }

    fn reset_and_subscribe_with(
        &self,
        reason: impl Into<String>,
        controller: bool,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rx = if controller {
            self.controller_events.subscribe()
        } else {
            self.events.subscribe()
        };
        (vec![self.reset_envelope_from_state(&state, reason)], rx)
    }

    pub(super) fn subscribe_from(
        &self,
        cursor: u64,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        self.subscribe_from_with(cursor, false)
    }

    pub(super) fn subscribe_controller_from(
        &self,
        cursor: u64,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        self.subscribe_from_with(cursor, true)
    }

    fn subscribe_from_with(
        &self,
        cursor: u64,
        controller: bool,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (envelopes, rx) = if controller {
            (
                &state.controller_envelopes,
                self.controller_events.subscribe(),
            )
        } else {
            (&state.envelopes, self.events.subscribe())
        };
        (self.replay_from_state(&state, envelopes, cursor), rx)
    }

    /// Coarse transcript filtering needs the retained response prefix to
    /// reconstruct an aggregate after reconnect. Replay the full retained
    /// window while the filter suppresses outputs at or before the requested
    /// cursor. Preserve the normal reset behavior when the cursor has fallen
    /// outside that window.
    pub(super) fn subscribe_coarse_from(
        &self,
        cursor: u64,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        self.subscribe_coarse_from_with(cursor, false)
    }

    pub(super) fn subscribe_controller_coarse_from(
        &self,
        cursor: u64,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        self.subscribe_coarse_from_with(cursor, true)
    }

    fn subscribe_coarse_from_with(
        &self,
        cursor: u64,
        controller: bool,
    ) -> (Vec<StreamEnvelope>, broadcast::Receiver<StreamEnvelope>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (envelopes, rx) = if controller {
            (
                &state.controller_envelopes,
                self.controller_events.subscribe(),
            )
        } else {
            (&state.envelopes, self.events.subscribe())
        };
        let requested = self.replay_from_state(&state, envelopes, cursor);
        if requested
            .iter()
            .any(|envelope| matches!(envelope, StreamEnvelope::Reset { .. }))
        {
            return (requested, rx);
        }
        if !coarse_replay_has_complete_response_boundaries(envelopes, &state.active_response_ids) {
            return (
                vec![self.reset_envelope_from_state(&state, "coarse_replay_incomplete")],
                rx,
            );
        }
        (envelopes.iter().cloned().collect(), rx)
    }

    fn replay_from_state(
        &self,
        state: &RunnerState,
        envelopes: &VecDeque<StreamEnvelope>,
        cursor: u64,
    ) -> Vec<StreamEnvelope> {
        let first_cursor = envelopes.iter().find_map(|envelope| match envelope {
            StreamEnvelope::Message { cursor, .. } | StreamEnvelope::Heartbeat { cursor } => {
                Some(*cursor)
            }
            StreamEnvelope::Snapshot { .. } | StreamEnvelope::Reset { .. } => None,
        });
        if let Some(first_cursor) = first_cursor {
            if cursor > 0 && cursor < first_cursor.saturating_sub(1) {
                return vec![StreamEnvelope::Reset {
                    reason: "replay_gap".to_string(),
                    snapshot: self.public_snapshot(state),
                }];
            }
        }
        envelopes
            .iter()
            .filter(|envelope| match envelope {
                StreamEnvelope::Message {
                    cursor: event_cursor,
                    ..
                }
                | StreamEnvelope::Heartbeat {
                    cursor: event_cursor,
                } => *event_cursor > cursor,
                StreamEnvelope::Snapshot { .. } | StreamEnvelope::Reset { .. } => true,
            })
            .cloned()
            .collect()
    }
}

fn normalize_hosted_controller_request(message: FromAgentMessage) -> FromAgentMessage {
    match message {
        FromAgentMessage::ToolCall {
            call_id,
            tool_execution_id,
            tool,
            args,
            requires_approval: true,
        } => FromAgentMessage::ServerRequest {
            request_id: call_id.clone(),
            request_type: ServerRequestType::Approval,
            call_id,
            tool_execution_id,
            tool,
            args,
            reason: "tool requires approval".to_string(),
            started_at_ms: None,
        },
        message => message,
    }
}

fn coarse_replay_has_complete_response_boundaries(
    envelopes: &VecDeque<StreamEnvelope>,
    live_active_responses: &HashSet<String>,
) -> bool {
    let mut active_responses = HashSet::new();
    for envelope in envelopes {
        let StreamEnvelope::Message { message, .. } = envelope else {
            continue;
        };
        match message.as_ref() {
            FromAgentMessage::ResponseStart { response_id } => {
                active_responses.insert(response_id.as_str());
            }
            FromAgentMessage::ResponseChunk { response_id, .. }
                if !active_responses.contains(response_id.as_str()) =>
            {
                return false;
            }
            FromAgentMessage::ResponseEnd { response_id, .. }
                if response_id == "done" && !active_responses.contains(response_id.as_str()) =>
            {
                // The native agent still emits a legacy `done` model boundary.
                // It is neither a streamed response nor a turn terminal.
            }
            FromAgentMessage::ResponseEnd { response_id, .. }
                if !active_responses.remove(response_id.as_str()) =>
            {
                return false;
            }
            _ => {}
        }
    }
    live_active_responses
        .iter()
        .all(|response_id| active_responses.contains(response_id.as_str()))
}

#[derive(Clone, Copy)]
struct PendingControllerRequest<'a> {
    request_type: ServerRequestType,
    pending: &'a crate::headless::PendingApproval,
}

type PendingControllerEventKey<'a> = (u8, &'a str, Option<&'a str>, Option<&'a str>);

const fn server_request_type_key(request_type: ServerRequestType) -> u8 {
    match request_type {
        ServerRequestType::Approval => 0,
        ServerRequestType::ClientTool => 1,
        ServerRequestType::UserInput => 2,
        ServerRequestType::ToolRetry => 3,
    }
}

fn pending_controller_request_key<'a>(
    request: &PendingControllerRequest<'a>,
) -> PendingControllerEventKey<'a> {
    (
        server_request_type_key(request.request_type),
        request.pending.call_id.as_str(),
        request.pending.request_id.as_deref(),
        request.pending.tool_execution_id.as_deref(),
    )
}

fn pending_controller_requests(state: &AgentState) -> Vec<PendingControllerRequest<'_>> {
    [
        (ServerRequestType::Approval, &state.pending_approvals),
        (ServerRequestType::ClientTool, &state.pending_client_tools),
        (ServerRequestType::UserInput, &state.pending_user_inputs),
        (ServerRequestType::ToolRetry, &state.pending_tool_retries),
    ]
    .into_iter()
    .flat_map(|(request_type, pending)| {
        pending.iter().map(move |pending| PendingControllerRequest {
            request_type,
            pending,
        })
    })
    .collect()
}

fn pending_controller_event_key(
    message: &FromAgentMessage,
) -> Option<PendingControllerEventKey<'_>> {
    match message {
        FromAgentMessage::ClientToolRequest {
            call_id,
            tool_execution_id,
            ..
        } => Some((
            server_request_type_key(ServerRequestType::ClientTool),
            call_id,
            None,
            tool_execution_id.as_deref(),
        )),
        FromAgentMessage::GovernedClientToolRequest(tool_wire::GovernedClientToolRequest {
            call_id,
            tool_execution_id,
            ..
        }) => Some((
            server_request_type_key(ServerRequestType::ClientTool),
            call_id,
            None,
            Some(tool_execution_id.as_str()),
        )),
        FromAgentMessage::ServerRequest {
            request_id,
            request_type,
            call_id,
            tool_execution_id,
            ..
        } => Some((
            server_request_type_key(*request_type),
            call_id,
            (request_id != call_id).then_some(request_id.as_str()),
            tool_execution_id.as_deref(),
        )),
        _ => None,
    }
}

fn pending_controller_event_matches(
    request: &PendingControllerRequest<'_>,
    message: &FromAgentMessage,
) -> bool {
    let pending = request.pending;
    match message {
        FromAgentMessage::ClientToolRequest {
            call_id,
            tool_execution_id,
            ..
        } => {
            request.request_type == ServerRequestType::ClientTool
                && pending.request_id.is_none()
                && pending.call_id == *call_id
                && pending.tool_execution_id == *tool_execution_id
        }
        FromAgentMessage::GovernedClientToolRequest(tool_wire::GovernedClientToolRequest {
            call_id,
            tool_execution_id,
            ..
        }) => {
            request.request_type == ServerRequestType::ClientTool
                && pending.request_id.is_none()
                && pending.call_id == *call_id
                && pending.tool_execution_id.as_deref() == Some(tool_execution_id.as_str())
        }
        FromAgentMessage::ServerRequest {
            request_id,
            request_type,
            call_id,
            tool_execution_id,
            ..
        } => {
            let request_id = (request_id != call_id).then_some(request_id.as_str());
            request.request_type == *request_type
                && pending.call_id == *call_id
                && pending.request_id.as_deref() == request_id
                && tool_execution_id.as_ref().is_none_or(|raw_execution_id| {
                    pending.tool_execution_id.as_ref() == Some(raw_execution_id)
                })
        }
        _ => false,
    }
}

fn redacted_pending_snapshot(
    live: Option<&[crate::headless::PendingApproval]>,
    restored: Option<&[serde_json::Value]>,
) -> Vec<serde_json::Value> {
    live.filter(|requests| !requests.is_empty())
        .map(redacted_pending_requests)
        .or_else(|| {
            restored
                .filter(|requests| !requests.is_empty())
                .map(redacted_pending_request_values)
        })
        .unwrap_or_default()
}

fn redacted_pending_requests(
    requests: &[crate::headless::PendingApproval],
) -> Vec<serde_json::Value> {
    requests
        .iter()
        .map(|request| {
            let mut redacted = serde_json::Map::new();
            redacted.insert(
                "call_id".to_string(),
                serde_json::Value::String(request.call_id.clone()),
            );
            if let Some(tool_execution_id) = request.tool_execution_id.as_ref() {
                redacted.insert(
                    "tool_execution_id".to_string(),
                    serde_json::Value::String(tool_execution_id.clone()),
                );
            }
            if let Some(request_id) = request.request_id.as_ref() {
                redacted.insert(
                    "request_id".to_string(),
                    serde_json::Value::String(request_id.clone()),
                );
            }
            redacted.insert(
                "tool".to_string(),
                serde_json::Value::String(request.tool.clone()),
            );
            redacted.insert("args".to_string(), serde_json::json!({}));
            if let Some(started_at_ms) = request.started_at_ms {
                redacted.insert(
                    "started_at_ms".to_string(),
                    serde_json::json!(started_at_ms),
                );
            }
            serde_json::Value::Object(redacted)
        })
        .collect()
}

fn redacted_active_tool_snapshot(
    live: Option<&crate::headless::AgentState>,
    restored: Option<&RuntimeStateSnapshot>,
) -> Vec<serde_json::Value> {
    live.filter(|state| !state.active_tools.is_empty())
        .map(|state| {
            state
                .active_tools
                .values()
                .map(|tool| {
                    serde_json::json!({
                        "call_id": tool.call_id,
                        "tool": tool.tool,
                        "output": "",
                    })
                })
                .collect()
        })
        .or_else(|| {
            restored
                .filter(|state| !state.active_tools.is_empty())
                .map(|state| redacted_active_tools(&state.active_tools))
        })
        .unwrap_or_default()
}

fn redacted_pending_request_values(requests: &[serde_json::Value]) -> Vec<serde_json::Value> {
    requests
        .iter()
        .map(redacted_pending_request_value)
        .collect()
}

fn redacted_pending_request_value(request: &serde_json::Value) -> serde_json::Value {
    let mut redacted = serde_json::Map::new();
    redacted.insert(
        "call_id".to_string(),
        request
            .get("call_id")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    insert_optional_json_field(&mut redacted, request, "tool_execution_id");
    if let Some(request_id) = request
        .get("request_id")
        .or_else(|| request.get("id"))
        .filter(|value| !value.is_null())
    {
        redacted.insert("request_id".to_string(), request_id.clone());
    }
    redacted.insert(
        "tool".to_string(),
        request
            .get("tool")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    redacted.insert("args".to_string(), serde_json::json!({}));
    insert_optional_json_field(&mut redacted, request, "started_at_ms");
    serde_json::Value::Object(redacted)
}

fn insert_optional_json_field(
    target: &mut serde_json::Map<String, serde_json::Value>,
    source: &serde_json::Value,
    key: &str,
) {
    if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
        target.insert(key.to_string(), value.clone());
    }
}

fn redacted_active_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "call_id": tool.get("call_id").cloned().unwrap_or(serde_json::Value::Null),
                "tool": tool.get("tool").cloned().unwrap_or(serde_json::Value::Null),
                "output": "",
            })
        })
        .collect()
}

#[cfg(test)]
mod coarse_replay_tests {
    use super::*;

    fn response_end(response_id: &str) -> StreamEnvelope {
        StreamEnvelope::Message {
            cursor: 1,
            message: Box::new(FromAgentMessage::ResponseEnd {
                response_id: response_id.to_string(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            }),
        }
    }

    #[test]
    fn lifecycle_done_does_not_require_a_response_start() {
        let envelopes = VecDeque::from([response_end("done")]);

        assert!(coarse_replay_has_complete_response_boundaries(
            &envelopes,
            &HashSet::new(),
        ));
    }

    #[test]
    fn unmatched_model_response_end_remains_incomplete() {
        let envelopes = VecDeque::from([response_end("response-1")]);

        assert!(!coarse_replay_has_complete_response_boundaries(
            &envelopes,
            &HashSet::new(),
        ));
    }

    #[test]
    fn matched_done_closes_its_response_boundary() {
        let envelopes = VecDeque::from([
            StreamEnvelope::Message {
                cursor: 1,
                message: Box::new(FromAgentMessage::ResponseStart {
                    response_id: "done".to_string(),
                }),
            },
            response_end("done"),
            StreamEnvelope::Message {
                cursor: 3,
                message: Box::new(FromAgentMessage::ResponseChunk {
                    response_id: "done".to_string(),
                    content: "after completion".to_string(),
                    is_thinking: false,
                }),
            },
        ]);

        assert!(!coarse_replay_has_complete_response_boundaries(
            &envelopes,
            &HashSet::new(),
        ));
    }
}
