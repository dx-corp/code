//! Auxiliary questions on the active native or Codex session.

use super::*;

impl NativeAgentRunner {
    pub(super) async fn run_side_question(&mut self, question: String, standalone: bool) {
        let question = self.credential_vault.vault_in_text(&question);
        let side_id = Uuid::new_v4().to_string();
        let _ = self.event_tx.send(FromAgent::SideQuestionStart {
            side_id: side_id.clone(),
            question: question.clone(),
            standalone,
        });

        let mut answer = String::new();
        let mut usage = TokenUsage::default();
        let mut saw_usage = false;
        let shutdown_token = self.shutdown_token.clone();
        let credential_vault = self.credential_vault.clone();
        let result = await_side_question_or_shutdown(&shutdown_token, async {
            if self.model_route.uses_app_server() {
                return self
                    .run_codex_side_question(
                        &question,
                        &side_id,
                        &mut answer,
                        &mut usage,
                        &mut saw_usage,
                    )
                    .await;
            }

            let mut messages = vault_provider_history(&self.messages, &credential_vault)?;
            messages.push(Message {
                role: Role::User,
                content: MessageContent::text(question.clone()),
            });
            let config = self.build_config(&messages, false).await?;
            let prepared_request = ProviderSafeRequest::prepare(
                &Arc::new(messages),
                config,
                &credential_vault,
            )?;
            let messages = prepared_request.messages.as_ref();
            let config = &prepared_request.config;
            let _ = self.event_tx.send(FromAgent::RequestContextPrepared { response_id: side_id.clone() });
            let _ = self.event_tx.send(FromAgent::OperationObservation {
                observation: maestro_runtime_contracts::operation_observation::OperationObservation::Prepared {
                    response_id: side_id.clone(), model_id: config.model.clone(), model_provider: self.client.as_ref().map(|client| client.provider_name()).unwrap_or("unknown").to_owned(),
                    message_count: messages.len().try_into().unwrap_or(u32::MAX),
                    input_size_bytes: serde_json::to_vec(&messages).ok().map(|v| v.len() as u64),
                },
            });
            let client = self
                .client
                .as_ref()
                .context("direct provider client missing for side question")?;
            let request_id = provider_request_id("side_question", &config.model, messages)?;
            self.admit_provider_request("side_question", &request_id, Some(&config.model))
                .await?;
            prepared_request.ensure_current(&credential_vault)?;
            let mut rx = client
                .stream_owned_config(messages, config.clone())
                .await?;

            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::ManagedGatewayReceipt(receipt) => {
                        let _ = self.event_tx.send(FromAgent::OperationObservation {
                            observation: maestro_runtime_contracts::operation_observation::OperationObservation::GatewayReceipt {
                                response_id: side_id.clone(), request_id: receipt.request_id.clone(),
                                record_id: receipt.record_id.clone(), lineage_id: receipt.lineage_id.clone(),
                                provider_tools_sha256: None, provider_tool_count: None,
                            },
                        });
                        let _ = self
                            .event_tx
                            .send(Self::managed_gateway_receipt_event(receipt, true));
                    }
                    StreamEvent::ContentBlockStart {
                        block: ContentBlock::Text { text },
                        ..
                    } if !text.is_empty() => {
                        answer.push_str(&text);
                        let _ = self.event_tx.send(FromAgent::SideQuestionChunk {
                            side_id: side_id.clone(),
                            content: text,
                        });
                    }
                    StreamEvent::TextDelta { text, .. } => {
                        answer.push_str(&text);
                        let _ = self.event_tx.send(FromAgent::SideQuestionChunk {
                            side_id: side_id.clone(),
                            content: text,
                        });
                    }
                    StreamEvent::ReasoningUsage { tokens } => {
                        let _ = self.event_tx.send(FromAgent::OperationObservation {
                            observation: maestro_runtime_contracts::operation_observation::OperationObservation::ReasoningUsage { response_id: side_id.clone(), tokens },
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
                    StreamEvent::MessageStop { .. } => return Ok(()),
                    StreamEvent::Error { message } => return Err(anyhow::anyhow!(message)),
                    StreamEvent::ProviderError { kind, message } => {
                        return Err(anyhow::Error::new(ProviderStreamFailure { kind, message }));
                    }
                    _ => {}
                }
            }
            Err(anyhow::Error::new(ProviderStreamFailure {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message: "side-question provider stream ended before a terminal event".to_string(),
            }))
        })
        .await
        .unwrap_or_else(|| Err(anyhow::anyhow!("Side question cancelled during shutdown")));

        let provider_error_kind = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ProviderStreamFailure>())
            .map(|error| error.kind);

        let _ = self.event_tx.send(FromAgent::SideQuestionEnd {
            side_id,
            question,
            answer,
            standalone,
            error: result.err().map(|err| err.to_string()),
            provider_error_kind,
            usage: saw_usage.then_some(usage),
        });
    }
    /// Run a Codex-native side question in an isolated, tool-free app-server
    /// thread. Side questions must not mutate the live thread or fall back to
    /// a direct HTTP client that would require copying ChatGPT credentials.
    pub(super) async fn run_codex_side_question(
        &mut self,
        question: &str,
        side_id: &str,
        answer: &mut String,
        usage: &mut TokenUsage,
        saw_usage: &mut bool,
    ) -> Result<()> {
        let model = crate::agent::codex_app_server_turns::codex_thread_model_id(&self.config.model);
        let started = Instant::now();
        let span = crate::model_span("openai-codex", &model);
        let result = self
            .run_codex_side_question_inner(question, side_id, answer, usage, saw_usage)
            .instrument(span.clone())
            .await;
        if *saw_usage {
            record_model_usage(
                &span,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
            );
        }
        record_outcome(
            &span,
            if result.is_ok() { "success" } else { "error" },
            started.elapsed(),
            result.is_err().then_some("provider_error"),
        );
        result
    }
    pub(super) async fn run_codex_side_question_inner(
        &mut self,
        question: &str,
        side_id: &str,
        answer: &mut String,
        usage: &mut TokenUsage,
        saw_usage: &mut bool,
    ) -> Result<()> {
        use crate::agent::codex_app_server_turns::TurnWaitEvent;

        let safe_messages = vault_provider_history(&self.messages, &self.credential_vault)?;
        let side_question_compactor = crate::agent::compaction::ContextCompactor::new(
            crate::agent::compaction::CompactionConfig {
                max_context_tokens: CODEX_SIDE_QUESTION_MAX_CONTEXT_TOKENS,
                keep_recent_tokens: CODEX_SIDE_QUESTION_MAX_CONTEXT_TOKENS / 2,
                // Count with the active model's tokenizer, like every other
                // compaction path, so this fixed budget means the same thing
                // here as it does on the main turn loop.
                model: Some(self.config.model.clone()),
                ..Default::default()
            },
        );
        let restored_messages = side_question_compactor
            .compact_with_tokens(&safe_messages)
            .messages;
        let instructions = runtime_system_prompt(
            self.config.system_prompt.as_deref(),
            self.prompt_context.as_deref(),
            &self.config.model,
            self.tool_executor.model_capabilities(&self.config.model),
        )
        .map(|text| self.credential_vault.vault_in_text(&text));
        let prepared_request = ProviderSafeRequest::prepare(
            &Arc::new(restored_messages),
            RequestConfig {
                system: instructions,
                ..RequestConfig::default()
            },
            &self.credential_vault,
        )?;
        let instructions = prepared_request.config.system.clone();
        let restored_messages = prepared_request.messages.as_ref();
        let question = self.credential_vault.vault_in_text(question);
        let vault_generation = self
            .credential_vault
            .attest_provider_text(&question)
            .map_err(anyhow::Error::msg)?;
        let auth = self
            .tool_executor
            .codex_auth_context()
            .map_err(anyhow::Error::msg)?;
        prepared_request.ensure_current(&self.credential_vault)?;
        let session =
            crate::agent::codex_app_server_turns::CodexAppServerTurnSession::connect_with_auth(
                crate::agent::codex_app_server_turns::codex_thread_model_id(&self.config.model),
                Some(self.config.cwd.clone()),
                Some("untrusted".to_owned()),
                Some("read-only".to_owned()),
                crate::agent::codex_app_server_turns::CodexThreadPayload {
                    dynamic_tools: &[],
                    instructions,
                    restored_messages,
                },
                &auth,
            )
            .await
            .context("start Codex app-server side-question session")?;
        anyhow::ensure!(
            self.credential_vault.has_generation(vault_generation),
            "credential vault changed before Codex side-question dispatch"
        );
        let turn_id = session
            .start_text_turn_with_thinking(
                question,
                self.config.thinking_enabled,
                self.config.thinking_budget,
                None,
            )
            .await?;

        let turn_result = tokio::time::timeout(CODEX_SIDE_QUESTION_TIMEOUT, async {
            loop {
                match session
                    .wait_server_request_or_turn_complete(&turn_id, Some(250))
                    .await?
                {
                    TurnWaitEvent::Pending => {}
                    TurnWaitEvent::ServerRequest(request) => {
                        request.reject("Codex side questions do not execute tools");
                    }
                    TurnWaitEvent::Completed(result) => {
                        if let Some(failure) = result.provider_failure() {
                            return Err(anyhow::anyhow!(failure.to_owned()));
                        }
                        if !result.assistant_text.is_empty() {
                            answer.push_str(&result.assistant_text);
                            let _ = self.event_tx.send(FromAgent::SideQuestionChunk {
                                side_id: side_id.to_owned(),
                                content: result.assistant_text,
                            });
                        }
                        let usage_notifications = session
                            .take_usage_notifications_for_turn(&result.turn_id)
                            .await;
                        if let Some(completion_usage) =
                            choose_codex_turn_usage(&result.raw_completion, &usage_notifications)
                        {
                            *usage = completion_usage;
                            *saw_usage = true;
                            let _ = self.event_tx.send(FromAgent::CodexUsageState {
                                source: "exact".to_owned(),
                                usage: Some(usage.clone()),
                            });
                        }
                        return Ok(());
                    }
                }
            }
        })
        .await;
        match turn_result {
            Ok(result) => result,
            Err(_) => {
                let _ = session.interrupt_turn(&turn_id, Some(1_500)).await;
                Err(anyhow::anyhow!(
                    "Codex side question timed out after {} seconds",
                    CODEX_SIDE_QUESTION_TIMEOUT.as_secs(),
                ))
            }
        }
    }
    pub(super) async fn run_queued_side_questions(&mut self) {
        loop {
            let pending =
                self.drain_leading_pending_messages(PromptKind::SideQuestion, QueueMode::One);
            let Some(pending) = pending.into_iter().next() else {
                return;
            };
            self.announce_next_turn_messages(std::slice::from_ref(&pending));
            self.run_side_question(pending.content, false).await;
        }
    }
}
