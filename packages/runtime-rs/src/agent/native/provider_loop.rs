//! Provider streaming and the native model/tool turn loop.

use super::provider_history::{OBSERVATION_FULL_TURNS, project_observation_history};
use super::*;

impl NativeAgentRunner {
    /// Run the agent loop until complete or interrupted
    /// One user turn.
    ///
    /// Wraps [`Self::run_loop_inner`] so every exit path -- normal completion,
    /// cancellation, provider error -- fires `on_turn_end` exactly once.
    pub(super) async fn run_loop(&mut self, step_budget: &mut TurnStepBudget) -> Result<()> {
        // Resolve consent only at the safe user-turn boundary; retries keep this assignment.
        let assignment = self
            .tool_executor
            .experiment_assignment(&self.config.model)
            .filter(|a| a.is_valid());
        let selected = assignment
            .as_ref()
            .map_or(self.baseline_tool_profile, |a| match a.arm {
                maestro_runtime_contracts::experiments::ExperimentArm::Control => ToolProfile::Fast,
                maestro_runtime_contracts::experiments::ExperimentArm::Minimal => {
                    ToolProfile::Minimal
                }
            });
        if self.experiment_assignment != assignment {
            self.tool_profile = selected;
            self.active_tool_names = initial_active_tool_names(
                selected,
                &self.tools,
                &self.external_tools,
                Some(&self.explicitly_allowed_tools),
                self.config.external_tool_schema_policy,
            );
            self.model_tool_cache = None;
            self.refresh_runtime_audit();
        }
        self.experiment_assignment = assignment;
        self.current_turn_id = Uuid::new_v4().to_string();
        self.turn_index = self.turn_index.saturating_add(1);
        self.turn_tool_calls = 0;
        // Announce the user turn before fallible preparation. Recovery may
        // re-enter run_loop_inner, but must not create another user turn.
        let _ = self.event_tx.send(FromAgent::OperationObservation {
            observation:
                maestro_runtime_contracts::operation_observation::OperationObservation::Admitted {
                    turn_id: self.current_turn_id.clone(),
                    experiment: self.experiment_assignment.clone().map(|assignment| {
                        maestro_runtime_contracts::experiments::ExperimentObservation {
                            assignment,
                            locally_applied: true,
                            runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
                        }
                    }),
                    thinking_level: self
                        .current_model_choice()
                        .thinking
                        .label()
                        .to_ascii_lowercase(),
                },
        });
        let _ = self.event_tx.send(FromAgent::TurnStarted);

        self.apply_requested_boost().await?;
        self.tool_executor.set_subagent_parent_model(
            self.current_model_choice().model,
            self.current_model_choice().thinking.label().to_owned(),
        );
        let turn_started = Instant::now();
        let turn = turn_span(None);
        turn.record("gen_ai.agent.run.id", self.current_turn_id.as_str());
        let outcome = self
            .run_with_model_recovery(step_budget)
            .instrument(turn.clone())
            .await;
        let outcome_label = if outcome.is_ok() { "success" } else { "error" };
        record_outcome(
            &turn,
            outcome_label,
            turn_started.elapsed(),
            outcome.is_err().then_some("turn_error"),
        );
        turn.in_scope(|| {
            let terminal = terminal_span(outcome_label);
            record_outcome(
                &terminal,
                outcome_label,
                turn_started.elapsed(),
                outcome.is_err().then_some("turn_error"),
            );
        });

        let cx = TurnEndContext {
            turn_id: self.current_turn_id.clone(),
            tool_calls: self.turn_tool_calls,
            interrupted: outcome.is_err(),
        };
        self.extensions.on_turn_end(&cx);
        outcome
    }
    /// Fire `on_user_turn_start` on every registered extension.
    ///
    /// Called from the `AgentCommand` arms that discard conversation state, the
    /// same three places the doom-loop detector was reset before it became an
    /// extension tenant.
    pub(super) fn notify_extensions_user_turn_start(&mut self) {
        let cx = TurnStartContext {
            turn_id: self.current_turn_id.clone(),
            turn_index: self.turn_index,
        };
        self.extensions.on_user_turn_start(&cx);
    }
    /// Build the `on_tool_call_planned` context for a call and dispatch it.
    ///
    /// Increments the per-turn tool-call counter, so `call_index` is the number
    /// of calls this turn planned before this one.
    pub(super) fn plan_tool_call_through_extensions(
        &mut self,
        call_id: &str,
        tool_name: &str,
        safe_args: &serde_json::Value,
    ) -> ExtensionVerdict {
        let cx = ExtensionToolCallContext {
            turn_id: self.current_turn_id.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args_hash: stable_stringify(safe_args),
            args: safe_args.clone(),
            call_index: self.turn_tool_calls,
        };
        self.turn_tool_calls = self.turn_tool_calls.saturating_add(1);
        self.extensions.on_tool_call_planned(&cx)
    }
    /// Dispatch `on_tool_result` and apply whatever the tenants left in the
    /// payload back onto the model-facing result.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_tool_result_extensions(
        &mut self,
        call_id: &str,
        tool_name: &str,
        safe_args: &serde_json::Value,
        duration_ms: u64,
        content: String,
        is_error: bool,
        receipt: Option<&crate::agent::protocol::ExecutionReceipt>,
    ) -> (String, bool) {
        if let Some(receipt) = receipt {
            self.retain_file_operation(call_id, receipt);
        }
        let cx = ExtensionToolResultContext {
            edit: receipt.and_then(|receipt| match &receipt.details {
                crate::agent::protocol::ToolReceiptDetails::BuiltIn(crate::ToolDetails::Edit(
                    edit,
                )) if matches!(
                    receipt.source,
                    crate::agent::protocol::ExecutionSource::Native
                ) =>
                {
                    Some(crate::agent::extensions::LocalEditResult {
                        path: edit.path.clone(),
                        text_not_found: edit.text_not_found,
                    })
                }
                _ => None,
            }),
            turn_id: self.current_turn_id.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args_hash: stable_stringify(safe_args),
            args: safe_args.clone(),
            is_error,
            duration_ms,
        };
        let mut payload = ToolResultPayload {
            content: self.credential_vault.vault_in_text(&content),
            is_error,
        };
        self.extensions.on_tool_result(&cx, &mut payload);
        (
            self.credential_vault.vault_in_text(&payload.content),
            payload.is_error,
        )
    }
    /// Dispatch `on_tool_batch_end` with the batch's last result as the mutable
    /// payload, then write any tenant edits back into that result.
    pub(super) fn apply_tool_batch_end_extensions(&mut self, tool_results: &mut [ContentBlock]) {
        let error_count = tool_results
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult {
                        is_error: Some(true),
                        ..
                    }
                )
            })
            .count() as u64;
        let cx = BatchEndContext {
            turn_id: self.current_turn_id.clone(),
            batch_size: tool_results.len() as u64,
            error_count,
        };

        let Some(ContentBlock::ToolResult {
            content, is_error, ..
        }) = tool_results.last_mut()
        else {
            // Still announce the boundary; a tenant that only counts batches
            // must not miss one because the batch ended on a non-tool block.
            let mut payload = ToolResultPayload::default();
            self.extensions.on_tool_batch_end(&cx, &mut payload);
            return;
        };

        let original_is_error = *is_error;
        let mut payload = ToolResultPayload {
            content: std::mem::take(content),
            is_error: original_is_error.unwrap_or(false),
        };
        self.extensions.on_tool_batch_end(&cx, &mut payload);
        *content = self.credential_vault.vault_in_text(&payload.content);
        // Only overwrite the flag when a tenant actually changed it, so a result
        // that carried `None` keeps carrying `None`.
        if Some(payload.is_error) != original_is_error {
            *is_error = Some(payload.is_error);
        }
    }
    pub(super) async fn run_loop_inner(&mut self, step_budget: &mut TurnStepBudget) -> Result<()> {
        // The caller may re-enter after a failed provider attempt. Those
        // attempts spent steps too, so refuse another request before either
        // provider route starts it.
        if !step_budget.can_continue() {
            return Err(step_budget.exhausted(Vec::new()).into());
        }
        if self.model_route.uses_app_server() {
            return self.run_loop_via_codex_app_server(step_budget).await;
        }

        // Reminders accumulate across the tool batches of one turn and reset
        // when a queued user message starts a new one.
        let mut reminders = ReminderEngine::new();
        // Nothing else in the runner watches assistant text. Without this the
        // only thing that ends a repeating generation is the provider's own
        // output cap, which the user pays for in full.
        let mut text_loop_detector = TextLoopDetector::new();
        let mut steered_after_text_loop = false;
        let mut steered_after_billed_empty = false;
        'turn: loop {
            step_budget.admit_attempt().map_err(anyhow::Error::msg)?;
            text_loop_detector.reset();
            step_budget.record_step();
            let response_id = Uuid::new_v4().to_string();
            let start_time = Instant::now();
            let mut stop_reason: Option<crate::ai::StopReason> = None;

            // Signal response start
            let _ = self.event_tx.send(FromAgent::ResponseStart {
                response_id: response_id.clone(),
            });

            // A previous turn may have been interrupted after recording
            // assistant tool calls (the select on the cancellation token can
            // drop this loop mid-await, skipping the cleanup below). Never
            // send a history with orphaned tool calls to the provider.
            self.repair_orphaned_tool_calls();

            // Bound the history before the request is built. Post-response
            // compaction cannot cover a tool chain, whose every response
            // carries tool calls, so without this the history grows until the
            // provider rejects it.
            // `record_step` above counts this attempt, so a count above one
            // means an earlier request in this turn already carried any
            // accepted user note to the provider.
            self.compact_before_request(step_budget.executed() > 1);

            // Make the API call
            let request_messages = project_observation_history(
                &self.messages,
                OBSERVATION_FULL_TURNS,
                self.active_tool_names.contains("recall_output"),
                |name, args| self.tool_executor.tool_context_effect(name, args),
            );
            let provider_messages =
                vault_provider_history_shared(&request_messages, &self.credential_vault)?;
            let (config, request_usage) = self
                .build_config_with_usage(&provider_messages, true)
                .await?;
            let prepared_request =
                ProviderSafeRequest::prepare(&provider_messages, config, &self.credential_vault)?;
            let provider_messages = &prepared_request.messages;
            let config = &prepared_request.config;
            let estimated_input_tokens = request_usage.total();
            let should_calibrate = estimated_input_tokens.is_some_and(|estimated| {
                !self.token_calibrated_models.contains(&config.model)
                    && self.compactor.should_calibrate_request(estimated)
            });
            if should_calibrate {
                // One non-generating count per model/session, only near the
                // compaction boundary. A failed or unsupported probe leaves
                // the existing heuristic intact and is not retried every turn.
                self.token_calibrated_models.insert(config.model.clone());
                if let (Some(client), Some(estimated)) =
                    (self.client.as_ref(), estimated_input_tokens)
                {
                    if let Ok(Some(observed)) = client
                        .count_input_tokens(provider_messages.as_slice(), config)
                        .await
                    {
                        self.compactor.calibrate_counter(estimated, observed);
                        tracing::info!(
                            target: "maestro.llm",
                            event = "context_token_counter_calibrated",
                            model = %config.model,
                            estimated_input_tokens = estimated,
                            observed_input_tokens = observed,
                        );
                    }
                }
            }
            let _ = self.event_tx.send(FromAgent::RequestContextPrepared {
                response_id: response_id.clone(),
            });
            let _ = self.event_tx.send(FromAgent::OperationObservation {
                observation: maestro_runtime_contracts::operation_observation::OperationObservation::Prepared {
                    response_id: response_id.clone(), model_id: config.model.clone(), model_provider: self.client.as_ref().map(|client| client.provider_name()).unwrap_or("unknown").to_owned(),
                    message_count: provider_messages.len().try_into().unwrap_or(u32::MAX),
                    input_size_bytes: serde_json::to_vec(provider_messages.as_ref()).ok().map(|v| v.len() as u64),
                },
            });
            let request_id = provider_request_id_with_tail(
                "primary",
                &config.model,
                provider_messages,
                config
                    .cache_topology
                    .as_ref()
                    .and_then(|prepared| prepared.volatile_tail()),
            )?;
            self.admit_provider_request("primary", &request_id, Some(&config.model))
                .await?;
            let client = self
                .client
                .as_ref()
                .context("direct provider client missing for native turn")?;
            prepared_request.ensure_current(&self.credential_vault)?;
            let mut rx = client
                .stream_owned_config_shared_messages_observed(
                    Arc::clone(provider_messages),
                    config.clone(),
                    Some(Arc::new({
                        let event_tx = self.event_tx.clone();
                        move |observation| {
                            let _ = event_tx.send(FromAgent::StreamObservation { observation });
                        }
                    })),
                )
                .await
                .map_err(model_dynamics::ProviderRequestFailure)?;

            // Collect the response
            let mut assistant_content: Vec<ContentBlock> = Vec::new();
            let mut current_text = String::new();
            let mut current_thinking = String::new();
            // Track active tool plus any pre-start deltas (index, id, name, json)
            let mut current_tool: Option<(
                usize,
                String,
                String,
                String,
                Option<maestro_ai::GeminiToolContext>,
            )> = None;
            let mut pending_tool_inputs: std::collections::HashMap<usize, String> =
                std::collections::HashMap::new();
            let mut usage = TokenUsage::default();
            // An OpenAI-compatible endpoint may omit the usage chunk entirely
            // (`packages/ai-rs/src/openai.rs` only emits `StreamEvent::Usage`
            // when the chunk carries one). Reporting the zero-valued default as
            // `Some(usage)` made "the provider says this turn cost nothing"
            // indistinguishable from "the provider said nothing", and a caller
            // metering the run believed the zero. The side-question loop
            // already made this distinction; the main turn loop did not.
            let mut saw_usage = false;
            let mut pending_tool_calls: Vec<(String, String, serde_json::Value, Option<String>)> =
                Vec::new();
            let mut stream_failed = false;
            let mut stream_error_message: Option<String> = None;
            let mut stream_error_kind: Option<ProviderStreamErrorKind> = None;
            let mut saw_stream_terminal = false;
            // Verdicts collected from `on_assistant_text_delta`, applied once
            // the provider response is complete so history is never left with
            // orphaned tool calls.
            let mut extension_text_block: Option<String> = None;
            let mut extension_text_steer: Vec<String> = Vec::new();
            let mut detected_text_loop: Option<LoopKind> = None;

            // Process stream events
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::ManagedGatewayReceipt(receipt) => {
                        let _ = self.event_tx.send(FromAgent::OperationObservation {
                            observation: maestro_runtime_contracts::operation_observation::OperationObservation::GatewayReceipt {
                                response_id: response_id.clone(), request_id: receipt.request_id.clone(),
                                record_id: receipt.record_id.clone(), lineage_id: receipt.lineage_id.clone(),
                                provider_tools_sha256: receipt.provider_tools_sha256.clone(), provider_tool_count: receipt.provider_tool_count,
                            },
                        });
                        let _ = self
                            .event_tx
                            .send(Self::managed_gateway_receipt_event(receipt, true));
                    }
                    StreamEvent::MessageStart { .. } => {}
                    StreamEvent::ContentBlockStart { index, block } => match &block {
                        ContentBlock::Text { text } => {
                            current_text = text.clone();
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            current_thinking = thinking.clone();
                        }
                        ContentBlock::ToolUse {
                            id,
                            name,
                            gemini_context,
                            ..
                        } => {
                            let buffered = pending_tool_inputs.remove(&index).unwrap_or_default();
                            current_tool = Some((
                                index,
                                id.clone(),
                                name.clone(),
                                buffered,
                                gemini_context.clone(),
                            ));
                        }
                        _ => {}
                    },
                    StreamEvent::TextDelta { text, .. } => {
                        current_text.push_str(&text);
                        match self.extensions.on_assistant_text_delta(&text) {
                            ExtensionVerdict::Proceed => {}
                            ExtensionVerdict::Block { reason } => {
                                if extension_text_block.is_none() {
                                    extension_text_block = Some(reason);
                                }
                            }
                            ExtensionVerdict::Steer { message } => {
                                if !extension_text_steer.contains(&message) {
                                    extension_text_steer.push(message);
                                }
                            }
                        }
                        // Check before rendering so the detector sees every
                        // delta exactly once and in order.
                        let text_loop = text_loop_detector
                            .add_text(&text, Instant::now() + TEXT_LOOP_CHECK_BUDGET);
                        let _ = self.event_tx.send(FromAgent::ResponseChunk {
                            response_id: response_id.clone(),
                            content: text,
                            is_thinking: false,
                        });
                        if let Some(kind) = text_loop {
                            // Stop reading the stream. Dropping `rx` ends the
                            // provider request, which is the point: the rest
                            // of this response is the same text again.
                            detected_text_loop = Some(kind);
                            saw_stream_terminal = true;
                            if !current_text.is_empty() {
                                assistant_content.push(ContentBlock::Text {
                                    text: std::mem::take(&mut current_text),
                                });
                            }
                            abort_pending_tools_after_stream_error(
                                &mut assistant_content,
                                &mut pending_tool_calls,
                            );
                            break;
                        }
                    }
                    StreamEvent::ThinkingDelta { thinking, .. } => {
                        current_thinking.push_str(&thinking);
                        let _ = self.event_tx.send(FromAgent::ResponseChunk {
                            response_id: response_id.clone(),
                            content: thinking,
                            is_thinking: true,
                        });
                    }
                    StreamEvent::ThinkingSignature { .. } => {
                        // Signature is captured in ContentBlockStop via parser state
                        // No action needed here - the signature is associated with the
                        // thinking block when the content block stops
                    }
                    StreamEvent::InputJsonDelta {
                        index,
                        partial_json,
                    } => {
                        // Deltas can precede a block start. Once the matching
                        // block is active, append only there; buffering as well
                        // would append the same bytes a second time at stop.
                        if let Some((active_index, _, _, ref mut json, _)) = current_tool {
                            if active_index == index {
                                json.push_str(&partial_json);
                                continue;
                            }
                        }
                        pending_tool_inputs
                            .entry(index)
                            .and_modify(|s| s.push_str(&partial_json))
                            .or_insert(partial_json);
                    }
                    StreamEvent::ContentBlockStop {
                        index: _,
                        thinking_signature,
                    } => {
                        // Finalize current content block
                        if !current_text.is_empty() {
                            assistant_content.push(ContentBlock::Text {
                                text: std::mem::take(&mut current_text),
                            });
                        }
                        append_completed_thinking_block(
                            &mut assistant_content,
                            &mut current_thinking,
                            thinking_signature,
                        );
                        if let Some((active_index, id, name, mut json, gemini_context)) =
                            current_tool.take()
                        {
                            // Merge any buffered deltas that arrived before the block start
                            if let Some(extra) = pending_tool_inputs.remove(&active_index) {
                                json.push_str(&extra);
                            }
                            let (input, parse_error) = match parse_tool_input(&name, &json) {
                                Ok(value) => (value, None),
                                Err(message) => (serde_json::json!({}), Some(message)),
                            };
                            let vaulted_input = self.credential_vault.vault_in_json(&input);
                            assistant_content.push(ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: vaulted_input.clone(),
                                gemini_context,
                            });
                            pending_tool_calls.push((id, name, input, parse_error));
                        }
                    }
                    StreamEvent::ReasoningUsage { tokens } => {
                        let _ = self.event_tx.send(FromAgent::OperationObservation {
                            observation: maestro_runtime_contracts::operation_observation::OperationObservation::ReasoningUsage { response_id: response_id.clone(), tokens },
                        });
                    }
                    StreamEvent::ProviderCost { cost_usd } => {
                        usage.cost = Some(cost_usd);
                    }
                    StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                    } => {
                        usage.input_tokens = input_tokens;
                        usage.output_tokens = output_tokens;
                        usage.cache_read_tokens = cache_read_tokens.unwrap_or(0);
                        usage.cache_write_tokens = cache_creation_tokens.unwrap_or(0);
                        saw_usage = true;
                    }
                    StreamEvent::MessageStop {
                        stop_reason: reason,
                    } => {
                        saw_stream_terminal = true;
                        stop_reason = reason;
                        // An output limit does not imply that the input context is full.
                        // Even valid JSON tool arguments can be only a prefix of the
                        // intended operation. Return explicit failures without execution.
                        if matches!(stop_reason, Some(StopReason::MaxTokens)) {
                            for (_, _, _, refusal) in &mut pending_tool_calls {
                                *refusal = Some(
                                    "not_executed: provider output was truncated at its token limit; request the complete tool call again".to_owned(),
                                );
                            }
                        }
                        break;
                    }
                    StreamEvent::Error { message } => {
                        saw_stream_terminal = true;
                        stream_failed = true;
                        stream_error_message = Some(message.clone());
                        abort_pending_tools_after_stream_error(
                            &mut assistant_content,
                            &mut pending_tool_calls,
                        );
                        break;
                    }
                    StreamEvent::ProviderError { kind, message } => {
                        saw_stream_terminal = true;
                        stream_failed = true;
                        stream_error_kind = Some(kind);
                        stream_error_message = Some(message.clone());
                        abort_pending_tools_after_stream_error(
                            &mut assistant_content,
                            &mut pending_tool_calls,
                        );
                        break;
                    }
                }
            }

            if !saw_stream_terminal {
                stream_failed = true;
                stream_error_kind = Some(ProviderStreamErrorKind::TransientProtocol);
                stream_error_message = Some(
                    "native provider stream ended before an explicit terminal event".to_string(),
                );
                abort_pending_tools_after_stream_error(
                    &mut assistant_content,
                    &mut pending_tool_calls,
                );
            }

            // Some provider streams repeat a terminal function-call item after
            // streaming its argument deltas. A duplicate tool result is invalid
            // for OpenAI-compatible APIs, so preserve only the first occurrence
            // of each call ID in both history and execution.
            let mut tool_use_ids = std::collections::HashSet::new();
            assistant_content.retain(|block| match block {
                ContentBlock::ToolUse { id, .. } => tool_use_ids.insert(id.clone()),
                _ => true,
            });
            let mut pending_call_ids = std::collections::HashSet::new();
            pending_tool_calls
                .retain(|(call_id, _, _, _)| pending_call_ids.insert(call_id.clone()));

            let process_usage = self
                .process_budget
                .as_ref()
                .map(|state| {
                    if !saw_usage {
                        return Err(anyhow::anyhow!("process response omitted usage"));
                    }
                    state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                        .observe_usage(
                            // Provider adapters normalize input into disjoint buckets.
                            // Cached tokens still consume the process token budget.
                            usage
                                .input_tokens
                                .checked_add(usage.cache_read_tokens)
                                .and_then(|tokens| tokens.checked_add(usage.cache_write_tokens))
                                .ok_or_else(|| anyhow::anyhow!("process input usage overflow"))?,
                            usage.output_tokens,
                            usage.cost.map(process_provider_cost_micros).transpose()?,
                        )
                        .map_err(anyhow::Error::msg)
                })
                .transpose();
            if let Err(error) = process_usage {
                if !stream_failed {
                    let _ = self.event_tx.send(FromAgent::LocalAssistantContent {
                        response_id: response_id.clone(),
                        content: assistant_content.clone(),
                    });
                }
                if !assistant_content.is_empty() {
                    self.messages_mut().push(Message {
                        role: Role::Assistant,
                        content: MessageContent::Blocks(assistant_content),
                    });
                }
                self.refuse_tool_batch(pending_tool_calls, &error.to_string());
                return Err(error);
            }

            // Mark the cleanup-sensitive interval before storing ToolUse
            // history, closing the gap where outer request cancellation could
            // otherwise leave an orphaned provider message.
            self.set_tool_batch_active(!pending_tool_calls.is_empty());

            let response_text = assistant_content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            if stream_failed {
                self.set_tool_batch_active(false);
                // The request still consumed provider output, but a partial
                // response is not authoritative assistant history and must
                // not run success-oriented post-message hooks.
                self.output_tokens_spent =
                    self.output_tokens_spent.saturating_add(usage.output_tokens);
                let last_assistant = (!response_text.is_empty()).then_some(response_text.as_str());
                let _ = self
                    .hooks
                    .hook_stop_failure("api_error", stream_error_message.as_deref(), last_assistant)
                    .await;
                let message = stream_error_message.unwrap_or_else(|| "stream failed".to_string());
                return match stream_error_kind {
                    Some(kind) => Err(anyhow::Error::new(ProviderStreamFailure { kind, message })),
                    None => Err(model_dynamics::ProviderRequestFailure(anyhow::anyhow!(
                        "{message}"
                    ))
                    .into()),
                };
            }

            if let Some(kind) = detected_text_loop {
                // The unified stream owns both its retry forwarder and the
                // provider's HTTP/SSE producer. Confirm both have released
                // the abandoned response before starting the steered retry;
                // merely dropping the receiver can leave either task running.
                rx.cancel_and_wait()
                    .await
                    .context("failed to stop looping provider stream")?;
                // The response is real output the provider billed, so charge
                // it and record it as assistant history before deciding what
                // to do about the repetition.
                self.output_tokens_spent =
                    self.output_tokens_spent.saturating_add(usage.output_tokens);
                let _ = self.event_tx.send(FromAgent::LocalAssistantContent {
                    response_id: response_id.clone(),
                    content: assistant_content.clone(),
                });
                if !assistant_content.is_empty() {
                    self.messages_mut().push(Message {
                        role: Role::Assistant,
                        content: MessageContent::Blocks(assistant_content),
                    });
                }
                let _ = self.event_tx.send(FromAgent::ResponseEnd {
                    response_id: response_id.clone(),
                    usage: saw_usage.then_some(usage),
                });
                self.tool_executor.report_diagnostic(format!(
                    "[agent] assistant text loop detected (kind={}, repetitions={}, already_steered={steered_after_text_loop}): {}",
                    kind.label(),
                    kind.repetitions(),
                    kind.preview(),
                ));
                if steered_after_text_loop {
                    // One reminder is the whole budget. A model that loops
                    // again after being told is not going to stop, and
                    // retrying costs the user another full generation.
                    return Err(anyhow::Error::new(AssistantTextLoop { kind }));
                }
                steered_after_text_loop = true;
                let _ = self.event_tx.send(FromAgent::Status {
                    message: "Model output was repeating; steering once and retrying.".to_string(),
                });
                self.messages_mut().push(Message {
                    role: Role::User,
                    content: MessageContent::text(loop_reminder_message(&kind)),
                });
                if self.drain_pending_commands().await {
                    self.repair_orphaned_tool_calls();
                    return Err(anyhow::anyhow!("Request cancelled"));
                }
                continue 'turn;
            }

            if response_text.trim().is_empty() && pending_tool_calls.is_empty() {
                let provider = self
                    .client
                    .as_ref()
                    .map(UnifiedClient::provider_name)
                    .unwrap_or("unknown");
                tracing::warn!(
                    target: "maestro.provider",
                    event = "provider_empty_assistant_response",
                    provider,
                    model = %self.config.model,
                    normalized_blocks = assistant_content.len(),
                    saw_usage,
                    output_tokens = usage.output_tokens,
                );
                self.tool_executor.report_diagnostic(format!(
                    "[agent] provider returned no assistant text or tool calls (provider={provider}, model={}, normalized_blocks={}, saw_usage={saw_usage}, output_tokens={})",
                    self.config.model,
                    assistant_content.len(),
                    usage.output_tokens,
                ));
                // A billed empty completion is thinking-only or a stripped
                // thought turn, not a dropped connection. Retrying the same
                // request reproduces it; one continuation is the recovery.
                if saw_usage && usage.output_tokens > 0 && !steered_after_billed_empty {
                    self.output_tokens_spent =
                        self.output_tokens_spent.saturating_add(usage.output_tokens);
                    let _ = self.event_tx.send(FromAgent::LocalAssistantContent {
                        response_id: response_id.clone(),
                        content: assistant_content.clone(),
                    });
                    if !assistant_content.is_empty() {
                        self.messages_mut().push(Message {
                            role: Role::Assistant,
                            content: MessageContent::Blocks(assistant_content),
                        });
                    }
                    let _ = self.event_tx.send(FromAgent::ResponseEnd {
                        response_id: response_id.clone(),
                        usage: Some(usage),
                    });
                    steered_after_billed_empty = true;
                    let _ = self.event_tx.send(FromAgent::Status {
                        message: "Model billed tokens with no assistant text; steering once and retrying."
                            .to_string(),
                    });
                    self.messages_mut().push(Message {
                        role: Role::User,
                        content: MessageContent::text(billed_empty_reminder_message()),
                    });
                    if self.drain_pending_commands().await {
                        self.repair_orphaned_tool_calls();
                        return Err(anyhow::anyhow!("Request cancelled"));
                    }
                    continue 'turn;
                }
                self.set_tool_batch_active(false);
                return Err(anyhow::Error::new(EmptyAssistantResponse));
            }

            // Shadow calibration only: keep compaction thresholds unchanged until
            // real estimation error is measured. Never mix in summarizer usage.
            if saw_usage {
                if let (Some(estimated), Some(prepared)) =
                    (estimated_input_tokens, &config.cache_topology)
                {
                    if let Some(observation) =
                        maestro_context::context_usage::ContextCalibration::from_usage(
                            request_id.clone(),
                            prepared.topology().generation,
                            estimated,
                            usage.input_tokens,
                            usage.cache_read_tokens,
                            usage.cache_write_tokens,
                        )
                    {
                        let _ = self
                            .event_tx
                            .send(FromAgent::ContextCalibration { observation });
                    }
                }
            }
            step_budget.accept_attempt();

            // Persist the completed provider blocks before tool execution
            // events. Display state has neither those calls yet nor thinking
            // signatures.
            let _ = self.event_tx.send(FromAgent::LocalAssistantContent {
                response_id: response_id.clone(),
                content: assistant_content.clone(),
            });

            // Add assistant message to history
            if !assistant_content.is_empty() {
                self.messages_mut().push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_content),
                });
            }

            let duration_ms = start_time.elapsed().as_millis() as u64;
            let stop_reason_label = stop_reason.map(Self::stop_reason_label);

            // Charge this response against any cumulative output budget before
            // the next request is built; `build_config` reads the running total.
            self.output_tokens_spent = self.output_tokens_spent.saturating_add(usage.output_tokens);

            // Complete optional summarization while this response still owns
            // the turn. Its billed usage is included in the response total.
            let prepared_compaction = if pending_tool_calls.is_empty()
                && self.compactor.should_auto_compact(&self.messages)
            {
                let compaction_started = Instant::now();
                let result = self.compactor.compact_with_tokens(&self.messages);
                let result = self
                    .enhance_compaction(result, &mut usage, &mut saw_usage)
                    .await;
                if result.was_compacted() {
                    let _ = self.event_tx.send(FromAgent::CompactionMeasured {
                        duration_ms: compaction_started
                            .elapsed()
                            .as_millis()
                            .min(u64::MAX as u128) as u64,
                    });
                }
                Some(result)
            } else {
                None
            };

            // The current user message is already in the JSONL. Snapshot its
            // size before ResponseEnd asks the UI to append the assistant turn,
            // so Session History waits for that exact persistence boundary.
            self.hooks
                .hook_checkpoint_transcript_before_response()
                .await;

            // Signal response end. `None` means the provider reported nothing
            // for this turn, which is not the same as reporting zero.
            let _ = self.event_tx.send(FromAgent::ResponseEnd {
                response_id: response_id.clone(),
                usage: saw_usage.then_some(usage.clone()),
            });

            // ResponseEnd is enqueued first so the UI can append and flush the
            // canonical JSONL while the PostMessage capture hook waits for the
            // file to cross its pre-response size boundary.
            let _ = self
                .hooks
                .hook_post_message(
                    &response_text,
                    usage.input_tokens,
                    usage.output_tokens,
                    duration_ms,
                    stop_reason_label,
                )
                .await;

            // An extension voted to stop the assistant mid-stream. End the turn
            // now that the provider response is complete.
            if let Some(reason) = extension_text_block {
                let _ = self.event_tx.send(FromAgent::Error {
                    message: reason,
                    fatal: false,
                    terminal: false,
                    retryable: false,
                });
                self.set_tool_batch_active(false);
                self.repair_orphaned_tool_calls();
                break 'turn;
            }

            // An extension asked to redirect the model. Queue the text as a
            // steering prompt, which the existing next-turn drain picks up.
            for message in std::mem::take(&mut extension_text_steer) {
                let _ = self.event_tx.send(FromAgent::Status {
                    message: message.clone(),
                });
                self.pending_messages
                    .push_with_kind(message, PromptKind::Steer);
            }

            if self.drain_pending_commands().await {
                self.repair_orphaned_tool_calls();
                return Err(anyhow::anyhow!("Request cancelled"));
            }

            // A tool batch costs another provider round trip: the runner has
            // to ask the model again with the results. When the turn cannot
            // afford that round trip, executing the batch would produce work
            // the model never sees, so the batch is refused explicitly and the
            // turn ends here.
            if !pending_tool_calls.is_empty() && !step_budget.can_continue() {
                let unexecuted_tools = self.refuse_tool_batch_over_step_budget(
                    pending_tool_calls,
                    step_budget.max_steps(),
                );
                self.set_tool_batch_active(false);
                let outcome: TurnOutcome = step_budget.exhausted(unexecuted_tools);
                return Err(anyhow::Error::new(outcome));
            }

            if let Err(reason) = pending_tool_calls
                .iter()
                .try_for_each(|(_, name, args, _)| step_budget.admit_tool(name, args))
            {
                self.refuse_tool_batch(pending_tool_calls, reason);
                self.set_tool_batch_active(false);
                return Err(anyhow::anyhow!(reason));
            }

            let process_tools = self
                .process_budget
                .as_ref()
                .map(|state| {
                    state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                        .admit_tools(pending_tool_calls.len())
                        .map_err(anyhow::Error::msg)
                })
                .transpose();
            if let Err(error) = process_tools {
                self.refuse_tool_batch(pending_tool_calls, &error.to_string());
                self.set_tool_batch_active(false);
                return Err(error);
            }

            // If there are tool calls, handle them
            if !pending_tool_calls.is_empty() {
                let mut tool_results: Vec<ContentBlock> = Vec::new();
                let mut deferred_steering: Vec<PendingMessage> = Vec::new();
                let mut deferred_tool_calls: Vec<DeferredToolCall> = Vec::new();
                let mut remaining_tool_calls: Vec<(
                    String,
                    String,
                    serde_json::Value,
                    Option<String>,
                )> = Vec::new();
                let mut pending_tool_calls_iter = pending_tool_calls.into_iter();
                let mut pending_read_only_tool_calls: Vec<QueuedReadOnlyToolExecution> = Vec::new();
                let mut processed_any_tool = false;

                while let Some((call_id, tool_name, args, parse_error)) =
                    pending_tool_calls_iter.next()
                {
                    self.tool_response_coordinator.remove_cancelled(&call_id);
                    if processed_any_tool {
                        if self.drain_pending_commands().await {
                            if !tool_results.is_empty() {
                                self.messages_mut().push(Message {
                                    role: Role::User,
                                    content: MessageContent::Blocks(std::mem::take(
                                        &mut tool_results,
                                    )),
                                });
                            }
                            self.repair_orphaned_tool_calls();
                            return Err(anyhow::anyhow!("Request cancelled"));
                        }
                        deferred_steering = self.dequeue_next_turn_messages(false);
                        if !deferred_steering.is_empty() {
                            self.drain_read_only_tool_calls(
                                &mut pending_read_only_tool_calls,
                                &mut tool_results,
                            )
                            .await?;
                            remaining_tool_calls.push((call_id, tool_name, args, parse_error));
                            remaining_tool_calls.extend(pending_tool_calls_iter);
                            break;
                        }
                    }
                    processed_any_tool = true;

                    if let Some(message) = parse_error {
                        self.drain_read_only_tool_calls(
                            &mut pending_read_only_tool_calls,
                            &mut tool_results,
                        )
                        .await?;
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: message.clone(),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id.clone(),
                            content: message,
                            is_error: Some(true),
                        });
                        continue;
                    }
                    let tool_key = tool_name.to_lowercase();
                    if !self.tools.contains_key(&tool_key) {
                        self.drain_read_only_tool_calls(
                            &mut pending_read_only_tool_calls,
                            &mut tool_results,
                        )
                        .await?;
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id,
                            content: format!("Tool `{tool_name}` is not available in this run"),
                            is_error: Some(true),
                        });
                        continue;
                    }

                    // Preserve the model-provided input so a call deferred
                    // behind an approval boundary can rerun PreToolUse
                    // against current state without applying an earlier hook
                    // rewrite a second time.
                    let pre_hook_args = args.clone();

                    // Execute PreToolUse hooks
                    let hook_result = self
                        .hooks
                        .hook_pre_tool_use(&tool_name, &call_id, &pre_hook_args)
                        .await;

                    // Handle hook results
                    let (args, extra_context) = match hook_result {
                        NativeHookResult::Block { reason } => {
                            self.drain_read_only_tool_calls(
                                &mut pending_read_only_tool_calls,
                                &mut tool_results,
                            )
                            .await?;
                            // Hook blocked the tool - return error to model
                            let _ = self.event_tx.send(FromAgent::HookBlocked {
                                call_id: call_id.clone(),
                                tool: tool_name.clone(),
                                reason: reason.clone(),
                            });
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: call_id,
                                content: format!("Tool blocked by hook: {reason}"),
                                is_error: Some(true),
                            });
                            continue;
                        }
                        NativeHookResult::ModifyInput { new_input } => {
                            // Use modified input
                            (new_input, None)
                        }
                        NativeHookResult::InjectContext { context } => {
                            // Keep original args, but track context to append
                            (args.clone(), Some(context))
                        }
                        NativeHookResult::Continue => {
                            // No modification
                            (args.clone(), None)
                        }
                    };

                    // Hooks may replace the complete input, so normalize and
                    // validate only after applying their result.
                    let (args, rewrote_empty_bash) =
                        normalize_post_hook_tool_args(&tool_name, args);
                    if rewrote_empty_bash {
                        let _ = self.event_tx.send(FromAgent::Status {
                            message:
                                "Received empty bash tool call; auto-filled command as \"pwd\" to proceed."
                                    .to_string(),
                        });
                    }
                    let missing = self.tool_executor.missing_required(&tool_name, &args);
                    if !missing.is_empty() {
                        self.drain_read_only_tool_calls(
                            &mut pending_read_only_tool_calls,
                            &mut tool_results,
                        )
                        .await?;
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id.clone(),
                            content: format!(
                                "Missing required fields for tool '{}': {}",
                                tool_name,
                                missing.join(", ")
                            ),
                            is_error: Some(true),
                        });
                        continue;
                    }

                    let safe_args = self.credential_vault.vault_in_json(&args);

                    // Ask the registered extensions whether this call runs.
                    // The `doom-loop` tenant answers with the doom-loop and
                    // rate-limit verdicts this branch used to read directly.
                    match self.plan_tool_call_through_extensions(&call_id, &tool_name, &safe_args) {
                        ExtensionVerdict::Proceed => {
                            // Proceed with tool execution
                        }
                        ExtensionVerdict::Block { reason } => {
                            self.drain_read_only_tool_calls(
                                &mut pending_read_only_tool_calls,
                                &mut tool_results,
                            )
                            .await?;
                            let _ = self.event_tx.send(FromAgent::Error {
                                message: reason.clone(),
                                fatal: false,
                                terminal: false,
                                retryable: false,
                            });
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: call_id,
                                content: reason,
                                is_error: Some(true),
                            });
                            continue;
                        }
                        ExtensionVerdict::Steer { message } => {
                            // The tool does not run, but the model is told why
                            // in a result it is not meant to read as a failure.
                            self.drain_read_only_tool_calls(
                                &mut pending_read_only_tool_calls,
                                &mut tool_results,
                            )
                            .await?;
                            let _ = self.event_tx.send(FromAgent::Status {
                                message: message.clone(),
                            });
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: call_id,
                                content: message,
                                is_error: Some(false),
                            });
                            continue;
                        }
                    }

                    let workflow_snapshot = self.workflow_state.snapshot();
                    // Ensure MCP annotations are loaded before firewall check
                    if self.tool_executor.is_mcp_tool(&tool_key) {
                        if let Err(error) = self.tool_executor.ensure_mcp_annotations().await {
                            self.tool_executor.report_diagnostic(format!(
                                "[agent] failed to refresh MCP annotations for {tool_key}: {error}"
                            ));
                        }
                    }
                    let is_external_tool = self.external_tools.contains(&tool_key);
                    let annotations = self.tool_executor.tool_annotations(&tool_key);
                    let firewall_verdict = if is_external_tool {
                        // The caller owns execution and applies its own sandbox and approval
                        // policy. The native firewall only governs native executors.
                        NativeFirewallVerdict::Allow
                    } else {
                        self.tool_executor.firewall_verdict(
                            &tool_key,
                            &safe_args,
                            &workflow_snapshot,
                            annotations.as_ref(),
                            false,
                        )
                    };
                    if let NativeFirewallVerdict::Block { reason } = &firewall_verdict {
                        self.drain_read_only_tool_calls(
                            &mut pending_read_only_tool_calls,
                            &mut tool_results,
                        )
                        .await?;
                        let _ = self.event_tx.send(FromAgent::Error {
                            message: reason.clone(),
                            fatal: false,
                            terminal: false,
                            retryable: false,
                        });
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id,
                            content: format!("Tool blocked by action firewall: {reason}"),
                            is_error: Some(true),
                        });
                        continue;
                    }

                    // Check if this tool requires approval. This is the ONE
                    // decision point for whether the runner executes inline
                    // below -- see `tool_requires_approval`'s doc comment.
                    let approval_decision = tool_requires_approval(
                        self.config.approval_mode,
                        is_external_tool,
                        &firewall_verdict,
                        &self.tool_executor,
                        &tool_name,
                        &safe_args,
                        &self.denial_memory,
                    );
                    // The user already refused this exact call in this turn.
                    // Answer from that decision instead of asking again.
                    if approval_decision.is_repeat_refusal() {
                        self.drain_read_only_tool_calls(
                            &mut pending_read_only_tool_calls,
                            &mut tool_results,
                        )
                        .await?;
                        let message = repeat_refusal_message(&tool_name);
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id,
                            content: message,
                            is_error: Some(true),
                        });
                        continue;
                    }
                    let requires_approval = approval_decision.requires_approval();

                    // `PermissionRequest` hooks are documented to run when a
                    // tool needs approval (docs/design/HOOKS_SYSTEM.md). This is
                    // the one place that decides that, so it is the only place
                    // the hook can run without disagreeing with the decision.
                    // A `Block` denies the call outright and the user is never
                    // asked; every other result falls through to the normal
                    // approval path, because an approval gate has nothing to do
                    // with modified input or injected context.
                    if requires_approval {
                        let permission = self
                            .hooks
                            .hook_permission_request(
                                &tool_name,
                                &call_id,
                                &args,
                                "tool requires approval",
                            )
                            .await;
                        if let NativeHookResult::Block { reason } = permission {
                            self.drain_read_only_tool_calls(
                                &mut pending_read_only_tool_calls,
                                &mut tool_results,
                            )
                            .await?;
                            let message = format!("Tool denied by permission hook: {reason}");
                            let _ = self.event_tx.send(FromAgent::Error {
                                message: message.clone(),
                                fatal: false,
                                terminal: false,
                                retryable: false,
                            });
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: call_id,
                                content: message,
                                is_error: Some(true),
                            });
                            continue;
                        }
                    }

                    let can_parallelize_read_only = is_native_parallel_read_only_tool_call(
                        &tool_key,
                        requires_approval,
                        annotations.as_ref(),
                        is_explicit_inline_read_only_tool(&tool_key, &self.tool_executor),
                    );

                    if !can_parallelize_read_only {
                        self.drain_read_only_tool_calls(
                            &mut pending_read_only_tool_calls,
                            &mut tool_results,
                        )
                        .await?;
                    }

                    let deferred_disposition = deferred_tool_call_disposition(
                        requires_approval,
                        !deferred_tool_calls.is_empty(),
                    );
                    if deferred_disposition == Some(DeferredToolCallDisposition::AwaitApproval) {
                        // Defer the wait for the user's decision: emit every
                        // ToolCall event in this batch before awaiting any
                        // decisions so the UI can present one batched modal
                        // (#3085). Capture execution context before publishing
                        // it, then carry that same snapshot to both the UI and
                        // the execution-boundary comparison.
                        let approval_inline_env =
                            self.tool_executor.inline_tool_approval_context(&tool_name);
                        let call = ToolCallContext {
                            call_id,
                            tool_name,
                            args,
                            safe_args,
                            extra_context,
                            pre_hook_args,
                            initial_firewall_verdict: firewall_verdict,
                            approval_inline_env,
                        };
                        let _ = self.event_tx.send(deferred_tool_call_event(&call, true));
                        deferred_tool_calls.push(DeferredToolCall::AwaitApproval(call));
                        continue;
                    }

                    if deferred_disposition == Some(DeferredToolCallDisposition::Execute) {
                        // Preserve the model's tool-call order after an
                        // approval boundary. Delay this auto-approved call's
                        // ToolCall event until its refreshed PreToolUse input
                        // is known, so the emitted and executed inputs match.
                        deferred_tool_calls.push(DeferredToolCall::Execute(ToolCallContext {
                            call_id,
                            tool_name,
                            args,
                            safe_args,
                            extra_context,
                            pre_hook_args,
                            initial_firewall_verdict: firewall_verdict,
                            approval_inline_env: None,
                        }));
                        continue;
                    }

                    let _ = self.event_tx.send(FromAgent::ToolCall {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        args: safe_args.clone(),
                        requires_approval,
                        approval_inline_env: None,
                    });

                    if can_parallelize_read_only {
                        let execution_args = tool_args_for_execution(&safe_args);
                        pending_read_only_tool_calls.push(QueuedReadOnlyToolExecution {
                            call_id,
                            tool_name,
                            args: safe_args.clone(),
                            safe_args,
                            execution_args,
                            extra_context,
                        });
                        continue;
                    }

                    // Auto-approved, execute immediately
                    // Note: ToolExecutor sends ToolStart/ToolEnd events internally
                    let result = {
                        let execution_args = tool_args_for_execution(&safe_args);
                        self.execute_tool(&tool_name, &execution_args, &call_id, None)
                            .await
                    };
                    let tool_name_for_cache = tool_name.clone();
                    let call = ToolCallContext {
                        call_id,
                        tool_name,
                        args,
                        safe_args,
                        extra_context,
                        pre_hook_args,
                        initial_firewall_verdict: firewall_verdict,
                        approval_inline_env: None,
                    };
                    let result_block = self
                        .finalize_tool_call_result(call, true, Some(result))
                        .await;
                    tool_results.push(result_block);
                    // Serial tools may mutate state through bash, inline, MCP,
                    // or external execution. Reads that follow in this model
                    // batch must not reuse entries cached before that call.
                    invalidate_cache_after_serial_tool(
                        &self.tool_executor,
                        &tool_name_for_cache,
                        true,
                    );
                }

                self.drain_read_only_tool_calls(
                    &mut pending_read_only_tool_calls,
                    &mut tool_results,
                )
                .await?;

                // Every ToolCall event in this batch has been emitted. Now
                // execute the deferred suffix in model order, awaiting gated
                // decisions in FIFO order. Responses that arrive out of order
                // are stashed by wait_for_tool_response until their turn.
                let mut deferred_tool_calls_iter =
                    std::mem::take(&mut deferred_tool_calls).into_iter();
                if self.take_active_operation_interruption() {
                    let cancelled_ids = cancel_deferred_suffix(
                        &self.event_tx,
                        deferred_tool_calls_iter.by_ref(),
                        &mut tool_results,
                        self.tool_executor.managed_policy_metadata(),
                    );
                    self.tool_response_coordinator
                        .discard_cancelled(&cancelled_ids);
                }
                while let Some(deferred_call) = deferred_tool_calls_iter.next() {
                    match deferred_call {
                        DeferredToolCall::AwaitApproval(mut call) => {
                            let approval_cancel = self.shutdown_token.child_token();
                            self.set_active_approval_cancel_token(Some(approval_cancel.clone()));
                            let approval_started = Instant::now();
                            let approval = approval_span();
                            let response = self
                                .tool_response_coordinator
                                .wait_for_tool_response(&call.call_id, &approval_cancel)
                                .instrument(approval.clone())
                                .await;
                            self.set_active_approval_cancel_token(None);
                            let (approval_outcome, approval_error) = match &response {
                                ToolResponseWait::Response((approved, _, _)) if *approved => {
                                    ("approved", None)
                                }
                                ToolResponseWait::Response(_) => {
                                    ("denied", Some("approval_denied"))
                                }
                                ToolResponseWait::Cancelled => {
                                    ("cancelled", Some("approval_cancelled"))
                                }
                                ToolResponseWait::Closed => {
                                    ("closed", Some("approval_channel_closed"))
                                }
                            };
                            record_outcome(
                                &approval,
                                approval_outcome,
                                approval_started.elapsed(),
                                approval_error,
                            );
                            let (approved, mut result, source) = match response {
                                ToolResponseWait::Response(response) => response,
                                ToolResponseWait::Cancelled => {
                                    self.take_active_operation_interruption();
                                    let skipped_message = "Skipped after request cancellation.";
                                    let _ = self.event_tx.send(FromAgent::ToolOutput {
                                        call_id: call.call_id.clone(),
                                        content: skipped_message.to_string(),
                                    });
                                    let mut cancelled_ids = HashSet::from([call.call_id.clone()]);
                                    let (event, result_block) = cancelled_deferred_tool(
                                        &call,
                                        skipped_message,
                                        self.tool_executor.managed_policy_metadata(),
                                    );
                                    let _ = self.event_tx.send(event);
                                    tool_results.push(result_block);
                                    cancelled_ids.extend(cancel_deferred_suffix(
                                        &self.event_tx,
                                        deferred_tool_calls_iter.by_ref(),
                                        &mut tool_results,
                                        self.tool_executor.managed_policy_metadata(),
                                    ));
                                    self.tool_response_coordinator
                                        .discard_cancelled(&cancelled_ids);
                                    break;
                                }
                                ToolResponseWait::Closed => {
                                    return Err(closed_tool_response_failure(&call.call_id));
                                }
                            };
                            let is_external_tool = self
                                .external_tools
                                .contains(&call.tool_name.to_ascii_lowercase());
                            if approved && result.is_some() && !is_external_tool {
                                let message = "Caller-supplied tool results are accepted only for registered external tools.";
                                let _ = self.event_tx.send(FromAgent::Error {
                                    message: message.to_string(),
                                    fatal: false,
                                    terminal: false,
                                    retryable: false,
                                });
                                result = Some(ToolResult::failure(message));
                            }
                            if approved && result.is_none() {
                                let (args, extra_context) =
                                    match rerun_deferred_pre_tool_use(&self.hooks, &call).await {
                                        Ok(result) => result,
                                        Err(reason) => {
                                            let (events, result_block) = deferred_hook_block(
                                                &call,
                                                reason,
                                                false,
                                                self.tool_executor.managed_policy_metadata(),
                                            );
                                            for event in events {
                                                let _ = self.event_tx.send(event);
                                            }
                                            tool_results.push(result_block);
                                            if self.cancel_remaining_deferred_if_interrupted(
                                                &mut deferred_tool_calls_iter,
                                                &mut tool_results,
                                            ) {
                                                break;
                                            }
                                            continue;
                                        }
                                    };
                                let (args, rewrote_empty_bash) =
                                    normalize_post_hook_tool_args(&call.tool_name, args);
                                if rewrote_empty_bash {
                                    let _ = self.event_tx.send(FromAgent::Status {
                                        message:
                                            "Received empty bash tool call; auto-filled command as \"pwd\" to proceed."
                                                .to_string(),
                                    });
                                }
                                let missing =
                                    self.tool_executor.missing_required(&call.tool_name, &args);
                                if !missing.is_empty() {
                                    let reason = format!(
                                        "Missing required fields for tool '{}': {}",
                                        call.tool_name,
                                        missing.join(", ")
                                    );
                                    emit_deferred_failure(
                                        &self.event_tx,
                                        &call,
                                        &reason,
                                        &mut tool_results,
                                        self.tool_executor.managed_policy_metadata(),
                                    );
                                    if self.cancel_remaining_deferred_if_interrupted(
                                        &mut deferred_tool_calls_iter,
                                        &mut tool_results,
                                    ) {
                                        break;
                                    }
                                    continue;
                                }
                                if let Some(reason) =
                                    approved_input_change_rejection(&call.args, &args)
                                {
                                    emit_deferred_failure(
                                        &self.event_tx,
                                        &call,
                                        reason,
                                        &mut tool_results,
                                        self.tool_executor.managed_policy_metadata(),
                                    );
                                    if self.cancel_remaining_deferred_if_interrupted(
                                        &mut deferred_tool_calls_iter,
                                        &mut tool_results,
                                    ) {
                                        break;
                                    }
                                    continue;
                                }
                                call.args = args;
                                call.safe_args = self.credential_vault.vault_in_json(&call.args);
                                call.extra_context = extra_context;

                                let tool_key = call.tool_name.to_lowercase();
                                if self.tool_executor.is_mcp_tool(&tool_key) {
                                    let _ = self.tool_executor.ensure_mcp_annotations().await;
                                }
                                let is_external_tool = self.external_tools.contains(&tool_key);
                                let annotations = self.tool_executor.tool_annotations(&tool_key);
                                let workflow_snapshot = self.workflow_state.snapshot();
                                let firewall_verdict = deferred_firewall_verdict(
                                    &self.tool_executor,
                                    &tool_key,
                                    &call.safe_args,
                                    &workflow_snapshot,
                                    annotations.as_ref(),
                                    is_external_tool,
                                );
                                let policy_rejection = deferred_approved_policy_rejection(
                                    &call.initial_firewall_verdict,
                                    firewall_verdict,
                                );
                                if let Some(reason) = policy_rejection {
                                    emit_deferred_policy_failure(
                                        &self.event_tx,
                                        &call,
                                        &reason,
                                        &mut tool_results,
                                        self.tool_executor.managed_policy_metadata(),
                                    );
                                    if self.cancel_remaining_deferred_if_interrupted(
                                        &mut deferred_tool_calls_iter,
                                        &mut tool_results,
                                    ) {
                                        break;
                                    }
                                    continue;
                                }
                                if let Some(approved_context) = &call.approval_inline_env {
                                    let current_env = self
                                        .tool_executor
                                        .inline_tool_approval_context(&tool_key)
                                        .map(|context| context.environment);
                                    if let Some(reason) = approved_inline_env_change_rejection(
                                        Some(&approved_context.environment),
                                        current_env.as_ref(),
                                    ) {
                                        emit_deferred_failure(
                                            &self.event_tx,
                                            &call,
                                            reason,
                                            &mut tool_results,
                                            self.tool_executor.managed_policy_metadata(),
                                        );
                                        if self.cancel_remaining_deferred_if_interrupted(
                                            &mut deferred_tool_calls_iter,
                                            &mut tool_results,
                                        ) {
                                            break;
                                        }
                                        continue;
                                    }
                                }
                                let deferred_verdict = self.plan_tool_call_through_extensions(
                                    &call.call_id,
                                    &call.tool_name,
                                    &call.safe_args,
                                );
                                match deferred_verdict {
                                    ExtensionVerdict::Proceed => {}
                                    ExtensionVerdict::Block { reason }
                                    | ExtensionVerdict::Steer { message: reason } => {
                                        // The call was already announced to the
                                        // UI as running, so a steer is reported
                                        // the same way a block is.
                                        emit_deferred_failure(
                                            &self.event_tx,
                                            &call,
                                            &reason,
                                            &mut tool_results,
                                            self.tool_executor.managed_policy_metadata(),
                                        );
                                        if self.cancel_remaining_deferred_if_interrupted(
                                            &mut deferred_tool_calls_iter,
                                            &mut tool_results,
                                        ) {
                                            break;
                                        }
                                        continue;
                                    }
                                }
                            }
                            let result = if approved {
                                // `source` is whatever the responder on the
                                // other end of the tool-response channel
                                // actually sent (the TUI approval dialog sends
                                // `ExecutionSource::Native`; a headless/remote
                                // client sends `RemoteClient`) -- never
                                // hardcoded here, so a locally-approved
                                // batched tool call is not mislabeled as
                                // remote-originated.
                                result.map(|result| {
                                    ToolExecution::from_legacy(
                                        &call.call_id,
                                        &call.tool_name,
                                        source,
                                        result,
                                    )
                                    .with_managed_policy(
                                        self.tool_executor.managed_policy_metadata(),
                                    )
                                })
                            } else {
                                Some(
                                    ToolExecution::denied(
                                        &call.call_id,
                                        &call.tool_name,
                                        DenialReason::User,
                                    )
                                    .with_managed_policy(
                                        self.tool_executor.managed_policy_metadata(),
                                    ),
                                )
                            };
                            let tool_name_for_cache = call.tool_name.clone();
                            let result_block =
                                self.finalize_tool_call_result(call, approved, result).await;
                            tool_results.push(result_block);
                            invalidate_cache_after_serial_tool(
                                &self.tool_executor,
                                &tool_name_for_cache,
                                approved,
                            );
                        }
                        DeferredToolCall::Execute(mut call) => {
                            // PreToolUse may depend on filesystem or workflow
                            // state changed by an earlier approved mutation.
                            // Re-run it at the actual execution boundary using
                            // the original model input, then rebuild every
                            // derived argument form from that fresh decision.
                            let (args, extra_context) =
                                match rerun_deferred_pre_tool_use(&self.hooks, &call).await {
                                    Ok(result) => result,
                                    Err(reason) => {
                                        let (events, result_block) = deferred_hook_block(
                                            &call,
                                            reason,
                                            true,
                                            self.tool_executor.managed_policy_metadata(),
                                        );
                                        for event in events {
                                            let _ = self.event_tx.send(event);
                                        }
                                        tool_results.push(result_block);
                                        if self.cancel_remaining_deferred_if_interrupted(
                                            &mut deferred_tool_calls_iter,
                                            &mut tool_results,
                                        ) {
                                            break;
                                        }
                                        continue;
                                    }
                                };
                            let (args, rewrote_empty_bash) =
                                normalize_post_hook_tool_args(&call.tool_name, args);
                            if rewrote_empty_bash {
                                let _ = self.event_tx.send(FromAgent::Status {
                                    message:
                                        "Received empty bash tool call; auto-filled command as \"pwd\" to proceed."
                                            .to_string(),
                                });
                            }
                            let missing =
                                self.tool_executor.missing_required(&call.tool_name, &args);
                            if !missing.is_empty() {
                                let reason = format!(
                                    "Missing required fields for tool '{}': {}",
                                    call.tool_name,
                                    missing.join(", ")
                                );
                                let _ = self.event_tx.send(deferred_tool_call_event(&call, false));
                                let _ = self
                                    .event_tx
                                    .send(deferred_rejection_output_event(&call, &reason));
                                let _ = self.event_tx.send(deferred_safety_rejection_event(
                                    &call,
                                    &reason,
                                    self.tool_executor.managed_policy_metadata(),
                                ));
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: call.call_id.clone(),
                                    content: reason,
                                    is_error: Some(true),
                                });
                                if self.cancel_remaining_deferred_if_interrupted(
                                    &mut deferred_tool_calls_iter,
                                    &mut tool_results,
                                ) {
                                    break;
                                }
                                continue;
                            }
                            call.args = args;
                            call.safe_args = self.credential_vault.vault_in_json(&call.args);
                            call.extra_context = extra_context;

                            // Earlier calls may have changed workflow state
                            // after this call's initial classification. Re-run
                            // the full firewall/approval gate against the
                            // current snapshot before allowing execution.
                            let tool_key = call.tool_name.to_lowercase();
                            if self.tool_executor.is_mcp_tool(&tool_key) {
                                let _ = self.tool_executor.ensure_mcp_annotations().await;
                            }
                            let is_external_tool = self.external_tools.contains(&tool_key);
                            let annotations = self.tool_executor.tool_annotations(&tool_key);
                            let workflow_snapshot = self.workflow_state.snapshot();
                            let firewall_verdict = deferred_firewall_verdict(
                                &self.tool_executor,
                                &tool_key,
                                &call.safe_args,
                                &workflow_snapshot,
                                annotations.as_ref(),
                                is_external_tool,
                            );
                            let deferred_policy_rejection = match &firewall_verdict {
                                NativeFirewallVerdict::Block { reason } => Some(reason.clone()),
                                NativeFirewallVerdict::RequireApproval { reason } => Some(format!(
                                    "Tool now requires approval after earlier tool execution: {reason}"
                                )),
                                NativeFirewallVerdict::Allow => match tool_requires_approval(
                                    self.config.approval_mode,
                                    is_external_tool,
                                    &firewall_verdict,
                                    &self.tool_executor,
                                    &tool_key,
                                    &call.safe_args,
                                    &self.denial_memory,
                                ) {
                                    ApprovalDecision::NotRequired => None,
                                    ApprovalDecision::Required => Some(
                                        "Tool now requires approval after earlier tool execution"
                                            .to_string(),
                                    ),
                                    ApprovalDecision::RefusedEarlierThisTurn => {
                                        Some(repeat_refusal_message(&tool_key))
                                    }
                                },
                            };
                            let deferred_requires_approval = matches!(
                                firewall_verdict,
                                NativeFirewallVerdict::RequireApproval { .. }
                            ) || tool_requires_approval(
                                self.config.approval_mode,
                                is_external_tool,
                                &firewall_verdict,
                                &self.tool_executor,
                                &tool_key,
                                &call.safe_args,
                                &self.denial_memory,
                            )
                            .requires_approval();
                            let _ = self
                                .event_tx
                                .send(deferred_tool_call_event(&call, deferred_requires_approval));
                            let mut rejected = false;
                            if let Some(reason) = deferred_policy_rejection {
                                let _ = self
                                    .event_tx
                                    .send(deferred_rejection_output_event(&call, &reason));
                                let _ = self.event_tx.send(deferred_policy_rejection_event(
                                    &call,
                                    &reason,
                                    self.tool_executor.managed_policy_metadata(),
                                ));
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: call.call_id.clone(),
                                    content: reason,
                                    is_error: Some(true),
                                });
                                rejected = true;
                            }

                            // Calls after an approval boundary were initially
                            // checked before earlier calls were recorded.
                            // Re-check against the now-current safety history
                            // so a deferred suffix cannot bypass doom-loop or
                            // rate-limit enforcement.
                            let extension_verdict = if rejected {
                                None
                            } else {
                                Some(self.plan_tool_call_through_extensions(
                                    &call.call_id,
                                    &call.tool_name,
                                    &call.safe_args,
                                ))
                            };
                            match extension_verdict {
                                None | Some(ExtensionVerdict::Proceed) => {}
                                Some(
                                    ExtensionVerdict::Block { reason }
                                    | ExtensionVerdict::Steer { message: reason },
                                ) => {
                                    let _ = self
                                        .event_tx
                                        .send(deferred_rejection_output_event(&call, &reason));
                                    let _ = self.event_tx.send(deferred_safety_rejection_event(
                                        &call,
                                        &reason,
                                        self.tool_executor.managed_policy_metadata(),
                                    ));
                                    tool_results.push(ContentBlock::ToolResult {
                                        tool_use_id: call.call_id.clone(),
                                        content: reason,
                                        is_error: Some(true),
                                    });
                                    rejected = true;
                                }
                            }
                            if !rejected {
                                let execution_args = tool_args_for_execution(&call.safe_args);
                                let result = self
                                    .execute_tool(
                                        &call.tool_name,
                                        &execution_args,
                                        &call.call_id,
                                        None,
                                    )
                                    .await;
                                let tool_name_for_cache = call.tool_name.clone();
                                let result_block = self
                                    .finalize_tool_call_result(call, true, Some(result))
                                    .await;
                                tool_results.push(result_block);
                                invalidate_cache_after_serial_tool(
                                    &self.tool_executor,
                                    &tool_name_for_cache,
                                    true,
                                );
                            }
                        }
                    }

                    // Ctrl+C during a deferred tool cancels that execution
                    // directly so its subprocess can finish cleanup. Stop the
                    // ordered suffix here; drain_pending_commands below will
                    // consume the queued Cancel and close the turn.
                    if self.take_active_operation_interruption() {
                        let cancelled_ids = cancel_deferred_suffix(
                            &self.event_tx,
                            deferred_tool_calls_iter.by_ref(),
                            &mut tool_results,
                            self.tool_executor.managed_policy_metadata(),
                        );
                        self.tool_response_coordinator
                            .discard_cancelled(&cancelled_ids);
                        break;
                    }
                }

                if deferred_steering.is_empty() {
                    if self.drain_pending_commands().await {
                        if !tool_results.is_empty() {
                            self.messages_mut().push(Message {
                                role: Role::User,
                                content: MessageContent::Blocks(std::mem::take(&mut tool_results)),
                            });
                        }
                        self.repair_orphaned_tool_calls();
                        return Err(anyhow::anyhow!("Request cancelled"));
                    }
                    deferred_steering = self.dequeue_next_turn_messages(false);
                }

                if !deferred_steering.is_empty() {
                    for (call_id, tool_name, args, _parse_error) in remaining_tool_calls {
                        let skipped_message = "Skipped due to queued user message.".to_string();
                        let _ = self.event_tx.send(FromAgent::ToolCall {
                            call_id: call_id.clone(),
                            tool: tool_name.clone(),
                            args: self.credential_vault.vault_in_json(&args),
                            requires_approval: false,
                            approval_inline_env: None,
                        });
                        let _ = self.event_tx.send(FromAgent::ToolOutput {
                            call_id: call_id.clone(),
                            content: skipped_message.clone(),
                        });
                        let _ = self.event_tx.send(FromAgent::ToolEnd {
                            call_id: call_id.clone(),
                            success: false,
                            result: Some(ToolResult::failure(skipped_message.clone())),
                            receipt: Some(
                                ToolExecution::cancelled(
                                    &call_id,
                                    &tool_name,
                                    ExecutionSource::Native,
                                    ExecutionPhase::Queued,
                                )
                                .with_managed_policy(self.tool_executor.managed_policy_metadata())
                                .receipt,
                            ),
                        });
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: call_id,
                            content: skipped_message,
                            is_error: Some(true),
                        });
                    }
                }

                // The batch is complete. Extensions see it before it becomes
                // history, with the last result as the mutable payload.
                // Reminder decisions use the unmutated outcomes so an extension
                // edit cannot hide a consecutive failure or an open todo list.
                let outcomes = self.tool_outcomes_for_batch(&tool_results);
                self.apply_tool_batch_end_extensions(&mut tool_results);
                if let Some(reminder) = reminders.observe_batch(&outcomes) {
                    append_reminder_to_last_tool_result(&mut tool_results, &reminder);
                }

                // This is the final projection boundary, after tool hooks, batch
                // extensions and reminders, including parallel read-only results.
                self.bound_final_tool_results(&mut tool_results).await;

                // Add tool results to history
                self.messages_mut().push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                });
                if self.finish_tool_batch() || self.drain_pending_commands().await {
                    self.repair_orphaned_tool_calls();
                    return Err(anyhow::anyhow!("Request cancelled"));
                }

                if !deferred_steering.is_empty() {
                    self.workflow_state.reset();
                    self.announce_next_turn_messages(&deferred_steering);
                    if self
                        .append_pending_messages_for_turn(deferred_steering)
                        .await?
                    {
                        begin_queued_user_turn(
                            &mut reminders,
                            &mut self.denial_memory,
                            step_budget,
                        );
                        continue 'turn;
                    }
                }

                // Continue the loop to process the tool results
                continue 'turn;
            }

            // No tool calls, we're done
            // Check for auto-compaction before the next turn
            if let Some(result) = prepared_compaction {
                if result.was_compacted() {
                    let split_note = if result.was_turn_split() {
                        " (turn was split)"
                    } else {
                        ""
                    };
                    eprintln!(
                        "[agent] Auto-compacted {} messages{}",
                        result.compacted_count, split_note
                    );

                    // Notify the UI about auto-compaction
                    let status_msg = if let Some(ref cut_point) = result.cut_point {
                        format!(
                            "Auto-compacted: {} messages summarized (~{} → ~{} tokens){}",
                            result.compacted_count,
                            cut_point.tokens_before,
                            cut_point.tokens_after,
                            split_note
                        )
                    } else {
                        format!(
                            "Auto-compacted: {} messages summarized",
                            result.compacted_count
                        )
                    };
                    emit_compaction_event(
                        &self.event_tx,
                        &self.messages,
                        result.summary.as_deref().unwrap_or(&status_msg),
                        result.cut_point.as_ref(),
                        result.continuation.as_ref(),
                        true,
                    );
                    let _ = self.event_tx.send(FromAgent::Status {
                        message: status_msg,
                    });
                    // Adopt provenance only with its compacted message history. A
                    // cancelled preparation must not become the next merge's input.
                    if let Some(record) = result.continuation {
                        self.semantic_continuation = Some(record);
                    }
                    self.messages = Arc::new(result.messages);
                    self.prepare_compacted_checkpoint(config)?;
                    self.emit_conversation_snapshot();
                }
            }

            if self.drain_pending_commands().await {
                return Err(anyhow::anyhow!("Request cancelled"));
            }

            self.run_queued_side_questions().await;

            let mut next_turn_messages = self.dequeue_next_turn_messages(true);
            while !next_turn_messages.is_empty() {
                self.workflow_state.reset();
                self.announce_next_turn_messages(&next_turn_messages);
                if self
                    .append_pending_messages_for_turn(next_turn_messages)
                    .await?
                {
                    begin_queued_user_turn(&mut reminders, &mut self.denial_memory, step_budget);
                    continue 'turn;
                }
                next_turn_messages = self.dequeue_next_turn_messages(true);
            }

            break;
        }

        Ok(())
    }
}
