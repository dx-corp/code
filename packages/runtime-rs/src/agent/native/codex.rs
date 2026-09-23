//! Codex transport integration and completion reconciliation.

use super::*;

/// Freeze the app-server tool call into the same opaque form used by native
/// provider tool calls before any policy decision or execution.
pub(super) fn codex_tool_args_for_admission(args: &Value, vault: &CredentialVault) -> Value {
    vault.vault_in_json(args)
}

/// Hosted AgentRun / A2A ids the worker injects into Maestro's environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct HostedCodexTurnCorrelation {
    pub agent_run_id: Option<String>,
    pub a2a_task_id: Option<String>,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) fn hosted_codex_turn_correlation_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> HostedCodexTurnCorrelation {
    HostedCodexTurnCorrelation {
        agent_run_id: lookup("MAESTRO_AGENT_RUN_ID").or_else(|| lookup("EVALOPS_AGENT_RUN_ID")),
        a2a_task_id: lookup("MAESTRO_A2A_TASK_ID")
            .or_else(|| lookup("MAESTRO_A2A_DELEGATION_ID"))
            .or_else(|| lookup("A2A_TASK_ID")),
    }
}

pub(super) fn hosted_codex_turn_correlation() -> HostedCodexTurnCorrelation {
    hosted_codex_turn_correlation_from(env_nonempty)
}

/// Connector mounts listed context files but hydration wrote none of them.
#[allow(dead_code)]
pub(super) fn vfs_hydration_yielded_no_files(
    context_file_count: i64,
    hydrated_file_count: i64,
) -> bool {
    context_file_count > 0 && hydrated_file_count == 0
}

fn warn_codex_turn_failed(event: &'static str, thread_id: &str, turn_id: &str) {
    let correlation = hosted_codex_turn_correlation();
    tracing::warn!(
        target: "maestro.codex",
        event,
        thread_id = %thread_id,
        turn_id = %turn_id,
        agent_run_id = correlation.agent_run_id.as_deref().unwrap_or(""),
        a2a_task_id = correlation.a2a_task_id.as_deref().unwrap_or(""),
        "Codex turn failed"
    );
}

impl NativeAgentRunner {
    /// Ensure a Codex app-server thread exists for `openai-codex/*`.
    pub(super) async fn ensure_codex_session(&mut self) -> Result<()> {
        if self.codex_session.is_some() {
            return Ok(());
        }
        let model = match &self.model_route {
            NativeModelRoute::CodexAppServer { model_id } => model_id.clone(),
            NativeModelRoute::DirectProvider => {
                crate::agent::codex_app_server_turns::codex_thread_model_id(&self.config.model)
            }
        };
        let cwd = self.config.cwd.clone();
        // Codex approvalPolicy values: never | on-request | on-failure | untrusted.
        // Safe is intentionally stricter than Selective (untrusted).
        //
        // Yolo uses `on-request`, not `never`. `never` means Codex never asks
        // Maestro, so the requestApproval handler — profile allowlist,
        // PreToolUse/PermissionRequest hooks, ActionFirewall — never runs and
        // a restricted code child can still mutate through the native path.
        // The handler still auto-accepts under Yolo after those checks pass.
        let approval_policy = match self.config.approval_mode {
            ApprovalMode::Yolo | ApprovalMode::Selective => Some("on-request".to_owned()),
            ApprovalMode::Safe => Some("untrusted".to_owned()),
        };
        // The configured sandbox policy previously reached only the Maestro
        // tool executor, so on this transport Codex ran its own
        // `commandExecution` and `fileChange` operations under whatever
        // `MAESTRO_SANDBOX_MODE` said -- nothing at all by default. A
        // read-only policy is a hard floor here: it is how a read-only
        // subagent role is expressed, so the environment override must not be
        // able to loosen it.
        let sandbox = match self.config.sandbox_policy {
            Some(maestro_sandbox::SandboxPolicy::ReadOnly) => Some("read-only".to_owned()),
            _ => std::env::var("MAESTRO_SANDBOX_MODE")
                .ok()
                .filter(|mode| !mode.is_empty() && mode != "default" && mode != "inherit")
                .or_else(|| codex_sandbox_mode(self.config.sandbox_policy.as_ref())),
        };
        let dynamic_tools =
            crate::agent::codex_app_server_turns::dynamic_tools_from_native(&self.tools);
        let provider_tools: Vec<Tool> = dynamic_tools
            .iter()
            .map(|tool| Tool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                schema_enforcement: Default::default(),
            })
            .collect();
        let safe_tools = vault_provider_tools(&provider_tools, &self.credential_vault)?;
        // Same standing instructions the HTTP path puts in RequestConfig.system.
        let instructions = runtime_system_prompt(
            self.config.system_prompt.as_deref(),
            self.prompt_context.as_deref(),
            &self.config.model,
            self.tool_executor.model_capabilities(&self.config.model),
        )
        .map(|text| self.credential_vault.vault_in_text(&text));
        let restored_prefix_len = self.codex_history_restore_prefix_len.unwrap_or(0);
        let restored_messages = vault_provider_history(
            &self.messages[..restored_prefix_len.min(self.messages.len())],
            &self.credential_vault,
        )?;
        let prepared_request = ProviderSafeRequest::prepare(
            &Arc::new(restored_messages),
            RequestConfig {
                system: instructions,
                tools: safe_tools,
                ..RequestConfig::default()
            },
            &self.credential_vault,
        )?;
        let dynamic_tools: Vec<_> = prepared_request
            .config
            .tools
            .iter()
            .map(
                |tool| crate::agent::codex_app_server_turns::DynamicToolSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                },
            )
            .collect();
        let instructions = prepared_request.config.system.clone();
        let restored_messages = prepared_request.messages.as_ref();
        let auth = self
            .tool_executor
            .codex_auth_context()
            .map_err(anyhow::Error::msg)?;
        let session_id = self.hooks.hook_session_id().await;
        prepared_request.ensure_current(&self.credential_vault)?;
        let session =
            crate::agent::codex_app_server_turns::CodexAppServerTurnSession::connect_persistent_with_auth(
                model,
                Some(cwd),
                approval_policy,
                sandbox,
                session_id.as_deref(),
                crate::agent::codex_app_server_turns::CodexThreadPayload {
                    dynamic_tools: &dynamic_tools,
                    instructions,
                    restored_messages,
                },
                &auth,
            )
            .await?;
        self.codex_history_restore_prefix_len = None;
        let session_state = match session.open_kind() {
            crate::codex_session::CodexSessionOpen::Resumed => "resumed",
            crate::codex_session::CodexSessionOpen::Created => "created",
        };
        let profile = session.profile().to_owned();
        let profile = if profile.is_empty() {
            "default".to_owned()
        } else {
            profile
        };
        let _ = self.event_tx.send(FromAgent::CodexCompatibility {
            protocol_version: session.compatibility().protocol_version.clone(),
            resume: session.compatibility().resume,
            steering: session.compatibility().steering,
        });
        let _ = self.event_tx.send(FromAgent::CodexSessionState {
            state: session_state.to_owned(),
            thread_id: session.thread_id().to_owned(),
            profile,
        });
        let _ = self.event_tx.send(FromAgent::Status {
            message: format!("Codex app-server thread ready ({})", session.thread_id()),
        });
        self.codex_session = Some(session);
        Ok(())
    }
    /// Drive one user turn (and any tool calls) entirely through Codex
    /// app-server so ChatGPT OAuth refresh is never handled as a Platform API key.
    ///
    /// **Native parity:**
    /// - Dynamic tools run via Maestro `ToolExecutor` + firewall (same as HTTP).
    /// - Codex-native `commandExecution` / `fileChange` approvals pass through
    ///   the same hooks, firewall, and keyed approval channel. Yolo auto-accepts
    ///   after hard checks; Selective/Safe wait for the user's ToolResponse.
    pub(super) async fn run_loop_via_codex_app_server(
        &mut self,
        step_budget: &mut TurnStepBudget,
    ) -> Result<()> {
        self.codex_correlations.reset();
        let model = crate::agent::codex_app_server_turns::codex_thread_model_id(&self.config.model);
        let started = Instant::now();
        let span = crate::model_span("openai-codex", &model);
        let result = self
            .run_loop_via_codex_app_server_inner(step_budget)
            .instrument(span.clone())
            .await;
        self.codex_correlations.reset();
        let outcome = if result.is_ok() { "success" } else { "error" };
        record_outcome(
            &span,
            outcome,
            started.elapsed(),
            result.is_err().then_some("provider_error"),
        );
        result
    }
    pub(super) async fn run_loop_via_codex_app_server_inner(
        &mut self,
        step_budget: &mut TurnStepBudget,
    ) -> Result<()> {
        use crate::agent::codex_app_server_turns::TurnWaitEvent;

        self.ensure_codex_session().await?;

        let user_text = codex_app_server_user_text(
            &self.messages,
            &self.active_user_note_texts,
            self.current_request_user_message_index,
        );
        if user_text.is_empty() {
            bail!("No user message available for Codex app-server turn");
        }
        let user_text = self.credential_vault.vault_in_text(&user_text);
        let vault_attestation = self
            .credential_vault
            .attest_provider_text(&user_text)
            .map_err(anyhow::Error::msg)?;

        // `tool_search` may activate schemas while this turn is in flight,
        // but those tools are not part of the model's turn-start contract.
        // Keep policy decisions on the same immutable snapshot so a later
        // same-turn item/tool/call cannot widen the governed execution set.
        let turn_start_active_tool_names = self.active_tool_names.clone();

        let response_id = Uuid::new_v4().to_string();
        let _ = self.event_tx.send(FromAgent::ResponseStart {
            response_id: response_id.clone(),
        });

        self.validate_codex_boost().await;
        step_budget.record_step();
        anyhow::ensure!(
            self.credential_vault.has_attestation(vault_attestation),
            "credential vault changed before Codex turn dispatch"
        );
        let turn_id = {
            let session = self
                .codex_session
                .as_ref()
                .context("Codex app-server session missing")?;
            session
                .start_text_turn_with_thinking(
                    user_text,
                    self.config.thinking_enabled,
                    self.config.thinking_budget,
                    None,
                )
                .await?
        };
        self.codex_current_prompt_started = true;
        self.codex_active_turn_id = Some(turn_id.clone());
        if let Some(session) = self.codex_session.as_ref() {
            let _ = self.event_tx.send(FromAgent::CodexTurnState {
                state: "accepted".to_owned(),
                thread_id: session.thread_id().to_owned(),
                turn_id: Some(turn_id.clone()),
            });
        }

        // Accumulate the current provider-history segment. Tool boundaries
        // consume that segment's authoritative item before flushing it, so
        // the terminal completion cannot repeat pre-tool assistant text.
        let mut streamed_assistant = String::new();

        loop {
            self.drain_codex_native_operation_completions().await;
            if self.drain_pending_commands().await {
                return Err(anyhow::anyhow!("Request cancelled"));
            }
            self.forward_pending_codex_steers(&turn_id).await?;

            // Stream any agent message deltas that arrived since the last wait.
            self.drain_codex_assistant_deltas(&response_id, &mut streamed_assistant)
                .await?;

            let event = {
                let session = self
                    .codex_session
                    .as_ref()
                    .context("Codex app-server session missing")?;
                session
                    .wait_server_request_or_turn_complete(&turn_id, Some(100))
                    .await?
            };

            match event {
                TurnWaitEvent::Pending => continue,
                TurnWaitEvent::Completed(result) => {
                    self.drain_codex_native_operation_completions().await;
                    self.codex_active_turn_id = None;
                    let (completion_delta, full_text) = Self::reconcile_codex_completion_text(
                        &streamed_assistant,
                        &result.assistant_text,
                        result.assistant_text_is_full,
                    );
                    if !completion_delta.is_empty() {
                        streamed_assistant.push_str(&completion_delta);
                        let _ = self.event_tx.send(FromAgent::ResponseChunk {
                            response_id: response_id.clone(),
                            content: completion_delta,
                            is_thinking: false,
                        });
                    }
                    let final_text = Self::codex_terminal_assistant_text(
                        streamed_assistant,
                        full_text,
                        result.assistant_text_is_full,
                    );
                    let usage_notifications = if let Some(session) = self.codex_session.as_ref() {
                        session
                            .take_usage_notifications_for_turn(&result.turn_id)
                            .await
                    } else {
                        Vec::new()
                    };
                    let usage =
                        choose_codex_turn_usage(&result.raw_completion, &usage_notifications);
                    if let Some(usage) = usage.as_ref() {
                        self.output_tokens_spent =
                            self.output_tokens_spent.saturating_add(usage.output_tokens);
                    }
                    let _ = self.event_tx.send(FromAgent::CodexUsageState {
                        source: if usage.is_some() {
                            "exact".to_owned()
                        } else {
                            "unavailable".to_owned()
                        },
                        usage: usage.clone(),
                    });
                    if let Some(failure) = result.provider_failure().map(str::to_owned) {
                        warn_codex_turn_failed(
                            "codex_turn_failed",
                            &result.thread_id,
                            &result.turn_id,
                        );
                        let _ = self.event_tx.send(FromAgent::CodexTurnState {
                            state: "failed".to_owned(),
                            thread_id: result.thread_id,
                            turn_id: Some(result.turn_id),
                        });
                        return Err(anyhow::anyhow!(failure));
                    }
                    if final_text.trim().is_empty() {
                        let correlation = hosted_codex_turn_correlation();
                        tracing::warn!(
                            target: "maestro.codex",
                            event = "codex_turn_empty_assistant_response",
                            thread_id = %result.thread_id,
                            turn_id = %result.turn_id,
                            agent_run_id = correlation.agent_run_id.as_deref().unwrap_or(""),
                            a2a_task_id = correlation.a2a_task_id.as_deref().unwrap_or(""),
                            assistant_text_chars = result.assistant_text.chars().count(),
                            assistant_text_is_full = result.assistant_text_is_full,
                            "Codex turn failed"
                        );
                        let _ = self.event_tx.send(FromAgent::CodexTurnState {
                            state: "failed".to_owned(),
                            thread_id: result.thread_id,
                            turn_id: Some(result.turn_id),
                        });
                        return Err(anyhow::Error::new(EmptyAssistantResponse));
                    }
                    self.messages_mut().push(Message {
                        role: Role::Assistant,
                        content: MessageContent::Text(final_text),
                    });
                    let _ = self.event_tx.send(FromAgent::CodexTurnState {
                        state: "completed".to_owned(),
                        thread_id: result.thread_id.clone(),
                        turn_id: Some(result.turn_id.clone()),
                    });
                    let _ = self
                        .event_tx
                        .send(FromAgent::ResponseEnd { response_id, usage });
                    return Ok(());
                }
                TurnWaitEvent::ServerRequest(request) => {
                    if !step_budget.can_continue() {
                        if let Some(session) = self.codex_session.as_ref() {
                            let _ = session.interrupt_turn(&turn_id, Some(1_500)).await;
                        }
                        self.codex_active_turn_id = None;
                        return Err(step_budget
                            .exhausted(vec!["Codex app-server tool request".to_string()])
                            .into());
                    }
                    // The server-request reader is ordered: any assistant
                    // notification read before this request is already queued.
                    // Drain that causal prefix now, before recording the tool
                    // use/result, rather than depending on the next loop tick.
                    self.drain_codex_assistant_deltas(&response_id, &mut streamed_assistant)
                        .await?;
                    self.reconcile_codex_completed_segment(&mut streamed_assistant)
                        .await?;
                    self.flush_codex_streamed_assistant(&mut streamed_assistant);
                    // Native completion notifications share the ordered
                    // server-request prefix but have a separate drain. Pull
                    // them in before policy evaluates an item-id-only
                    // approval, otherwise a queued item/completed path is
                    // invisible and the action firewall fails closed.
                    self.drain_codex_native_operation_completions().await;
                    self.handle_codex_server_request(request, &turn_start_active_tool_names)
                        .await?;
                    // Returning a tool result lets app-server start the next
                    // model response in this turn, so charge it now.
                    step_budget.record_step();
                }
            }
        }
    }
    pub(super) async fn forward_pending_codex_steers(&mut self, turn_id: &str) -> Result<()> {
        let pending = self.drain_leading_pending_messages(PromptKind::Steer, self.steering_mode);
        if pending.is_empty() {
            return Ok(());
        }
        self.announce_next_turn_messages(&pending);
        for pending_message in pending {
            let Some((message, prompt_context)) =
                self.prepare_pending_message(&pending_message).await?
            else {
                continue;
            };
            let mut text = match &message.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            if let Some(context) = prompt_context {
                text.push_str("\n\n");
                text.push_str(&context);
            }
            self.codex_session
                .as_ref()
                .context("Codex app-server session missing")?
                .steer_text(turn_id, text, None)
                .await?;
            if pending_message.id != 0 {
                self.processed_prompt_queue_ids.insert(pending_message.id);
            }
            if let Some(session) = self.codex_session.as_ref() {
                let _ = self.event_tx.send(FromAgent::CodexTurnState {
                    state: "steering".to_owned(),
                    thread_id: session.thread_id().to_owned(),
                    turn_id: Some(turn_id.to_owned()),
                });
            }
            self.messages_mut().push(message);
        }
        Ok(())
    }
    pub(super) async fn drain_codex_assistant_deltas(
        &self,
        response_id: &str,
        current_segment: &mut String,
    ) -> Result<()> {
        use crate::codex_app_server::agent_message_text_from_notifications;

        let session = self
            .codex_session
            .as_ref()
            .context("Codex app-server session missing")?;
        let deltas = session.take_message_deltas().await;
        let text = agent_message_text_from_notifications(&deltas);
        if !text.is_empty() {
            current_segment.push_str(&text);
            let _ = self.event_tx.send(FromAgent::ResponseChunk {
                response_id: response_id.to_owned(),
                content: text,
                is_thinking: false,
            });
        }
        Ok(())
    }
    pub(super) async fn reconcile_codex_completed_segment(
        &self,
        current_segment: &mut String,
    ) -> Result<()> {
        let session = self
            .codex_session
            .as_ref()
            .context("Codex app-server session missing")?;
        let completed_text = session.take_completed_assistant_text().await;
        if !completed_text.is_empty() {
            let (_, authoritative_segment) =
                Self::reconcile_codex_completion_text(current_segment, &completed_text, true);
            *current_segment = authoritative_segment;
        }
        Ok(())
    }
    pub(super) fn reconcile_codex_completion_text(
        emitted_assistant: &str,
        completion_text: &str,
        completion_is_full: bool,
    ) -> (String, String) {
        if completion_text.is_empty() {
            return (String::new(), emitted_assistant.to_owned());
        }
        if completion_is_full {
            let tail = completion_text
                .strip_prefix(emitted_assistant)
                .unwrap_or_default()
                .to_owned();
            return (tail, completion_text.to_owned());
        }

        (
            completion_text.to_owned(),
            format!("{emitted_assistant}{completion_text}"),
        )
    }
    pub(super) fn codex_terminal_assistant_text(
        streamed_assistant: String,
        reconciled_full_text: String,
        completion_is_full: bool,
    ) -> String {
        if completion_is_full {
            reconciled_full_text
        } else {
            streamed_assistant
        }
    }
    pub(super) fn flush_codex_streamed_assistant(&mut self, streamed_assistant: &mut String) {
        if !streamed_assistant.is_empty() {
            self.messages_mut().push(Message {
                role: Role::Assistant,
                content: MessageContent::Text(std::mem::take(streamed_assistant)),
            });
        }
    }
    pub(super) fn record_codex_tool_use(&mut self, call_id: &str, tool_name: &str, args: &Value) {
        let input = self.credential_vault.vault_in_json(args);
        append_codex_tool_use(self.messages_mut(), call_id, tool_name, input);
    }
    pub(super) fn record_codex_tool_result(
        &mut self,
        call_id: &str,
        content: String,
        is_error: bool,
    ) {
        let content = self.credential_vault.vault_in_text(&content);
        append_codex_tool_result(self.messages_mut(), call_id, content, is_error);
    }
    /// Run `PostToolUse` for a Codex tool call, fold in any injected context,
    /// record the result in history, and return the text for the wire.
    ///
    /// Both history and the wire response carry the appended context, so the
    /// model sees the same result the transcript records.
    ///
    /// Returns the wire text and whether the call must be reported as failed,
    /// which an `EvalGate` rejection can turn on for an otherwise successful
    /// tool.
    pub(super) async fn finalize_codex_tool_result(
        &mut self,
        outcome: CodexToolOutcome<'_>,
    ) -> (String, bool) {
        let CodexToolOutcome {
            tool_name,
            call_id,
            args,
            hook_output,
            result_text,
            is_error,
            pre_hook_context,
            duration_ms,
        } = outcome;
        let hook_outcome = run_post_execution_hooks(
            &self.hooks,
            &self.credential_vault,
            PostExecutionHookInput {
                tool_name,
                call_id,
                args,
                raw_output: hook_output,
                is_error,
                duration_ms,
            },
        )
        .await;
        let safe_result_text = self.credential_vault.vault_in_text(&result_text);
        let mut text = append_hook_context(
            &self.hooks,
            safe_result_text,
            NativeHookEvent::PreToolUse,
            pre_hook_context,
        );
        text = append_hook_context(
            &self.hooks,
            text,
            NativeHookEvent::PostToolUse,
            hook_outcome.context.as_deref(),
        );
        if let Some(reason) = &hook_outcome.rejected {
            text = format!("{text}\n\n[Eval gate rejected this result: {reason}]");
        }
        let reported_error = is_error || hook_outcome.rejected.is_some();
        let (text, reported_error) = self.apply_tool_result_extensions(
            call_id,
            tool_name,
            args,
            duration_ms,
            text,
            reported_error,
            None,
        );
        let safe_text = self.credential_vault.vault_in_text(&text);
        let response = opaque_codex_tool_result_for_wire(&safe_text);
        self.record_codex_tool_result(call_id, safe_text, reported_error);
        (response, reported_error)
    }
    /// Pull file-change item notifications into the correlation map.
    ///
    /// Must run before handling a pathless `item/fileChange/requestApproval`
    /// so ordinary Codex edits are not fail-closed solely because the approval
    /// RPC omits paths.
    pub(super) async fn ingest_codex_file_change_notifications(&mut self) {
        let Some(session) = self.codex_session.as_ref() else {
            return;
        };
        let notes = session.take_file_change_item_notifications().await;
        for note in notes {
            if let Some(params) = note.params.as_ref() {
                remember_codex_file_change_item_paths(
                    params,
                    &mut self.codex_correlations.file_changes,
                );
            }
        }
    }
    pub(super) async fn drain_codex_native_operation_completions(&mut self) {
        let Some(session) = self.codex_session.as_ref() else {
            return;
        };
        let notes = session
            .take_native_operation_completion_notifications()
            .await;
        for note in notes {
            if let Some((call_id, success)) = codex_native_completion(&note) {
                self.extensions.on_native_tool_result(
                    &crate::agent::extensions::NativeToolResultContext {
                        turn_id: self.current_turn_id.clone(),
                        call_id,
                        success,
                    },
                );
            }
            remember_codex_file_change_completion_paths(
                &note,
                &mut self.codex_correlations.file_changes,
            );
            if let Some(event) = project_or_defer_codex_native_completion(
                &note,
                &mut self.codex_correlations.approved,
                &mut self.codex_correlations.pending_completions,
                self.tool_executor.managed_policy_metadata(),
            ) {
                let _ = self.event_tx.send(event);
            }
        }
    }
    pub(super) async fn handle_codex_server_request(
        &mut self,
        request: crate::codex_app_server::IncomingServerRequest,
        turn_start_active_tool_names: &HashSet<String>,
    ) -> Result<()> {
        use crate::agent::codex_app_server_turns::{
            approval_decision, parse_tool_call_params, tool_call_error_result,
            tool_call_success_result,
        };

        // Always ingest first: file-change paths may have arrived as earlier
        // notifications still sitting in the client buffer.
        self.ingest_codex_file_change_notifications().await;

        let method = request.method.clone();
        match method.as_str() {
            "item/tool/call" => {
                let params = request.params.clone().unwrap_or(Value::Null);
                let (tool_name, call_id, args) = match parse_tool_call_params(&params) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        request.respond(tool_call_error_result(err.to_string()));
                        return Ok(());
                    }
                };
                self.tool_response_coordinator.remove_cancelled(&call_id);

                // Prefer the original registry key (case-insensitive / sanitized).
                let registry_name = self
                    .tools
                    .keys()
                    .find(|name| {
                        name.eq_ignore_ascii_case(&tool_name)
                            || name.replace([' ', '/', ':'], "_") == tool_name
                    })
                    .cloned()
                    .unwrap_or_else(|| tool_name.to_lowercase());

                let tool_key = registry_name.to_lowercase();
                self.record_codex_tool_use(&call_id, &registry_name, &args);

                if let Some(reason) =
                    codex_tool_call_denied_by_active_tools(&tool_key, turn_start_active_tool_names)
                {
                    let error = format!("Tool denied by governed allowlist: {reason}");
                    self.record_codex_tool_result(&call_id, error.clone(), true);
                    request.respond(tool_call_error_result(error));
                    return Ok(());
                }

                // This handler is the second place that decides whether a tool
                // executes. The HTTP tool loop runs `PreToolUse` before the
                // firewall so the firewall vets whatever the hook rewrote; the
                // same order applies here, otherwise a policy hook is enforced
                // for one transport and skipped for the other.
                let (args, pre_hook_context) =
                    match run_pre_tool_use_hook(&self.hooks, &registry_name, &call_id, &args).await
                    {
                        Ok(outcome) => outcome,
                        Err(reason) => {
                            let _ = self.event_tx.send(FromAgent::HookBlocked {
                                call_id: call_id.clone(),
                                tool: registry_name.clone(),
                                reason: reason.clone(),
                            });
                            let error = format!("Tool blocked by hook: {reason}");
                            self.record_codex_tool_result(&call_id, error.clone(), true);
                            request.respond(tool_call_error_result(error));
                            return Ok(());
                        }
                    };

                // The app-server model and PreToolUse hook can both supply
                // plaintext. Policy, approval events, hooks, and execution
                // must all see the same vaulted arguments.
                let args = codex_tool_args_for_admission(&args, &self.credential_vault);

                let is_external_tool = self.external_tools.contains(&tool_key);
                let annotations = self.tool_executor.tool_annotations(&tool_key);
                let workflow_snapshot = self.workflow_state.snapshot();
                let firewall_verdict = if is_external_tool {
                    NativeFirewallVerdict::Allow
                } else {
                    self.tool_executor.firewall_verdict(
                        &tool_key,
                        &args,
                        &workflow_snapshot,
                        annotations.as_ref(),
                        false,
                    )
                };
                if let NativeFirewallVerdict::Block { reason } = &firewall_verdict {
                    let _ = self.event_tx.send(FromAgent::Error {
                        message: reason.clone(),
                        fatal: false,
                        terminal: false,
                        retryable: false,
                    });
                    let error = format!("Tool blocked by action firewall: {reason}");
                    self.record_codex_tool_result(&call_id, error.clone(), true);
                    request.respond(tool_call_error_result(error));
                    return Ok(());
                }

                let approval_decision = tool_requires_approval(
                    self.config.approval_mode,
                    is_external_tool,
                    &firewall_verdict,
                    &self.tool_executor,
                    &registry_name,
                    &args,
                    &self.denial_memory,
                );
                if approval_decision.is_repeat_refusal() {
                    let message = repeat_refusal_message(&registry_name);
                    self.record_codex_tool_result(&call_id, message.clone(), true);
                    request.respond(tool_call_error_result(message));
                    return Ok(());
                }
                let requires_approval = approval_decision.requires_approval();

                // The approval decision is the `PermissionRequest` boundary on
                // this transport, matching the HTTP tool loop. A `block`
                // denies the call and the user is never asked.
                if requires_approval {
                    let permission = self
                        .hooks
                        .hook_permission_request(
                            &registry_name,
                            &call_id,
                            &args,
                            "tool requires approval",
                        )
                        .await;
                    if let NativeHookResult::Block { reason } = permission {
                        let message = format!("Tool denied by permission hook: {reason}");
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: message.clone(),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                        self.record_codex_tool_result(&call_id, message.clone(), true);
                        request.respond(tool_call_error_result(message));
                        return Ok(());
                    }
                }

                if requires_approval {
                    let _ = self.event_tx.send(FromAgent::ToolCall {
                        call_id: call_id.clone(),
                        tool: registry_name.clone(),
                        args: args.clone(),
                        requires_approval: true,
                        approval_inline_env: None,
                    });
                    // Codex can issue multiple server tool calls. Reuse the
                    // keyed waiter so an approval for a later call is retained
                    // with its consumption receipt until that call waits.
                    let approval_cancel = self.shutdown_token.child_token();
                    self.set_active_approval_cancel_token(Some(approval_cancel.clone()));
                    let approval_started = Instant::now();
                    let approval = approval_span();
                    let response = self
                        .tool_response_coordinator
                        .wait_for_tool_response(&call_id, &approval_cancel)
                        .instrument(approval.clone())
                        .await;
                    self.set_active_approval_cancel_token(None);
                    let (approval_outcome, approval_error) = match &response {
                        ToolResponseWait::Response((approved, _, _)) if *approved => {
                            ("approved", None)
                        }
                        ToolResponseWait::Response(_) => ("denied", Some("approval_denied")),
                        ToolResponseWait::Cancelled => ("cancelled", Some("approval_cancelled")),
                        ToolResponseWait::Closed => ("closed", Some("approval_channel_closed")),
                    };
                    record_outcome(
                        &approval,
                        approval_outcome,
                        approval_started.elapsed(),
                        approval_error,
                    );
                    let (approved, provided_result, _source) = match response {
                        ToolResponseWait::Response(response) => response,
                        ToolResponseWait::Cancelled => {
                            let cancelled_ids = HashSet::from([call_id.clone()]);
                            self.tool_response_coordinator
                                .discard_cancelled(&cancelled_ids);
                            let error = "Tool approval cancelled".to_owned();
                            self.record_codex_tool_result(&call_id, error.clone(), true);
                            request.respond(tool_call_error_result(error));
                            return Ok(());
                        }
                        ToolResponseWait::Closed => {
                            let error = "Tool approval channel closed".to_owned();
                            self.record_codex_tool_result(&call_id, error.clone(), true);
                            request.respond(tool_call_error_result(error));
                            return Ok(());
                        }
                    };
                    if !approved {
                        self.denial_memory.record(&registry_name, &args);
                        let error = "Tool denied by user".to_owned();
                        self.record_codex_tool_result(&call_id, error.clone(), true);
                        request.respond(tool_call_error_result(error));
                        return Ok(());
                    }
                    if is_external_tool && provided_result.is_none() {
                        let error =
                            "Caller-owned tool response did not include a result".to_owned();
                        self.record_codex_tool_result(&call_id, error.clone(), true);
                        request.respond(tool_call_error_result(error));
                        return Ok(());
                    }
                    if let Some(result) = provided_result {
                        let is_error = !result.success;
                        let vaulted_text = if result.success {
                            result.output
                        } else {
                            result.error.unwrap_or_else(|| result.output.clone())
                        };
                        let hook_output = vaulted_text.clone();
                        // A UI-supplied result was not executed here, so there
                        // is no interval this path can measure.
                        let (response, is_error) = self
                            .finalize_codex_tool_result(CodexToolOutcome {
                                tool_name: &registry_name,
                                call_id: &call_id,
                                args: &args,
                                hook_output: &hook_output,
                                result_text: vaulted_text,
                                is_error,
                                pre_hook_context: pre_hook_context.as_deref(),
                                duration_ms: 0,
                            })
                            .await;
                        if is_error {
                            request.respond(tool_call_error_result(response));
                        } else {
                            request.respond(tool_call_success_result(response));
                        }
                        return Ok(());
                    }
                } else {
                    let _ = self.event_tx.send(FromAgent::ToolCall {
                        call_id: call_id.clone(),
                        tool: registry_name.clone(),
                        args: args.clone(),
                        requires_approval: false,
                        approval_inline_env: None,
                    });
                }

                let execution = self
                    .execute_tool(&registry_name, &args, &call_id, None)
                    .await;
                let is_error = execution.is_error();
                let hook_output = execution.raw_content();
                let duration_ms = execution.receipt.duration_ms.unwrap_or(0);
                let (response, is_error) = self
                    .finalize_codex_tool_result(CodexToolOutcome {
                        tool_name: &registry_name,
                        call_id: &call_id,
                        args: &args,
                        hook_output: &hook_output,
                        result_text: execution.model_content(),
                        is_error,
                        pre_hook_context: pre_hook_context.as_deref(),
                        duration_ms,
                    })
                    .await;
                if is_error {
                    request.respond(tool_call_error_result(response));
                } else {
                    request.respond(tool_call_success_result(response));
                }
                Ok(())
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "applyPatchApproval"
            | "execCommandApproval" => {
                // Native Codex command/file approvals use the same keyed ToolCall
                // approval channel as dynamic tools. Yolo accepts after hard policy
                // checks; Selective and Safe wait for the caller's decision.
                //
                // A read-only sandbox policy overrides the approval mode. Every
                // subagent runs in Yolo, because a delegated child cannot
                // answer an approval prompt, so without this a read-only child
                // role -- explore, plan, review -- had its native exec and
                // file-change requests auto-accepted, and with
                // `isolation=shared` those act on the parent's own checkout.
                // Codex is also asked to sandbox itself on `thread/start`, but
                // that is a request to another process; this is the part
                // Maestro enforces.
                // Report the operation for output accounting before deciding
                // on it. Codex runs these itself instead of through
                // `item/tool/call`, so they produce no `ToolCall` event and a
                // caller metering this stream -- the subagent scheduler --
                // never charged the command or patch the model generated.
                // Charged whether or not it is approved: the model produced
                // the payload either way.
                let _ = self.event_tx.send(FromAgent::CodexNativeOperation {
                    method: request.method.clone(),
                    output_chars: codex_native_operation_chars(request.params.as_ref()),
                });

                // Policy hooks govern this branch too. Round 4 routed
                // `item/tool/call` through the pipeline, but a Codex-native
                // mutation is approved here instead, so a hook that blocks
                // shell commands or file writes was bypassed on exactly the
                // operations it exists to stop.
                //
                // The operation is presented under a stable synthetic tool name
                // so a policy can match it, with the request params as its
                // arguments. Only `block` is actionable: Codex has already
                // decided what to run and there is no way to hand it rewritten
                // arguments, so a `ModifyInput` rewrite is treated as a denial
                // rather than silently approving the unsanitized original.
                // A hook that must rewrite a command has to use the
                // `item/tool/call` path.
                //
                // Capture any paths on the approval itself before hooks run,
                // then hand hooks the same correlated path set the firewall
                // uses. itemId-only v2 approvals otherwise leave path-sensitive
                // PreToolUse / PermissionRequest hooks blind.
                if let Some(params) = request.params.as_ref() {
                    remember_codex_file_change_item_paths(
                        params,
                        &mut self.codex_correlations.file_changes,
                    );
                }
                let policy_tool = codex_native_policy_tool(&request.method);
                let policy_args = codex_native_policy_hook_args(
                    &request.method,
                    request.params.as_ref(),
                    &self.codex_correlations.file_changes,
                );
                if codex_native_contains_plaintext_credential(&self.credential_vault, &policy_args)
                {
                    discard_deferred_codex_native_completion(
                        request.params.as_ref(),
                        &mut self.codex_correlations.pending_completions,
                    );
                    let _ = self.event_tx.send(FromAgent::Status {
                        message: format!(
                            "Declined Codex-native {} (credential-bearing payload cannot be rewritten)",
                            request.method
                        ),
                    });
                    let _ = self.event_tx.send(FromAgent::CodexNativeDecision {
                        method: request.method.clone(),
                        decision: "denied_policy".to_owned(),
                    });
                    request.respond(approval_decision(false));
                    return Ok(());
                }
                let policy_call_id = Uuid::new_v4().to_string();
                let hook_denial = match run_pre_tool_use_hook(
                    &self.hooks,
                    policy_tool,
                    &policy_call_id,
                    &policy_args,
                )
                .await
                {
                    Err(reason) => Some(reason),
                    Ok((rewritten, _)) if rewritten != policy_args => Some(
                        "PreToolUse rewrote the Codex-native operation, which cannot accept rewritten parameters"
                            .to_string(),
                    ),
                    Ok(_) => match self.hooks.hook_permission_request(
                        policy_tool,
                        &policy_call_id,
                        &policy_args,
                        "Codex-native mutation",
                    )
                    .await
                    {
                        NativeHookResult::Block { reason } => Some(reason),
                        _ => None,
                    },
                };

                let denies_mutation = config_denies_mutation(self.config.sandbox_policy.as_ref());
                let profile_denial = codex_native_denied_by_active_tools(
                    &request.method,
                    turn_start_active_tool_names,
                );
                let firewall_decision = codex_native_firewall_decision(
                    &self.tool_executor,
                    &request.method,
                    request.params.as_ref(),
                    Some(&self.workflow_state.snapshot()),
                    Some(&self.codex_correlations.file_changes),
                );
                let firewall_denial = match &firewall_decision {
                    CodexNativeFirewallDecision::Block { reason } => Some(reason.clone()),
                    _ => None,
                };
                let denial_reason = if let Some(reason) = hook_denial.as_deref() {
                    Some(format!("blocked by a policy hook: {reason}"))
                } else if let Some(reason) = firewall_denial.as_deref() {
                    Some(format!("blocked by the action firewall: {reason}"))
                } else if let Some(reason) = profile_denial {
                    Some(reason.to_string())
                } else if denies_mutation {
                    Some("the sandbox policy is read-only".to_string())
                } else {
                    None
                };
                if let Some(reason) = denial_reason {
                    discard_deferred_codex_native_completion(
                        request.params.as_ref(),
                        &mut self.codex_correlations.pending_completions,
                    );
                    let _ = self.event_tx.send(FromAgent::Status {
                        message: format!("Declined Codex-native {} ({reason})", request.method),
                    });
                    let _ = self.event_tx.send(FromAgent::CodexNativeDecision {
                        method: request.method.clone(),
                        decision: "denied_policy".to_owned(),
                    });
                    request.respond(approval_decision(false));
                    return Ok(());
                }
                if !codex_native_approval_requires_user(self.config.approval_mode) {
                    let _ = self.event_tx.send(FromAgent::CodexNativeDecision {
                        method: request.method.clone(),
                        decision: "approved_policy".to_owned(),
                    });
                    remember_approved_codex_native_operation(
                        request.params.as_ref(),
                        &policy_call_id,
                        policy_tool,
                        &mut self.codex_correlations.approved,
                    );
                    if let Some(event) = project_deferred_codex_native_completion(
                        request.params.as_ref(),
                        &mut self.codex_correlations.pending_completions,
                        &mut self.codex_correlations.approved,
                        self.tool_executor.managed_policy_metadata(),
                    ) {
                        let _ = self.event_tx.send(event);
                    }
                    request.respond(approval_decision(true));
                    return Ok(());
                }
                let _ = self.event_tx.send(FromAgent::ToolCall {
                    call_id: policy_call_id.clone(),
                    tool: policy_tool.to_owned(),
                    args: policy_args,
                    requires_approval: true,
                    approval_inline_env: None,
                });
                let approval_cancel = self.shutdown_token.child_token();
                self.set_active_approval_cancel_token(Some(approval_cancel.clone()));
                let approval_started = Instant::now();
                let approval = approval_span();
                let response = self
                    .tool_response_coordinator
                    .wait_for_tool_response(&policy_call_id, &approval_cancel)
                    .instrument(approval.clone())
                    .await;
                self.set_active_approval_cancel_token(None);
                let (approval_outcome, approval_error) = match &response {
                    ToolResponseWait::Response((approved, _, _)) if *approved => ("approved", None),
                    ToolResponseWait::Response(_) => ("denied", Some("approval_denied")),
                    ToolResponseWait::Cancelled => ("cancelled", Some("approval_cancelled")),
                    ToolResponseWait::Closed => ("closed", Some("approval_channel_closed")),
                };
                record_outcome(
                    &approval,
                    approval_outcome,
                    approval_started.elapsed(),
                    approval_error,
                );
                let (approved, _, _) = match response {
                    ToolResponseWait::Response(response) => response,
                    ToolResponseWait::Cancelled => {
                        discard_deferred_codex_native_completion(
                            request.params.as_ref(),
                            &mut self.codex_correlations.pending_completions,
                        );
                        let cancelled_ids = HashSet::from([policy_call_id.clone()]);
                        self.tool_response_coordinator
                            .discard_cancelled(&cancelled_ids);
                        let _ = self.event_tx.send(FromAgent::CodexNativeDecision {
                            method: request.method.clone(),
                            decision: "cancelled".to_owned(),
                        });
                        request.respond(approval_decision(false));
                        return Ok(());
                    }
                    ToolResponseWait::Closed => {
                        discard_deferred_codex_native_completion(
                            request.params.as_ref(),
                            &mut self.codex_correlations.pending_completions,
                        );
                        let _ = self.event_tx.send(FromAgent::CodexNativeDecision {
                            method: request.method.clone(),
                            decision: "channel_closed".to_owned(),
                        });
                        request.respond(approval_decision(false));
                        return Ok(());
                    }
                };
                if !approved {
                    let _ = self.event_tx.send(FromAgent::Status {
                        message: format!("Declined Codex-native {} (user denied)", request.method),
                    });
                }
                let _ = self.event_tx.send(FromAgent::CodexNativeDecision {
                    method: request.method.clone(),
                    decision: if approved {
                        "approved_user"
                    } else {
                        "denied_user"
                    }
                    .to_owned(),
                });
                if approved {
                    remember_approved_codex_native_operation(
                        request.params.as_ref(),
                        &policy_call_id,
                        policy_tool,
                        &mut self.codex_correlations.approved,
                    );
                    if let Some(event) = project_deferred_codex_native_completion(
                        request.params.as_ref(),
                        &mut self.codex_correlations.pending_completions,
                        &mut self.codex_correlations.approved,
                        self.tool_executor.managed_policy_metadata(),
                    ) {
                        let _ = self.event_tx.send(event);
                    }
                } else {
                    discard_deferred_codex_native_completion(
                        request.params.as_ref(),
                        &mut self.codex_correlations.pending_completions,
                    );
                }
                request.respond(approval_decision(approved));
                Ok(())
            }
            "item/permissions/requestApproval" => {
                request.respond(json!({ "permissions": {}, "scope": "turn" }));
                Ok(())
            }
            other => {
                request.reject(format!("Unsupported Codex server-request: {other}"));
                Ok(())
            }
        }
    }
    /// Interrupt the active Codex turn after the outer cancellation future is dropped.
    ///
    /// The app-server owns the provider request, so dropping Maestro's wait future
    /// is not sufficient to stop the remote turn or release its thread.
    pub(super) async fn interrupt_active_codex_turn(&mut self) {
        let Some(turn_id) = self.codex_active_turn_id.take() else {
            return;
        };
        let Some(session) = self.codex_session.as_ref() else {
            return;
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            session.interrupt_turn(&turn_id, Some(1_500)),
        )
        .await;
        let message = match result {
            Ok(Ok(())) => {
                let _ = self.event_tx.send(FromAgent::CodexTurnState {
                    state: "interrupted".to_owned(),
                    thread_id: session.thread_id().to_owned(),
                    turn_id: Some(turn_id.clone()),
                });
                format!("Codex turn interrupted ({turn_id})")
            }
            Ok(Err(error)) => {
                let _ = self.event_tx.send(FromAgent::CodexTurnState {
                    state: "failed".to_owned(),
                    thread_id: session.thread_id().to_owned(),
                    turn_id: Some(turn_id.clone()),
                });
                format!("Codex turn interrupt failed: {error:#}")
            }
            Err(_) => {
                let _ = self.event_tx.send(FromAgent::CodexTurnState {
                    state: "failed".to_owned(),
                    thread_id: session.thread_id().to_owned(),
                    turn_id: Some(turn_id.clone()),
                });
                "Codex turn interrupt timed out".to_owned()
            }
        };
        let _ = self.event_tx.send(FromAgent::Status { message });
    }
    pub(super) fn compact_codex_history_for_boundary(&mut self) {
        if !self.model_route.uses_app_server() {
            return;
        }

        let compaction_started = Instant::now();
        let mut result = self.compactor.compact_with_tokens(&self.messages);
        self.prepare_continuation(&mut result);
        if !result.was_compacted() {
            return;
        }

        let _ = self.event_tx.send(FromAgent::CompactionMeasured {
            duration_ms: compaction_started
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        });
        let status_message = format!(
            "Codex history compacted: {} messages summarized, {} oversized messages bounded",
            result.compacted_count, result.intra_compacted_count
        );
        emit_compaction_event(
            &self.event_tx,
            &self.messages,
            result.summary.as_deref().unwrap_or(&status_message),
            result.cut_point.as_ref(),
            result.continuation.as_ref(),
            true,
        );
        // Adopt provenance only with its compacted message history. A
        // cancelled preparation must not become the next merge's input.
        if let Some(record) = result.continuation {
            self.semantic_continuation = Some(record);
        }
        self.messages = Arc::new(result.messages);
        self.codex_session = None;
        self.codex_correlations.reset();
        self.codex_history_restore_prefix_len = Some(self.messages.len());
        let _ = self.event_tx.send(FromAgent::Status {
            message: status_message,
        });
    }
}

#[cfg(test)]
mod hosted_turn_diagnostics_tests {
    use super::vfs_hydration_yielded_no_files;

    #[test]
    fn vfs_hydration_gap_is_context_files_with_zero_hydrated() {
        assert!(vfs_hydration_yielded_no_files(4, 0));
        assert!(!vfs_hydration_yielded_no_files(4, 1));
        assert!(!vfs_hydration_yielded_no_files(0, 0));
    }

    #[test]
    fn hosted_correlation_reads_agent_run_and_a2a_task_ids() {
        let correlation = super::hosted_codex_turn_correlation_from(|name| match name {
            "MAESTRO_AGENT_RUN_ID" => Some("run-9047".to_string()),
            "MAESTRO_A2A_TASK_ID" => Some("task-9047".to_string()),
            _ => None,
        });
        assert_eq!(correlation.agent_run_id.as_deref(), Some("run-9047"));
        assert_eq!(correlation.a2a_task_id.as_deref(), Some("task-9047"));
    }
}
