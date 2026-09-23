//! Actor command handling and queue lifecycle.

use super::*;

impl NativeAgentRunner {
    pub(super) async fn take_deferred_command(
        &mut self,
    ) -> Option<(AgentCommand, Option<CancellationToken>)> {
        if self.deferred_commands.is_empty() {
            return None;
        }

        // The command drain can await hook state changes, so do not hold the
        // synchronous cancellation mutex across it. A cancellation that races
        // this drain still reaches the token installed below directly.
        let _ = self.drain_pending_commands().await;
        let command = self.deferred_commands.pop_front()?;
        let request_token = match &command {
            AgentCommand::Prompt { kind, .. } if prompt_kind_starts_main_request(*kind) => {
                let token = self
                    .active_cancellation
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .activate_request();
                self.cancel_token = Some(token.clone());
                Some(token)
            }
            _ => None,
        };
        Some((command, request_token))
    }
    pub(super) async fn activate_received_command(
        &mut self,
        command: AgentCommand,
    ) -> (AgentCommand, Option<CancellationToken>) {
        let starts_main_request = matches!(
            &command, AgentCommand::Prompt { kind, .. } if prompt_kind_starts_main_request(*kind)
        );
        if !starts_main_request {
            return (command, None);
        }

        // Install the token before draining so a cancellation that races the
        // asynchronous hook updates can cancel the active request directly.
        let token = self
            .active_cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .activate_request();
        self.cancel_token = Some(token.clone());
        let _ = self.drain_pending_commands().await;
        (command, Some(token))
    }
    pub(super) fn enqueue_pending_prompt(
        &mut self,
        content: String,
        attachments: Vec<String>,
        kind: PromptKind,
        queue_id: Option<u64>,
        managed_request_lineage: Option<String>,
        managed_inference_authorization: Option<ManagedInferenceAuthorization>,
    ) {
        let id = queue_id.unwrap_or_else(|| self.pending_messages.reserve_id());
        let pending = if kind == PromptKind::Steer {
            PendingMessage::urgent_with_kind_and_id_and_attachments(content, kind, id, attachments)
        } else {
            PendingMessage::with_kind_and_id_and_attachments(content, kind, id, attachments)
        }
        .with_managed_request_lineage(managed_request_lineage)
        .with_managed_inference_authorization(managed_inference_authorization);
        let dropped = self.pending_messages.push_message(pending);
        if let Some(dropped) = dropped {
            let _ = self.event_tx.send(FromAgent::Status {
                message: format!("Queue full, dropped oldest {}", dropped.kind.label()),
            });
        }
        let stats = self.pending_messages.stats();
        let label = kind.label();
        let _ = self.event_tx.send(FromAgent::Status {
            message: if stats.pending_count == 1 {
                format!("Queued {label} #{id} (1 pending)")
            } else {
                format!("Queued {} #{} ({} pending)", label, id, stats.pending_count)
            },
        });
    }
    pub(super) fn requeue_follow_up_front(
        &mut self,
        content: String,
        attachments: Vec<String>,
        queue_id: u64,
        managed_request_lineage: Option<String>,
    ) {
        let pending = PendingMessage::with_kind_and_id_and_attachments(
            content,
            PromptKind::FollowUp,
            queue_id,
            attachments,
        )
        .with_managed_request_lineage(managed_request_lineage);
        let dropped = self.pending_messages.push_message_front_of_kind(pending);
        if let Some(dropped) = dropped {
            let _ = self.event_tx.send(FromAgent::Status {
                message: format!("Queue full, dropped oldest {}", dropped.kind.label()),
            });
        }
    }
    pub(super) async fn drain_pending_commands(&mut self) -> bool {
        let mut cancelled = false;
        while let Ok(cmd) = self.command_rx.try_recv() {
            match cmd {
                AgentCommand::ApplySelectiveSummary { reply, .. } => {
                    let _ = reply.send(Err(anyhow::anyhow!(
                        "Wait for the current turn and queued messages to finish"
                    )));
                }
                AgentCommand::SelectiveSummaryPreview { reply } => {
                    let _ = reply.send(Err(anyhow::anyhow!(
                        "Wait for the current turn and queued messages to finish"
                    )));
                }
                AgentCommand::SelectiveSummary { reply, .. } => {
                    let _ = reply.send(crate::agent::SelectiveSummaryOutcome {
                        usage: None,
                        result: Err(anyhow::anyhow!(
                            "Wait for the current turn and queued messages to finish"
                        )),
                    });
                }
                AgentCommand::Prompt {
                    content,
                    attachments,
                    kind,
                    queue_id,
                    managed_request_lineage,
                    managed_inference_authorization,
                } => {
                    let managed_request_lineage = match self
                        .resolve_managed_request_lineage(managed_request_lineage)
                        .await
                    {
                        Ok(lineage) => lineage,
                        Err(error) => {
                            self.reject_managed_request(error);
                            continue;
                        }
                    };
                    if should_defer_prompt_command(kind, cancelled) {
                        self.deferred_commands.push_back(AgentCommand::Prompt {
                            content,
                            attachments,
                            kind,
                            queue_id,
                            managed_request_lineage,
                            managed_inference_authorization,
                        });
                    } else {
                        self.enqueue_pending_prompt(
                            content,
                            attachments,
                            kind,
                            queue_id,
                            managed_request_lineage,
                            managed_inference_authorization,
                        );
                    }
                }
                AgentCommand::Cancel { clear_pending } => {
                    self.clear_pending_on_cancel = clear_pending;
                    if clear_pending {
                        let cleared = self.pending_messages.clear();
                        let cleared_stashed = clear_stashed_prompts(&mut self.deferred_commands);
                        let cleared_count = cleared.len() + cleared_stashed;
                        if cleared_count != 0 {
                            let _ = self.event_tx.send(FromAgent::Status {
                                message: format!("Cleared {cleared_count} pending message(s)"),
                            });
                        }
                    }
                    self.reject_pending_tool_responses_on_cancel();
                    cancelled = true;
                }
                AgentCommand::CancelQueued { id } => {
                    // The staged system prompt is not keyed by id and stays
                    // staged: the skills it carries are still active in the UI,
                    // so the next message to start should see them.
                    if let Some(removed) = self.pending_messages.remove_by_id(id) {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message: format!(
                                "Removed queued {} #{}",
                                removed.kind.label(),
                                removed.id
                            ),
                        });
                    } else {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message: format!("No queued prompt found with id #{id}"),
                        });
                    }
                }
                AgentCommand::ReorderQueued { id, placement } => {
                    if !self.pending_messages.move_by_id(id, placement) {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message: format!("No queued prompt found with id #{id}"),
                        });
                    }
                }
                AgentCommand::RequeueFollowUpFront {
                    content,
                    attachments,
                    queue_id,
                    managed_request_lineage,
                } => match self
                    .resolve_managed_request_lineage(managed_request_lineage)
                    .await
                {
                    Ok(lineage) => {
                        self.requeue_follow_up_front(content, attachments, queue_id, lineage);
                    }
                    Err(error) => self.reject_managed_request(error),
                },
                AgentCommand::SetContextToolExcluded { name, excluded } => {
                    self.set_context_tool_excluded(&name, excluded);
                }
                AgentCommand::Boost => {
                    let mut state = self.dynamics.lock().expect("model dynamics mutex");
                    if !state.used {
                        state.requested = true;
                        state.status = crate::agent::model_dynamics::BoostStatus::Pending;
                        let _ = self.event_tx.send(FromAgent::BoostChanged {
                            status: state.status,
                            thinking: None,
                        });
                    }
                }
                AgentCommand::SetThinking { enabled, budget } => {
                    self.preserve_explicit_intelligence_choice();
                    self.config.thinking_enabled = enabled;
                    self.config.thinking_budget = budget;
                }
                AgentCommand::RefreshModelBudgets => {
                    let model = self.config.model.clone();
                    refresh_model_budgets_with_host(
                        &self.tool_executor,
                        &mut self.config,
                        &mut self.compactor,
                        &model,
                    );
                }
                AgentCommand::SetMaxTokens { max_tokens } => {
                    set_explicit_max_tokens(&mut self.config, max_tokens);
                }
                AgentCommand::InstallProcessBudget {
                    limits,
                    checkpoint,
                    applied,
                } => {
                    let result = self.apply_process_budget(limits, checkpoint);
                    let _ = applied.send(result);
                }
                AgentCommand::ClearProcessBudget {
                    system_prompt,
                    applied,
                } => {
                    let result = self.retire_process_budget(system_prompt);
                    let _ = applied.send(result);
                }
                AgentCommand::SetOutputTokenBudget {
                    max_total_output_tokens,
                } => {
                    self.output_token_budget = Some(max_total_output_tokens);
                }
                AgentCommand::SetSubagentParentScope { parent_scope_id } => {
                    self.tool_executor
                        .set_subagent_parent_scope(parent_scope_id);
                }
                AgentCommand::SetSessionContext {
                    session_id,
                    transcript_path,
                    reason,
                    owns_persistent_tool_spills,
                    preserve_compacted_checkpoint,
                } => {
                    self.apply_session_context(
                        session_id,
                        transcript_path,
                        &reason,
                        owns_persistent_tool_spills,
                        preserve_compacted_checkpoint,
                    )
                    .await;
                }
                AgentCommand::SetHookLogFile { path } => {
                    self.hooks.hook_set_log_file(Some(path)).await;
                }
                AgentCommand::SetHooksEnabled { enabled } => {
                    self.hooks.hook_set_enabled(enabled).await;
                }
                AgentCommand::ReloadHooks => {
                    if let Err(error) = self.hooks.hook_reload().await {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: format!("Failed to reload hooks: {error}"),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                    }
                }
                AgentCommand::InspectHooks { reply } => {
                    let _ = reply.send(self.hooks.hook_runtime_snapshot().await);
                }
                AgentCommand::SetGoalToolsVisible { visible } => {
                    self.set_goal_tools_visible(visible);
                }
                AgentCommand::SetApprovalMode { mode } => {
                    self.config.approval_mode = mode;
                }
                AgentCommand::ReplaceGovernedTools {
                    allowed_tools,
                    external_tool_definitions,
                } => {
                    self.replace_governed_tools(&allowed_tools, external_tool_definitions);
                }
                AgentCommand::SetSteeringMode { mode } => {
                    self.steering_mode = mode;
                }
                AgentCommand::SetFollowUpMode { mode } => {
                    self.follow_up_mode = mode;
                }
                AgentCommand::SetSystemPrompt { system_prompt } => {
                    self.config.system_prompt = Some(system_prompt);
                    self.system_prompt_revision = self.system_prompt_revision.saturating_add(1);
                    self.runtime_prompt_revision = self.runtime_prompt_revision.saturating_add(1);
                    self.refresh_runtime_audit();
                }
                AgentCommand::SetSystemPromptForQueuedPrompt {
                    queue_id,
                    system_prompt,
                } => {
                    self.queued_system_prompts
                        .insert(queue_id, (self.system_prompt_revision, system_prompt));
                }
                AgentCommand::InjectUserNote {
                    content,
                    applied,
                    consumed,
                } => {
                    // Defer until idle so we never insert a user message mid-tool-loop.
                    self.deferred_commands
                        .push_back(AgentCommand::InjectUserNote {
                            content,
                            applied,
                            consumed,
                        });
                }
                other => {
                    self.deferred_commands.push_back(other);
                }
            }
        }
        if cancelled {
            if let Some(token) = &self.cancel_token {
                token.cancel();
            }
        }
        cancelled
    }
    pub(super) fn apply_user_note(
        &mut self,
        content: String,
        consumed: tokio::sync::oneshot::Sender<()>,
    ) {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return;
        }
        let safe_content = self.credential_vault.vault_in_text(trimmed);
        self.messages_mut().push(Message {
            role: Role::User,
            content: MessageContent::text(safe_content.clone()),
        });
        self.pending_user_note_consumptions.push(consumed);
        self.pending_user_note_texts.push(safe_content);
    }
    pub(super) fn begin_user_note_consumption(&mut self) {
        debug_assert!(self.active_user_note_consumptions.is_empty());
        self.active_user_note_consumptions
            .append(&mut self.pending_user_note_consumptions);
        self.active_user_note_texts
            .append(&mut self.pending_user_note_texts);
    }
    pub(super) fn finish_user_note_consumption(&mut self, succeeded: bool) {
        if succeeded {
            for consumed in self.active_user_note_consumptions.drain(..) {
                let _ = consumed.send(());
            }
            self.active_user_note_texts.clear();
        } else {
            self.pending_user_note_consumptions
                .append(&mut self.active_user_note_consumptions);
            self.pending_user_note_texts
                .append(&mut self.active_user_note_texts);
        }
    }
    pub(super) fn reset_user_note_consumption(&mut self) {
        self.pending_user_note_consumptions.clear();
        self.active_user_note_consumptions.clear();
        self.pending_user_note_texts.clear();
        self.active_user_note_texts.clear();
        self.current_request_user_message_index = None;
    }
    /// Run the background task loop
    pub(super) async fn run(mut self) {
        loop {
            if self.shutdown_token.is_cancelled() {
                break;
            }
            let (cmd, activated_request_token) =
                if let Some(command) = self.take_deferred_command().await {
                    command
                } else {
                    let command =
                        recv_command_or_shutdown(&self.shutdown_token, &mut self.command_rx).await;
                    let Some(command) = command else {
                        break;
                    };
                    self.activate_received_command(command).await
                };
            let Some(cmd) = command_after_shutdown_check(cmd, &self.shutdown_token) else {
                break;
            };
            match cmd {
                AgentCommand::AwaitIdle { reply } => {
                    // Active-turn command draining defers this until cleanup completes.
                    let _ = reply.send(());
                }
                #[cfg(test)]
                AgentCommand::InspectSession { capture, reply } => {
                    let _ = reply.send(tests::session_scenarios::inspect(&self, capture));
                }
                AgentCommand::ApplySelectiveSummary {
                    messages,
                    digest,
                    reply,
                } => {
                    let result = if self.busy
                        || !self.pending_messages.is_empty()
                        || !self.deferred_commands.is_empty()
                        || !self.command_rx.is_empty()
                    {
                        Err(anyhow::anyhow!(
                            "Wait for the current turn and queued messages to finish"
                        ))
                    } else {
                        self.apply_selective_summary_history(messages, &digest)
                            .await
                    };
                    let _ = reply.send(result);
                }
                AgentCommand::SelectiveSummaryPreview { reply } => {
                    let result = if self.busy
                        || !self.pending_messages.is_empty()
                        || !self.deferred_commands.is_empty()
                        || !self.command_rx.is_empty()
                    {
                        Err(anyhow::anyhow!(
                            "Wait for the current turn and queued messages to finish"
                        ))
                    } else {
                        crate::agent::selective_summary::preview(&self.messages)
                    };
                    let _ = reply.send(result);
                }
                AgentCommand::SelectiveSummary {
                    selection,
                    digest,
                    instructions,
                    cancellation,
                    mut reply,
                } => {
                    let mut usage = TokenUsage::default();
                    let mut saw_usage = false;
                    let result = if self.busy
                        || !self.pending_messages.is_empty()
                        || !self.deferred_commands.is_empty()
                        || !self.command_rx.is_empty()
                    {
                        Err(anyhow::anyhow!(
                            "Wait for the current turn and queued messages to finish"
                        ))
                    } else {
                        // Keep the task alive to settle usage when the UI cancels.
                        let dropped = cancellation.clone();
                        let operation = self.run_selective_summary(
                            selection,
                            &digest,
                            instructions.as_deref(),
                            &cancellation,
                            &mut usage,
                            &mut saw_usage,
                        );
                        tokio::pin!(operation);
                        tokio::select! {
                            result = &mut operation => result,
                            () = reply.closed() => { dropped.cancel(); operation.await }
                        }
                    };
                    if saw_usage {
                        self.output_tokens_spent =
                            self.output_tokens_spent.saturating_add(usage.output_tokens);
                    }
                    let _ = reply.send(crate::agent::SelectiveSummaryOutcome {
                        usage: (saw_usage || usage.cost.is_some()).then_some(usage),
                        result,
                    });
                }
                AgentCommand::RequeueFollowUpFront {
                    content,
                    attachments,
                    queue_id,
                    managed_request_lineage,
                } => {
                    match self
                        .resolve_managed_request_lineage(managed_request_lineage)
                        .await
                    {
                        Ok(lineage) => {
                            self.requeue_follow_up_front(content, attachments, queue_id, lineage);
                        }
                        Err(error) => self.reject_managed_request(error),
                    }
                    continue;
                }
                AgentCommand::InjectUserNote {
                    content,
                    applied,
                    consumed,
                } => {
                    if self.busy {
                        self.deferred_commands
                            .push_back(AgentCommand::InjectUserNote {
                                content,
                                applied,
                                consumed,
                            });
                        continue;
                    }
                    self.apply_user_note(content, consumed);
                    let _ = applied.send(());
                    continue;
                }
                AgentCommand::EnsureProviderPromptInstalled { applied } => {
                    let result = if self.model_route.uses_app_server() {
                        self.ensure_codex_session()
                            .await
                            .map_err(|error| format!("{error:#}"))
                    } else {
                        Ok(())
                    };
                    let _ = applied.send(result);
                    continue;
                }
                AgentCommand::Prompt {
                    content,
                    attachments,
                    kind,
                    queue_id,
                    managed_request_lineage,
                    managed_inference_authorization,
                } => {
                    let managed_request_lineage = match self
                        .resolve_managed_request_lineage(managed_request_lineage)
                        .await
                    {
                        Ok(lineage) => lineage,
                        Err(error) => {
                            self.reject_managed_request(error);
                            continue;
                        }
                    };
                    if self.busy {
                        self.enqueue_pending_prompt(
                            content,
                            attachments,
                            kind,
                            queue_id,
                            managed_request_lineage,
                            managed_inference_authorization,
                        );
                        continue;
                    }

                    if let Some(client) = self.client.as_mut() {
                        client.set_managed_request_lineage(managed_request_lineage);
                        client.set_managed_inference_authorization(
                            managed_inference_authorization
                                .map(ManagedInferenceAuthorization::into_inner),
                        );
                    }

                    if kind == PromptKind::SideQuestion {
                        self.busy = true;
                        self.run_side_question(content, true).await;
                        self.busy = false;
                        self.emit_conversation_snapshot();
                        let _ = self.event_tx.send(FromAgent::ResponseEnd {
                            response_id: "done".to_string(),
                            usage: None,
                        });
                        continue;
                    }

                    self.busy = true;
                    self.workflow_state.reset();
                    self.tool_executor.reset_coding_turn();

                    let mut prompt = content;
                    let mut attachments = attachments;
                    let mut prompt_context: Option<String> = None;

                    // Execute UserPromptSubmit hooks
                    let hook_result = self
                        .hooks
                        .hook_user_prompt_submit(&prompt, attachments.len() as u32)
                        .await;
                    match hook_result {
                        NativeHookResult::Block { reason } => {
                            self.emit_conversation_snapshot();
                            let _ = self.event_tx.send(FromAgent::Error {
                                message: format!("Prompt blocked by hook: {reason}"),
                                fatal: false,
                                terminal: true,
                                retryable: false,
                            });
                            self.busy = false;
                            self.set_active_request_cancel_token(None);
                            self.prompt_context = None;
                            continue;
                        }
                        NativeHookResult::ModifyInput { new_input } => {
                            Self::apply_message_hook_modification(
                                &mut prompt,
                                &mut attachments,
                                new_input,
                            );
                        }
                        NativeHookResult::InjectContext { context } => {
                            Self::merge_prompt_context(&mut prompt_context, context);
                        }
                        NativeHookResult::Continue => {}
                    }

                    // Execute PreMessage hooks
                    let hook_result = self
                        .hooks
                        .hook_pre_message(&prompt, &attachments, Some(&self.config.model))
                        .await;
                    match hook_result {
                        NativeHookResult::Block { reason } => {
                            self.emit_conversation_snapshot();
                            let _ = self.event_tx.send(FromAgent::Error {
                                message: format!("Prompt blocked by hook: {reason}"),
                                fatal: false,
                                terminal: true,
                                retryable: false,
                            });
                            self.busy = false;
                            self.set_active_request_cancel_token(None);
                            self.prompt_context = None;
                            continue;
                        }
                        NativeHookResult::ModifyInput { new_input } => {
                            Self::apply_message_hook_modification(
                                &mut prompt,
                                &mut attachments,
                                new_input,
                            );
                        }
                        NativeHookResult::InjectContext { context } => {
                            Self::merge_prompt_context(&mut prompt_context, context);
                        }
                        NativeHookResult::Continue => {}
                    }

                    self.prompt_context = prompt_context;

                    // Create cancellation token for this request
                    let cancel_token = activated_request_token.unwrap_or_else(|| {
                        let token = CancellationToken::new();
                        self.set_active_request_cancel_token(Some(token.clone()));
                        token
                    });

                    let mut blocks = Vec::new();
                    blocks.push(ContentBlock::Text { text: prompt });
                    match load_until_cancelled(
                        self.load_attachment_blocks(&attachments),
                        &cancel_token,
                        &self.shutdown_token,
                    )
                    .await
                    {
                        CancellableLoad::Loaded(attachment_blocks) => {
                            blocks.extend(attachment_blocks);
                        }
                        CancellableLoad::RequestCancelled => {
                            // Preserve the normal cancelled-request terminal below.
                        }
                        CancellableLoad::Shutdown => {
                            self.busy = false;
                            self.set_active_request_cancel_token(None);
                            self.prompt_context = None;
                            break;
                        }
                    }

                    for block in &mut blocks {
                        if let ContentBlock::Text { text } = block {
                            *text = self.credential_vault.vault_in_text(text);
                        }
                    }
                    let content = if blocks.len() == 1 {
                        match &blocks[0] {
                            ContentBlock::Text { text } => MessageContent::text(text.clone()),
                            _ => MessageContent::Blocks(blocks),
                        }
                    } else {
                        MessageContent::Blocks(blocks)
                    };

                    let current_prompt_index = self.messages.len();
                    self.messages_mut().push(Message {
                        role: Role::User,
                        content,
                    });
                    if let Some(queue_id) = queue_id {
                        self.processed_prompt_queue_ids.insert(queue_id);
                    }
                    self.current_request_user_message_index = Some(current_prompt_index);
                    let current_prompt_uses_codex = self.model_route.uses_app_server();
                    if current_prompt_uses_codex {
                        self.codex_current_prompt_started = false;
                    }

                    // Reset retry policy for new request
                    self.retry_policy.reset();
                    self.begin_user_note_consumption();
                    // Provider retries are attempts within this user turn, not
                    // fresh turns. Keep refusal memory and the provider
                    // round-trip ceiling outside the retry loop so neither is
                    // reset by a transient request failure.
                    self.denial_memory.begin_turn();
                    let mut step_budget =
                        TurnStepBudget::new(self.config.resolved_max_turn_steps());

                    // Run the agent loop with cancellation and retry support
                    let shutdown_token = self.shutdown_token.clone();
                    let active_cancellation = Arc::clone(&self.active_cancellation);
                    let mut request_cancelled = false;
                    let mut terminal_request_failure = false;
                    let mut terminal_failure_event = None;
                    let mut waited_for_codex_login = false;
                    let mut codex_transport_restarted = false;
                    let mut codex_auth_resumed = false;
                    let mut compacted_after_context_overflow = false;
                    loop {
                        let result = run_request_with_cancellation(
                            self.run_loop(&mut step_budget),
                            &cancel_token,
                            &shutdown_token,
                            &active_cancellation,
                        )
                        .await;

                        match result {
                            Ok(()) => break,
                            Err(e) => {
                                let provider_stream_failure = e
                                    .downcast_ref::<ProviderStreamFailure>()
                                    .map(|error| (error.kind, error.message.clone()));
                                let provider_admission_denied =
                                    e.downcast_ref::<ProviderAdmissionDenied>().is_some();
                                // Raised while composing the request, so it
                                // never reached a provider and carries no
                                // provider error text to classify.
                                let request_exceeds_context_window =
                                    e.downcast_ref::<RequestExceedsContextWindow>().is_some();
                                let empty_assistant_response =
                                    e.downcast_ref::<EmptyAssistantResponse>().is_some();
                                // Preserve the complete anyhow cause chain so
                                // connect/inject/start errors retain provider
                                // retry metadata hidden below their RPC context.
                                let msg = format!("{e:#}");
                                if msg == "Request cancelled" {
                                    request_cancelled = true;
                                    break;
                                }

                                // Classify error and check if we should retry. The host's
                                // admission reason is diagnostic text and may contain words
                                // such as "timeout". Never let that text turn an authoritative
                                // admission decision into an outer retry that could open a new
                                // provider request.
                                let error_kind = if provider_admission_denied {
                                    crate::agent::retry::ErrorKind::Unknown
                                } else {
                                    crate::agent::retry::ErrorKind::classify(&msg)
                                };
                                // Failed provider attempts spend turn steps as
                                // surely as successful ones. Once the last
                                // step is spent, neither retry backoff nor a
                                // context/sign-in recovery can start another
                                // provider request.
                                if !step_budget.can_continue()
                                    && provider_stream_failure.is_none()
                                    && (error_kind.is_retryable()
                                        || request_exceeds_context_window
                                        || matches!(
                                            error_kind,
                                            crate::agent::retry::ErrorKind::ContextOverflow
                                        )
                                        || (current_prompt_uses_codex
                                            && !self.codex_current_prompt_started
                                            && matches!(
                                                error_kind,
                                                crate::agent::retry::ErrorKind::AuthFailure
                                            )))
                                {
                                    terminal_failure_event = Some(FromAgent::Error {
                                        message: step_budget.exhausted(Vec::new()).to_string(),
                                        fatal: false,
                                        terminal: true,
                                        retryable: false,
                                    });
                                    terminal_request_failure = true;
                                    break;
                                }
                                if current_prompt_uses_codex
                                    && !self.codex_current_prompt_started
                                    && !waited_for_codex_login
                                    && matches!(
                                        error_kind,
                                        crate::agent::retry::ErrorKind::AuthFailure
                                    )
                                {
                                    waited_for_codex_login = true;
                                    let auth = match self.tool_executor.codex_auth_context() {
                                        Ok(auth) => auth,
                                        Err(error) => {
                                            terminal_failure_event = Some(FromAgent::Error {
                                                message: error,
                                                fatal: false,
                                                terminal: true,
                                                retryable: false,
                                            });
                                            terminal_request_failure = true;
                                            break;
                                        }
                                    };
                                    let requested_profile = (auth.profile_name != "default")
                                        .then_some(auth.profile_name.clone());
                                    let profile_arg = requested_profile
                                        .as_deref()
                                        .map(|name| format!(" --profile {name}"))
                                        .unwrap_or_default();
                                    let _ = self.event_tx.send(FromAgent::Status {
                                        message: format!(
                                            "Codex sign-in needs attention. Run `deixic-code codex login{profile_arg} --force`; this prompt will resume after sign-in."
                                        ),
                                    });
                                    if wait_for_codex_auth_refresh(
                                        &self.tool_executor,
                                        &auth.auth_path,
                                        &cancel_token,
                                        &shutdown_token,
                                        Duration::from_mins(5),
                                    )
                                    .await
                                    {
                                        self.codex_session = None;
                                        self.codex_correlations.reset();
                                        codex_auth_resumed = true;
                                        continue;
                                    }
                                    if cancel_token.is_cancelled() || shutdown_token.is_cancelled()
                                    {
                                        request_cancelled = cancel_token.is_cancelled();
                                        break;
                                    }
                                }

                                // The provider rejected this request as larger
                                // than its context window. The retry policy
                                // below treats that as terminal, which is right
                                // for resending the same request and wrong for
                                // the turn: the oversized history stays, so
                                // every later prompt fails identically.
                                // Compact once and resend.
                                if (request_exceeds_context_window
                                    || matches!(
                                        error_kind,
                                        crate::agent::retry::ErrorKind::ContextOverflow
                                    ))
                                    && !compacted_after_context_overflow
                                    && self.compact_after_context_overflow()
                                {
                                    compacted_after_context_overflow = true;
                                    let _ = self.event_tx.send(FromAgent::Status {
                                        message: "Request exceeded the model context window; compacted the conversation and retrying."
                                            .to_owned(),
                                    });
                                    continue;
                                }

                                let retry_decision = if step_budget
                                    .discarded_attempt_limit_reached()
                                {
                                    crate::agent::retry::RetryDecision::GiveUp {
                                        reason: "Native turn stopped after three discarded model attempts".into(),
                                    }
                                } else {
                                    request_retry_decision(
                                        &mut self.retry_policy,
                                        error_kind,
                                        if provider_stream_failure.is_some() {
                                            RequestFailureOwner::ProviderStream
                                        } else {
                                            RequestFailureOwner::Request
                                        },
                                    )
                                };
                                match retry_decision {
                                    crate::agent::retry::RetryDecision::Retry {
                                        delay,
                                        attempt,
                                        reason,
                                    } => {
                                        let _ =
                                            self.event_tx.send(FromAgent::RequestRetryScheduled {
                                                attempt,
                                                delay_ms: delay
                                                    .as_millis()
                                                    .try_into()
                                                    .unwrap_or(u64::MAX),
                                                rate_limited: matches!(
                                                    error_kind,
                                                    crate::agent::retry::ErrorKind::RateLimited { .. }
                                                ),
                                            });
                                        if current_prompt_uses_codex
                                            && !self.codex_current_prompt_started
                                        {
                                            self.codex_session = None;
                                            self.codex_correlations.reset();
                                            codex_transport_restarted = true;
                                            let _ =
                                                self.event_tx.send(FromAgent::CodexSessionState {
                                                    state: "reconnecting".to_owned(),
                                                    thread_id: String::new(),
                                                    profile: String::new(),
                                                });
                                            let _ = self.event_tx.send(FromAgent::Status {
                                                message: "Codex app-server disconnected before the turn started; restarting it safely"
                                                    .to_owned(),
                                            });
                                        }
                                        // Notify UI about retry
                                        let _ = self.event_tx.send(FromAgent::Status {
                                            message: format!(
                                                "{}. Retrying in {:.1}s (attempt {})...",
                                                reason,
                                                delay.as_secs_f64(),
                                                attempt
                                            ),
                                        });

                                        // Wait before retrying, but do not make
                                        // shutdown wait for the backoff timer.
                                        if !wait_for_retry_delay(
                                            delay,
                                            &cancel_token,
                                            &shutdown_token,
                                        )
                                        .await
                                        {
                                            request_cancelled = cancel_token.is_cancelled();
                                            break;
                                        }
                                        let _ =
                                            self.event_tx.send(FromAgent::RequestRetryObservation);
                                    }
                                    crate::agent::retry::RetryDecision::GiveUp { reason } => {
                                        // Not retryable or exhausted retries
                                        if empty_assistant_response {
                                            let _ = self.hooks.hook_stop_failure(
                                                "empty_assistant_response",
                                                Some(
                                                    "provider returned no assistant text or tool calls",
                                                ),
                                                None,
                                            )
                                            .await;
                                        }
                                        let hint = if matches!(
                                            error_kind,
                                            crate::agent::retry::ErrorKind::AuthFailure
                                        ) {
                                            if current_prompt_uses_codex {
                                                " — run `deixic-code codex status`; if needed, run `deixic-code codex login --force`"
                                            } else {
                                                " — run `deixic-code codex login --force` or set OPENAI_API_KEY"
                                            }
                                        } else {
                                            ""
                                        };
                                        terminal_failure_event = if let Some((kind, message)) =
                                            provider_stream_failure
                                        {
                                            Some(FromAgent::ProviderError { kind, message })
                                        } else {
                                            Some(FromAgent::Error {
                                                message: format!(
                                                    "Agent error: {msg} ({reason}){hint}"
                                                ),
                                                fatal: false,
                                                terminal: true,
                                                retryable: matches!(
                                                    error_kind,
                                                    crate::agent::retry::ErrorKind::Transient
                                                        | crate::agent::retry::ErrorKind::RateLimited { .. }
                                                ),
                                            })
                                        };
                                        terminal_request_failure = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    if current_prompt_uses_codex {
                        if (terminal_request_failure || request_cancelled)
                            && !self.codex_current_prompt_started
                        {
                            let current_prompt = self.messages.get(current_prompt_index);
                            debug_assert!(
                                current_prompt.is_some_and(|message| message.role == Role::User),
                                "current Codex prompt index must still identify its user message"
                            );
                            if current_prompt.is_some_and(|message| message.role == Role::User) {
                                self.messages_mut().remove(current_prompt_index);
                            }
                        }
                        if terminal_request_failure
                            && self.codex_current_prompt_started
                            && !request_cancelled
                        {
                            if let (Some(turn_id), Some(session)) = (
                                self.codex_active_turn_id.take(),
                                self.codex_session.as_ref(),
                            ) {
                                let _ = self.event_tx.send(FromAgent::CodexTurnState {
                                    state: "failed".to_owned(),
                                    thread_id: session.thread_id().to_owned(),
                                    turn_id: Some(turn_id),
                                });
                            }
                        }
                        self.codex_current_prompt_started = false;
                    }

                    if request_cancelled {
                        // The cancellation token is tripped before the queued Cancel
                        // command is observed. Interrupt the provider turn before
                        // draining commands so the server cannot keep generating.
                        self.interrupt_active_codex_turn().await;
                        // Drain the command channel
                        // while this request still owns it so any prompts that preceded
                        // Cancel are stashed instead of being started as a new request
                        // ahead of that cancellation.
                        let _ = self.drain_pending_commands().await;
                    }

                    let completion_event = if !terminal_request_failure && !request_cancelled {
                        match coding_turn_completed_event(&self.tool_executor, "done") {
                            Ok(event) => Some(event),
                            Err(message) => {
                                terminal_request_failure = true;
                                terminal_failure_event = Some(FromAgent::Error {
                                    message,
                                    fatal: false,
                                    terminal: true,
                                    retryable: false,
                                });
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Count only turns that produced a completion the session
                    // still owns. SessionEnd reports this as turnCount.
                    if !terminal_request_failure && !request_cancelled {
                        self.hooks.hook_increment_turn().await;
                    }
                    self.current_request_user_message_index = None;

                    self.finish_task_boost(request_cancelled).await;
                    self.busy = false;
                    self.set_active_request_cancel_token(None);
                    let codex_transport_receipt = if current_prompt_uses_codex {
                        let outcome = if request_cancelled {
                            "cancelled"
                        } else if terminal_request_failure {
                            "failed"
                        } else {
                            "completed"
                        };
                        Some(FromAgent::CodexTransportReceipt {
                            provider: "openai-codex".to_owned(),
                            transport: "codex-app-server".to_owned(),
                            outcome: outcome.to_owned(),
                            transport_restarted: codex_transport_restarted,
                            auth_resumed: codex_auth_resumed,
                            cancellation_requested: request_cancelled,
                        })
                    } else {
                        None
                    };

                    self.prompt_context = None;

                    self.repair_orphaned_tool_calls();
                    // The semantic checkpoint is private but synchronously
                    // persistable by hosted owners. Publish it before exposing
                    // any terminal that permits the current owner to be killed.
                    self.emit_conversation_snapshot();

                    // Errors are terminal events and clear the TUI busy state.
                    // Publishing ResponseEnd after one would let stream adapters
                    // reinterpret a failed turn as a successful empty response.
                    if let Some(event) = terminal_failure_event {
                        match &event {
                            FromAgent::Error { message, fatal, .. } => {
                                let _ = self
                                    .hooks
                                    .hook_on_error(
                                        message,
                                        "agent_error",
                                        Some("agent_turn"),
                                        !fatal,
                                    )
                                    .await;
                            }
                            FromAgent::ProviderError { kind, message } => {
                                let error_kind = format!("provider_{kind:?}").to_lowercase();
                                let _ = self
                                    .hooks
                                    .hook_on_error(
                                        message,
                                        &error_kind,
                                        Some("provider_stream"),
                                        true,
                                    )
                                    .await;
                            }
                            _ => {}
                        }
                        let _ = self.event_tx.send(event);
                        if let Some(receipt) = codex_transport_receipt {
                            let _ = self.event_tx.send(receipt);
                        }
                    } else {
                        if let Some(receipt) = codex_transport_receipt {
                            let _ = self.event_tx.send(receipt);
                        }
                        let _ = self.event_tx.send(FromAgent::ResponseEnd {
                            response_id: "done".to_string(),
                            usage: None,
                        });
                    }
                    self.finish_user_note_consumption(
                        !terminal_request_failure && !request_cancelled,
                    );
                    if !terminal_request_failure && !request_cancelled {
                        if let Some(event) = completion_event {
                            let _ = self.event_tx.send(event);
                        }
                    } else if request_cancelled {
                        self.compact_interrupted_native_history();
                        let _ = self.event_tx.send(FromAgent::TurnInterrupted {
                            response_id: "done".to_string(),
                            reason: "cancelled".to_string(),
                        });
                    }
                }
                AgentCommand::Cancel { clear_pending } => {
                    if let Some(token) = &self.cancel_token {
                        token.cancel();
                    }
                    self.clear_pending_on_cancel = clear_pending;
                    self.busy = false;
                    self.prompt_context = None;
                    if clear_pending {
                        // Also clear any pending messages on cancel
                        let cleared = self.pending_messages.clear();
                        if !cleared.is_empty() {
                            let _ = self.event_tx.send(FromAgent::Status {
                                message: format!("Cleared {} pending message(s)", cleared.len()),
                            });
                        }
                    }
                    self.reject_pending_tool_responses_on_cancel();
                }
                AgentCommand::CancelQueued { id } => {
                    // The staged system prompt is not keyed by id and stays
                    // staged: the skills it carries are still active in the UI,
                    // so the next message to start should see them.
                    if let Some(removed) = self.pending_messages.remove_by_id(id) {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message: format!(
                                "Removed queued {} #{}",
                                removed.kind.label(),
                                removed.id
                            ),
                        });
                    } else {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message: format!("No queued prompt found with id #{id}"),
                        });
                    }
                }
                AgentCommand::ReorderQueued { id, placement } => {
                    if !self.pending_messages.move_by_id(id, placement) {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message: format!("No queued prompt found with id #{id}"),
                        });
                    }
                }
                AgentCommand::SetModel { model } => {
                    let policy_id = policy_model_id(&model);
                    if let Some(reason) = self.tool_executor.model_allowed(&policy_id) {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: reason.clone(),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                        let _ = self
                            .event_tx
                            .send(FromAgent::ModelChangeFailed { model, reason });
                        continue;
                    }

                    match self.tool_executor.resolve_model(&model) {
                        Ok(resolved) => {
                            let NativeResolvedClient {
                                client,
                                provider_name: provider,
                                model_route,
                            } = resolved;
                            // Admit the replacement before discarding the live transport.
                            self.codex_session = None;
                            self.codex_correlations.reset();
                            self.codex_active_turn_id = None;
                            self.preserve_explicit_intelligence_choice();
                            let requested_thinking = crate::agent::model_dynamics::thinking_level(
                                self.config.thinking_enabled,
                                self.config.thinking_budget,
                            );
                            let thinking = self
                                .tool_executor
                                .normalize_thinking(&model, requested_thinking);
                            let (thinking_enabled, thinking_budget) = thinking.to_config();
                            self.config.thinking_enabled = thinking_enabled;
                            self.config.thinking_budget = thinking_budget;
                            self.client = client;
                            if let Some(client) = self.client.as_mut() {
                                client.set_managed_authorization_provider(
                                    self.managed_authorization.clone(),
                                );
                            }
                            self.model_route = model_route;
                            refresh_model_budgets_with_host(
                                &self.tool_executor,
                                &mut self.config,
                                &mut self.compactor,
                                &model,
                            );
                            self.config.model = model.clone();
                            self.hooks.hook_set_model(&model).await;
                            let _ = self
                                .event_tx
                                .send(FromAgent::ModelChanged { model, provider });
                            let _ = self.event_tx.send(FromAgent::BoostChanged {
                                status: crate::agent::model_dynamics::BoostStatus::Idle,
                                thinking: Some(thinking),
                            });
                        }
                        Err(e) => {
                            let message = format!("Failed to set model: {e}");
                            let _ = self.event_tx.send(FromAgent::Error {
                                message: message.clone(),
                                fatal: false,
                                terminal: false,
                                retryable: false,
                            });
                            let _ = self.event_tx.send(FromAgent::ModelChangeFailed {
                                model,
                                reason: message,
                            });
                        }
                    }
                }
                AgentCommand::SetContextToolExcluded { name, excluded } => {
                    self.set_context_tool_excluded(&name, excluded);
                }
                AgentCommand::Boost => {
                    let mut state = self.dynamics.lock().expect("model dynamics mutex");
                    if !state.used {
                        state.requested = true;
                        state.status = crate::agent::model_dynamics::BoostStatus::Pending;
                        let _ = self.event_tx.send(FromAgent::BoostChanged {
                            status: state.status,
                            thinking: None,
                        });
                    }
                }
                AgentCommand::SetThinking { enabled, budget } => {
                    self.preserve_explicit_intelligence_choice();
                    self.config.thinking_enabled = enabled;
                    self.config.thinking_budget = budget;
                }
                AgentCommand::RefreshModelBudgets => {
                    let model = self.config.model.clone();
                    refresh_model_budgets_with_host(
                        &self.tool_executor,
                        &mut self.config,
                        &mut self.compactor,
                        &model,
                    );
                }
                AgentCommand::SetMaxTokens { max_tokens } => {
                    set_explicit_max_tokens(&mut self.config, max_tokens);
                }
                AgentCommand::InstallProcessBudget {
                    limits,
                    checkpoint,
                    applied,
                } => {
                    let result = self.apply_process_budget(limits, checkpoint);
                    let _ = applied.send(result);
                }
                AgentCommand::ClearProcessBudget {
                    system_prompt,
                    applied,
                } => {
                    let result = self.retire_process_budget(system_prompt);
                    let _ = applied.send(result);
                }
                AgentCommand::SetOutputTokenBudget {
                    max_total_output_tokens,
                } => {
                    self.output_token_budget = Some(max_total_output_tokens);
                }
                AgentCommand::SetSubagentParentScope { parent_scope_id } => {
                    self.tool_executor
                        .set_subagent_parent_scope(parent_scope_id);
                }
                AgentCommand::SetSessionContext {
                    session_id,
                    transcript_path,
                    reason,
                    owns_persistent_tool_spills,
                    preserve_compacted_checkpoint,
                } => {
                    self.apply_session_context(
                        session_id,
                        transcript_path,
                        &reason,
                        owns_persistent_tool_spills,
                        preserve_compacted_checkpoint,
                    )
                    .await;
                }
                AgentCommand::SetHookLogFile { path } => {
                    self.hooks.hook_set_log_file(Some(path)).await;
                }
                AgentCommand::SetHooksEnabled { enabled } => {
                    self.hooks.hook_set_enabled(enabled).await;
                }
                AgentCommand::ReloadHooks => {
                    if let Err(error) = self.hooks.hook_reload().await {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: format!("Failed to reload hooks: {error}"),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                    }
                }
                AgentCommand::InspectHooks { reply } => {
                    let _ = reply.send(self.hooks.hook_runtime_snapshot().await);
                }
                AgentCommand::SetGoalToolsVisible { visible } => {
                    self.set_goal_tools_visible(visible);
                }
                AgentCommand::SetApprovalMode { mode } => {
                    self.config.approval_mode = mode;
                }
                AgentCommand::ReplaceGovernedTools {
                    allowed_tools,
                    external_tool_definitions,
                } => {
                    self.replace_governed_tools(&allowed_tools, external_tool_definitions);
                }
                AgentCommand::SetSteeringMode { mode } => {
                    self.steering_mode = mode;
                }
                AgentCommand::SetFollowUpMode { mode } => {
                    self.follow_up_mode = mode;
                }
                AgentCommand::SetSystemPrompt { system_prompt } => {
                    self.config.system_prompt = Some(system_prompt);
                    self.system_prompt_revision = self.system_prompt_revision.saturating_add(1);
                    self.runtime_prompt_revision = self.runtime_prompt_revision.saturating_add(1);
                    self.refresh_runtime_audit();
                }
                AgentCommand::SetSystemPromptForQueuedPrompt {
                    queue_id,
                    system_prompt,
                } => {
                    self.queued_system_prompts
                        .insert(queue_id, (self.system_prompt_revision, system_prompt));
                }
                AgentCommand::ClearHistory => {
                    self.semantic_continuation = None;
                    self.reset_tool_response_state();
                    self.reset_user_note_consumption();
                    // Reset ends this history's lifetime, including its high-water
                    // capacity. Replacing also avoids copy-on-write before clearing.
                    self.messages = Arc::new(Vec::new());
                    self.codex_session = None;
                    self.codex_correlations.reset();
                    self.codex_history_restore_prefix_len = None;
                    self.codex_current_prompt_started = false;
                    self.pending_messages.clear();
                    // The prompts it was staged for are gone with the queue.
                    self.queued_system_prompts.clear();
                    self.notify_extensions_user_turn_start();
                    self.credential_vault.clear();
                }
                AgentCommand::ReplaceHistory {
                    messages,
                    continuation,
                } => {
                    self.semantic_continuation = continuation;
                    self.reset_tool_response_state();
                    self.reset_user_note_consumption();
                    let restored_prefix_len = messages.len();
                    self.messages = Arc::new(messages);
                    self.codex_session = None;
                    self.codex_correlations.reset();
                    self.codex_history_restore_prefix_len = Some(restored_prefix_len);
                    self.codex_current_prompt_started = false;
                    self.compact_codex_history_for_boundary();
                    self.pending_messages.clear();
                    // The prompts it was staged for are gone with the queue.
                    self.queued_system_prompts.clear();
                    self.notify_extensions_user_turn_start();
                    // Replacing history is used for session restore. References
                    // from the previous active session must not cross that boundary.
                    self.credential_vault.clear();
                }
                AgentCommand::ReplaceHistoryPreservingCredentials { messages } => {
                    self.semantic_continuation = None;
                    self.reset_user_note_consumption();
                    let restored_prefix_len = messages.len();
                    // `main` stores runner history in an Arc; keep this
                    // assignment compatible with both the pre-merge Vec and
                    // the current shared-history representation.
                    self.messages = history_storage(messages);
                    self.codex_session = None;
                    self.codex_correlations.reset();
                    self.codex_history_restore_prefix_len = Some(restored_prefix_len);
                    self.codex_current_prompt_started = false;
                    self.compact_codex_history_for_boundary();
                    self.pending_messages.clear();
                    // The prompts it was staged for are gone with the queue.
                    self.queued_system_prompts.clear();
                    self.notify_extensions_user_turn_start();
                }
                AgentCommand::Continue => {
                    // Continue from current context without adding a new user message
                    // Used for retry after transient errors
                    if self.busy {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: "Agent is busy".to_string(),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                        continue;
                    }

                    // Need at least some history to continue from
                    if self.messages.is_empty() {
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: "Cannot continue: no conversation history".to_string(),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                        continue;
                    }

                    self.busy = true;
                    self.current_request_user_message_index = None;
                    self.begin_user_note_consumption();
                    self.denial_memory.begin_turn();
                    let mut step_budget =
                        TurnStepBudget::new(self.config.resolved_max_turn_steps());
                    let cancel_token = CancellationToken::new();
                    self.set_active_request_cancel_token(Some(cancel_token.clone()));
                    let shutdown_token = self.shutdown_token.clone();
                    let active_cancellation = Arc::clone(&self.active_cancellation);

                    // Run the agent loop without adding a user message
                    let result = run_request_with_cancellation(
                        self.run_loop(&mut step_budget),
                        &cancel_token,
                        &shutdown_token,
                        &active_cancellation,
                    )
                    .await;

                    let mut request_succeeded = result.is_ok();
                    let mut request_cancelled = false;
                    let mut request_failure_event = None;
                    if let Err(e) = result {
                        let provider_stream_failure = e
                            .downcast_ref::<ProviderStreamFailure>()
                            .map(|error| (error.kind, error.message.clone()));
                        let msg = e.to_string();
                        if msg == "Request cancelled" {
                            request_cancelled = true;
                        } else if let Some((kind, message)) = provider_stream_failure {
                            request_failure_event =
                                Some(FromAgent::ProviderError { kind, message });
                        } else {
                            request_failure_event = Some(FromAgent::Error {
                                message: format!("Agent error: {e}"),
                                fatal: false,
                                terminal: true,
                                retryable: matches!(
                                    crate::agent::retry::ErrorKind::classify(&msg),
                                    crate::agent::retry::ErrorKind::Transient
                                        | crate::agent::retry::ErrorKind::RateLimited { .. }
                                ),
                            });
                        }
                    }

                    let completion_event = if request_succeeded {
                        match coding_turn_completed_event(&self.tool_executor, "continue") {
                            Ok(event) => Some(event),
                            Err(message) => {
                                request_succeeded = false;
                                request_failure_event = Some(FromAgent::Error {
                                    message,
                                    fatal: false,
                                    terminal: true,
                                    retryable: false,
                                });
                                None
                            }
                        }
                    } else {
                        None
                    };

                    self.finish_task_boost(request_cancelled).await;
                    self.busy = false;
                    self.set_active_request_cancel_token(None);
                    self.prompt_context = None;
                    self.current_request_user_message_index = None;

                    self.repair_orphaned_tool_calls();
                    self.emit_conversation_snapshot();

                    if let Some(event) = request_failure_event {
                        let _ = self.event_tx.send(event);
                    } else {
                        let _ = self.event_tx.send(FromAgent::ResponseEnd {
                            response_id: "continue".to_string(),
                            usage: None,
                        });
                    }
                    self.finish_user_note_consumption(request_succeeded);
                    if request_succeeded {
                        if let Some(event) = completion_event {
                            let _ = self.event_tx.send(event);
                        }
                    } else if request_cancelled {
                        self.compact_interrupted_native_history();
                        let _ = self.event_tx.send(FromAgent::TurnInterrupted {
                            response_id: "continue".to_string(),
                            reason: "cancelled".to_string(),
                        });
                    }
                }
            }
        }

        // Close the active session for hooks here rather than from the caller.
        // The app's own exit path is skipped entirely on SIGINT/SIGTERM --
        // `run_with_shutdown` drops the `app.run()` future and then cancels the
        // runner -- and a command sent at that point would race the
        // cancellation. This runs on every way out of the loop, so a handled
        // signal, a normal quit, and a closed command channel all emit it.
        self.apply_session_context(None, None, "shutdown", false, false)
            .await;

        self.tool_executor.shutdown_background_processes().await;
    }
}
