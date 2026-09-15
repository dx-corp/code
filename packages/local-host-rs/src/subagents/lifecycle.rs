//! Child execution, terminal recording, and lifecycle publication.
use super::*;

impl SubagentManager {
    pub(super) async fn run_child(
        &self,
        mut record: SubagentRecord,
        run: ChildRun,
        launch: ChildLaunch,
    ) -> Result<SubagentRecord, String> {
        let ChildRun {
            prompt,
            mut history,
            sandbox_policy,
            token,
            mut control_rx,
        } = run;
        let ChildLaunch {
            lease,
            credential_vault,
            parent_credential_vault,
            parent_credential_generation,
            parent_cancel: _,
        } = launch;
        let parent_credential_scope = ParentCredentialScope {
            vault: &parent_credential_vault,
            generation: parent_credential_generation,
        };
        let _permit = tokio::select! {
            biased;
            () = token.cancelled() => {
                return self.finish_record(
                    record,
                    SubagentStatus::Cancelled,
                    None,
                    Some("subagent cancelled while queued".to_string()),
                    &credential_vault,
                    &parent_credential_scope,
                );
            }
            permit = self.runtime.acquire_permit() => permit.map_err(|error| {
                format!("acquire subagent scheduler permit: {error}")
            })?,
        };
        let _lease = match lease {
            Some(lease) => lease,
            None => SessionLock::acquire(&Self::timeline_path(&record)).map_err(|error| {
                format!("acquire subagent {} execution lease: {error}", record.id)
            })?,
        };
        let child_cwd = deserialize_repository_path(&record.cwd);
        if record.attempt > 0 {
            let (initial_paths, initial_fingerprints) = changed_file_baseline(&child_cwd);
            let (initial_files, initial_file_fingerprints) =
                serialize_file_baseline(initial_paths, initial_fingerprints);
            record.initial_files = initial_files;
            record.initial_file_fingerprints = initial_file_fingerprints;
            record.initial_head = git_repository_head(&child_cwd);
        }
        record.status = SubagentStatus::Running;
        record.started_at_ms = Some(now_millis());
        self.write_record(&record)?;

        let session_dir = Self::session_dir(&record);
        let mut recorder = match SessionRecorder::resume(&session_dir, &record.id) {
            Ok(recorder) => recorder,
            Err(error) => {
                return self.finish_record(
                    record,
                    SubagentStatus::Failed,
                    None,
                    Some(format!("open child transcript: {error}")),
                    &credential_vault,
                    &parent_credential_scope,
                );
            }
        };
        let snapshot_attempt = recorder
            .semantic_conversation_attempt()
            .or(record.snapshot_attempt);
        let processed_queue_ids = recorder.semantic_processed_queue_ids().clone();
        let replay_receipts = self
            .control_receipts(&record)
            .into_iter()
            .filter(|receipt| {
                control_receipt_needs_replay(receipt, snapshot_attempt, &processed_queue_ids)
            })
            .filter(|receipt| matches!(receipt.mode, ControlMode::Steer | ControlMode::Followup));
        for receipt in replay_receipts {
            let message = crate::ai::Message {
                role: crate::ai::Role::User,
                content: crate::ai::MessageContent::text(receipt.body),
            };
            history.get_or_insert_with(Vec::new).push(message);
        }

        let platform_session = match crate::credential_mode::detect() {
            Ok(crate::credential_mode::DetectedMode::Platform(session)) => Some(session),
            _ => None,
        };
        let managed_setup =
            crate::managed_setup::ManagedSetupClient::resolve(platform_session.as_ref());
        let managed_mcp_policy = managed_setup
            .is_managed()
            .then(|| crate::mcp::ManagedMcpPolicy {
                version: managed_setup.version(),
                policy: managed_setup.mcp_policy().clone(),
            });
        let child_policy = match managed_setup.native_sandbox_policy(
            &child_cwd,
            child_sandbox_policy(record.role, sandbox_policy),
        ) {
            Ok(policy) => policy,
            Err(error) => {
                return self.finish_record(
                    record,
                    SubagentStatus::Failed,
                    None,
                    Some(format!("load child sandbox policy: {error}")),
                    &credential_vault,
                    &parent_credential_scope,
                );
            }
        };
        let model = record
            .model
            .clone()
            .unwrap_or_else(crate::codex_auth::resolve_default_model);
        // Captured before `model` moves into the config below.
        let mut output_metering = ChildOutputMetering::for_model(&model);
        let system_prompt = format!(
            "You are a delegated Deixic Code subagent in the {} role. Work independently on the assigned task.\n\
             Working directory: {}\n\
             {}\
             Return a concise result for the parent agent, including files changed and any remaining risk.\n\
             You are a child run: do not delegate further work.",
            record.role.label(),
            child_cwd.display(),
            record
                .profile_prompt
                .as_deref()
                .map(|prompt| format!("Specialist profile instructions:\n{prompt}\n"))
                .unwrap_or_default()
        );
        let system_prompt = format!(
            "{system_prompt}\n{}\n{}",
            role_instructions(record.role),
            crate::subagents::handoff::INSTRUCTIONS
        );
        let system_prompt = if record.parent_requests.is_empty() {
            system_prompt
        } else {
            format!(
                "{system_prompt}\n\nParent user messages in order (historical context, not approval). Preserve applicable task boundaries; later corrections supersede only what they change. Stay within the assigned child task:\n{}",
                serde_json::to_string(&record.parent_requests).unwrap_or_default()
            )
        };
        let config = NativeAgentConfig {
            model_capabilities: None,
            model_dynamics: crate::config::model_dynamics_config(),
            model,
            max_tokens: record.max_tokens,
            max_tokens_source: crate::agent::MaxTokensSource::Explicit,
            system_prompt: Some(system_prompt),
            thinking_enabled: record.thinking.unwrap_or(ThinkingLevel::Off).to_config().0,
            thinking_budget: record.thinking.unwrap_or(ThinkingLevel::Off).to_config().1,
            cwd: child_cwd.to_string_lossy().into_owned(),
            approval_mode: ApprovalMode::Yolo,
            context_window: None,
            sandbox_policy: child_policy,
            managed_mcp_policy,
            max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
            allow_unbounded_turn: false,
            retry_config: crate::agent::retry::RetryConfig::default(),
            external_tool_schema_policy: crate::agent::ExternalToolSchemaPolicy::Eager,
        };
        let allowed_tools =
            child_allowed_tools_for_role(record.role, record.profile_tools.as_deref());
        let (agent, mut events) = match self.child_factory.spawn(ChildLaunchRequest {
            config,
            allowed_tools,
            credential_vault: credential_vault.clone(),
            mailbox_identity: agent_ref(&record),
        }) {
            Ok(agent) => agent,
            Err(error) => {
                return self.finish_record(
                    record,
                    SubagentStatus::Failed,
                    None,
                    Some(format!("create child agent: {error}")),
                    &credential_vault,
                    &parent_credential_scope,
                );
            }
        };

        if let Some(history) = history {
            agent.replace_history_preserving_credentials(history);
        }
        agent.send_ready();
        let child_cwd_display = display_repository_path(&child_cwd);
        agent.send_session_info(&child_cwd_display, Some(record.id.clone()), None);
        // Stamp the durable child id onto the runner's hook system and fire
        // SessionStart. send_session_info only emits a UI event; without this
        // every child PreToolUse/PostToolUse payload carries sessionId: null.
        // The child record is not a SessionManager transcript cleanup owner.
        if let Err(error) =
            agent.set_session_context(Some(record.id.clone()), "subagent_start", false)
        {
            agent.shutdown().await;
            return self.finish_record(
                record,
                SubagentStatus::Failed,
                None,
                Some(format!("set child session context: {error}")),
                &credential_vault,
                &parent_credential_scope,
            );
        }
        // Hand the whole-run allowance to the runner before the prompt that
        // spends it. The runner subtracts each response and clamps the request
        // it is about to build, so the cap does not depend on a per-response
        // update arriving before the next request is built.
        if let Err(error) = agent.set_output_token_budget(record.max_tokens) {
            agent.shutdown().await;
            return self.finish_record(
                record,
                SubagentStatus::Failed,
                None,
                Some(format!("set child output budget: {error}")),
                &credential_vault,
                &parent_credential_scope,
            );
        }
        let execution_prompt = credential_vault.resolve_all(&prompt);
        if let Err(error) = agent.prompt(execution_prompt, Vec::new()).await {
            agent.shutdown().await;
            return self.finish_record(
                record,
                SubagentStatus::Failed,
                None,
                Some(format!("start child prompt: {error}")),
                &credential_vault,
                &parent_credential_scope,
            );
        }

        let mut current_output = String::new();
        let mut last_output = String::new();
        let mut terminal_seen = false;
        let mut semantic_snapshot_seen = false;
        let mut cancelled = false;
        let mut interrupted = false;
        let mut timed_out = false;
        let mut output_tokens_used = 0_u64;
        // Assistant characters streamed since the last response boundary, used
        // to charge runtimes that report no usage.
        let mut streamed_output_chars = 0_u64;
        let mut run_error = None;
        let mut recording_error = None;
        let deadline = tokio::time::sleep(Duration::from_millis(record.timeout_ms));
        tokio::pin!(deadline);
        let mut control_poll = tokio::time::interval(Duration::from_millis(250));
        control_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let event = if terminal_seen {
                tokio::time::timeout(TERMINAL_SNAPSHOT_WAIT, events.recv())
                    .await
                    .unwrap_or_default()
            } else {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        cancelled = true;
                        agent.cancel();
                        break;
                    }
                    () = &mut deadline => {
                        timed_out = true;
                        agent.cancel();
                        break;
                    }
                    request = control_rx.recv() => {
                        if let Some(request) = request {
                            match self.apply_child_control(
                                &agent,
                                &mut recorder,
                                &record,
                                &credential_vault,
                                request,
                                false,
                            ).await {
                                ChildControlOutcome::Continue => {}
                                ChildControlOutcome::Interrupted => {
                                    interrupted = true;
                                    break;
                                }
                                ChildControlOutcome::Cancelled => {
                                    cancelled = true;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    _ = control_poll.tick() => {
                        if let Some(request) = self.claim_durable_control(&record).await {
                            match self.apply_child_control(
                                &agent,
                                &mut recorder,
                                &record,
                                &credential_vault,
                                request,
                                true,
                            ).await {
                                ChildControlOutcome::Continue => {}
                                ChildControlOutcome::Interrupted => {
                                    interrupted = true;
                                    break;
                                }
                                ChildControlOutcome::Cancelled => {
                                    cancelled = true;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    event = events.recv() => event,
                }
            };

            let Some(event) = event else {
                if !terminal_seen && !cancelled && run_error.is_none() {
                    run_error =
                        Some("child agent event stream ended before completion".to_string());
                }
                break;
            };

            match persist_child_event(
                &mut recorder,
                &event,
                &record.id,
                &credential_vault,
                record.attempt,
            ) {
                Ok(snapshot_persisted) => semantic_snapshot_seen |= snapshot_persisted,
                Err(error) => {
                    recording_error = Some(format!("persist child event: {error}"));
                    agent.cancel();
                    break;
                }
            }

            match event {
                FromAgent::ModelChanged { model, .. } => {
                    record.model = Some(model);
                }
                FromAgent::BoostChanged {
                    thinking: Some(thinking),
                    ..
                } => {
                    record.thinking = Some(thinking);
                }
                FromAgent::ResponseChunk {
                    content,
                    is_thinking,
                    ..
                } => {
                    // Thinking text is billed as output too, so it counts
                    // against the budget even though it is not the child's
                    // answer.
                    streamed_output_chars =
                        streamed_output_chars.saturating_add(content.chars().count() as u64);
                    if !is_thinking {
                        current_output.push_str(&content);
                    }
                    // Only an unmetered runtime is policed mid-stream. A
                    // metered one is bounded per request by the runner's clamp
                    // and charged exactly at the boundary, so estimating here
                    // could only cancel a response the budget still allowed.
                    if output_metering.enforces_mid_stream()
                        && child_output_budget_exhausted(
                            output_tokens_used,
                            streamed_output_chars,
                            record.max_tokens,
                        )
                    {
                        run_error = Some(format!(
                            "subagent exhausted its cumulative {} output-token budget",
                            record.max_tokens
                        ));
                        agent.cancel();
                        break;
                    }
                }
                FromAgent::ResponseEnd { response_id, usage } => {
                    let budget_exhausted = record_child_output_tokens(
                        &mut output_tokens_used,
                        usage.as_ref(),
                        output_metering.estimate_for_turn(usage.is_some(), streamed_output_chars),
                        record.max_tokens,
                    );
                    streamed_output_chars = 0;
                    if budget_exhausted && response_id != "done" {
                        run_error = Some(format!(
                            "subagent exhausted its cumulative {} output-token budget",
                            record.max_tokens
                        ));
                        agent.cancel();
                        break;
                    }
                    if response_id != "done" {
                        if !current_output.is_empty() {
                            last_output.clone_from(&current_output);
                            current_output.clear();
                        }
                        // A non-terminal response boundary means a previous
                        // recoverable tool error did not prevent progress.
                        run_error = None;
                        // The next request's allowance needs no update here:
                        // the runner holds the whole-run budget sent before the
                        // prompt and clamps each request it builds. Lowering it
                        // from this loop raced the request the child had
                        // already started building.
                    }
                }
                FromAgent::TurnCompleted { .. } => {
                    terminal_seen = true;
                }
                FromAgent::TurnInterrupted { reason, .. } => {
                    run_error = Some(reason);
                    terminal_seen = true;
                }
                FromAgent::ProviderError { kind, message } => {
                    run_error = Some(format!("provider failure ({kind:?}): {message}"));
                    terminal_seen = true;
                }
                FromAgent::ToolCall {
                    call_id,
                    tool,
                    requires_approval: true,
                    ..
                } => {
                    let reason = format!(
                        "child tool `{tool}` requires approval, which delegated runs cannot request"
                    );
                    let _ = agent.tool_response_sender().send((
                        call_id,
                        false,
                        Some(ToolResult::failure(reason.clone())),
                        crate::agent::ExecutionSource::Native,
                        None,
                    ));
                    run_error = Some(reason);
                    agent.cancel();
                    break;
                }
                FromAgent::CodexNativeOperation {
                    method,
                    output_chars,
                } => {
                    // Codex runs `commandExecution` and `fileChange` itself, so
                    // these never arrive as `ToolCall`. Without this arm a child
                    // could run repeated large native operations with no
                    // assistant text and never reach its budget.
                    let _ = method;
                    streamed_output_chars = streamed_output_chars.saturating_add(output_chars);
                    if output_metering.enforces_mid_stream()
                        && child_output_budget_exhausted(
                            output_tokens_used,
                            streamed_output_chars,
                            record.max_tokens,
                        )
                    {
                        run_error = Some(format!(
                            "subagent exhausted its cumulative {} output-token budget",
                            record.max_tokens
                        ));
                        agent.cancel();
                        break;
                    }
                }
                FromAgent::ToolCall { tool, args, .. } => {
                    // A tool call is model-produced output even though it never
                    // arrives as assistant text. Counting only `ResponseChunk`
                    // let a child emit large `write`/`edit` arguments, or call
                    // tools with no prose at all, and never reach its budget;
                    // the time limit was the only thing that stopped it.
                    streamed_output_chars =
                        streamed_output_chars.saturating_add(tool_call_output_chars(&tool, &args));
                    if output_metering.enforces_mid_stream()
                        && child_output_budget_exhausted(
                            output_tokens_used,
                            streamed_output_chars,
                            record.max_tokens,
                        )
                    {
                        run_error = Some(format!(
                            "subagent exhausted its cumulative {} output-token budget",
                            record.max_tokens
                        ));
                        agent.cancel();
                        break;
                    }
                }
                FromAgent::Error {
                    message,
                    fatal,
                    terminal,
                    ..
                } if fatal || terminal => {
                    run_error = Some(message);
                    break;
                }
                FromAgent::CodexSessionState { .. }
                | FromAgent::CodexTurnState { .. }
                | FromAgent::CodexUsageState { .. }
                | FromAgent::CodexCompatibility { .. } => {}
                _ => {}
            }

            if terminal_checkpoint_ready(terminal_seen, semantic_snapshot_seen) {
                break;
            }
        }

        if token.is_cancelled() {
            cancelled = true;
            agent.cancel();
        }
        agent.shutdown().await;
        if let Err(error) = drain_child_events(
            &mut recorder,
            &mut events,
            &record.id,
            &credential_vault,
            record.attempt,
        ) {
            recording_error = Some(format!("persist shutdown child event: {error}"));
        }
        let checkpoint_flushed = match recorder.flush_checkpoint() {
            Ok(()) => true,
            Err(error) => {
                recording_error = Some(format!("flush child transcript: {error}"));
                false
            }
        };
        if checkpoint_flushed {
            if let Some(snapshot_attempt) = recorder.semantic_conversation_attempt() {
                record.snapshot_attempt = Some(snapshot_attempt);
            }
        }

        let output = if current_output.is_empty() {
            last_output
        } else {
            current_output
        };
        let output = credential_vault.vault_in_text(&output);
        let files_modified = changed_files_since(
            &child_cwd,
            record.initial_head.as_deref(),
            &record.initial_files,
            &record.initial_file_fingerprints,
        );
        let (status, error) = child_terminal_status(
            cancelled,
            interrupted,
            timed_out,
            recording_error.or(run_error),
            record.timeout_ms,
        );
        self.finish_record(
            record,
            status,
            Some(SubagentResult {
                output,
                files_modified,
            }),
            error,
            &credential_vault,
            &parent_credential_scope,
        )
    }
    pub(super) fn finish_record(
        &self,
        mut record: SubagentRecord,
        status: SubagentStatus,
        result: Option<SubagentResult>,
        error: Option<String>,
        credential_vault: &CredentialVault,
        parent_credential_scope: &ParentCredentialScope<'_>,
    ) -> Result<SubagentRecord, String> {
        record.status = status;
        let finished_at_ms = now_millis();
        record.finished_at_ms = Some(finished_at_ms);
        let vaulted_result = result.map(|mut result| {
            result.output = credential_vault.vault_in_text(&result.output);
            result
        });
        let vaulted_error = error.map(|error| credential_vault.vault_in_text(&error));
        let credential_reference_map = parent_credential_scope
            .vault
            .absorb_child_credentials_at_generation(
                credential_vault,
                parent_credential_scope.generation,
            );
        record.result = vaulted_result.map(|mut result| {
            result.output =
                CredentialVault::translate_references(&result.output, &credential_reference_map);
            result
        });
        record.error = vaulted_error
            .map(|error| CredentialVault::translate_references(&error, &credential_reference_map));

        let duration_ms = record
            .started_at_ms
            .map(|started_at_ms| finished_at_ms.saturating_sub(started_at_ms))
            .unwrap_or_default();
        let result_text = record.result.as_ref().map(|result| result.output.as_str());
        let mut hooks = IntegratedHookSystem::load_from_config(&self.cwd.to_string_lossy());
        // Local load skips the runner's SetSessionContext wiring, so stamp the
        // raw parent session id (not the `session:` routing scope) before
        // dispatching so payloads match every other hook in that session.
        let hook_session =
            crate::agent::ParentScopeId::from_raw(&record.last_parent_scope_id).hook_session_id();
        hooks.set_session_id(Some(hook_session.into_string()));
        let _ = hooks.execute_subagent_stop(
            record.role.label(),
            &record.id,
            result_text,
            duration_ms,
            status == SubagentStatus::Completed,
        );
        // Address the completion to the scope and call that most recently
        // launched this child, not the original spawn. After a resume from a
        // different app or executor — the restart case — the spawning scope no
        // longer has a consumer, so an event queued under it is never polled
        // and the current parent never learns the child finished.
        match self.write_record(&record) {
            Ok(()) => {
                self.seal_coding_validator_record(&record);
                if let Err(error) = self.publish_lifecycle_notification(&mut record) {
                    self.pending_lifecycle
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(record.id.clone());
                    eprintln!(
                        "subagent {} completed, but its lifecycle notification is pending retry: {error}",
                        record.id
                    );
                }
                Ok(record)
            }
            Err(error) => Err(format!(
                "persist terminal subagent record {}: {error}",
                record.id
            )),
        }
    }
    pub(super) fn publish_lifecycle_notification(
        &self,
        record: &mut SubagentRecord,
    ) -> Result<(), String> {
        if !record.status.is_terminal() {
            return Err(format!(
                "subagent {} is not terminal and cannot publish a lifecycle notification",
                record.id
            ));
        }
        let finished_at_ms = record.finished_at_ms.unwrap_or_else(now_millis);
        let summary = record.result.as_ref().map(|result| {
            if record.backend == SubagentBackend::Native {
                crate::subagents::handoff::notification(&result.output)
            } else {
                result.output.trim().chars().take(500).collect::<String>()
            }
        });
        let mut mailbox = crate::mailbox::MailboxStore::with_path(&self.mailbox_path);
        mailbox
            .send_typed(
                agent_ref(record),
                record.last_parent_scope_id.clone(),
                format!(
                    "Subagent {} attempt {} finished with status {}",
                    record.id,
                    record.attempt,
                    status_label(record.status)
                ),
                crate::mailbox::MailboxPayload::SubagentLifecycle {
                    subagent_id: record.id.clone(),
                    parent_call_id: record.last_call_id.clone(),
                    attempt: record.attempt,
                    status: record.status.into(),
                    summary,
                    error: record.error.clone(),
                    finished_at_ms,
                },
                crate::mailbox::MailboxDeliveryState::Queued,
                Some(format!(
                    "lifecycle:{}:{}:{}",
                    record.id,
                    record.attempt,
                    status_label(record.status)
                )),
            )
            .map_err(|error| {
                format!(
                    "persist terminal notification for subagent {}: {error}",
                    record.id
                )
            })?;
        record.lifecycle_notification_published = true;
        self.write_record(record)
    }
}
