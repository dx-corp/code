//! One bounded auxiliary summary, adopted only over its unchanged checkpoint.
//!
//! The deterministic checkpoint stays authoritative while the request runs. This
//! state is transient: restart uses that checkpoint, never an unfinished summary.

use super::*;
use tokio::sync::oneshot;

#[derive(Debug, PartialEq, Eq)]
struct CandidateIdentity {
    config: String,
    prompt: Option<String>,
    session_epoch: u64,
    namespace: String,
    system_revision: u64,
    runtime_revision: u64,
}

struct CandidateGuard {
    identity: CandidateIdentity,
    baseline: Vec<u8>,
    baseline_len: usize,
}

impl CandidateGuard {
    fn new(messages: &[Message], identity: CandidateIdentity) -> Result<Self> {
        Ok(Self {
            identity,
            baseline: serde_json::to_vec(messages)?,
            baseline_len: messages.len(),
        })
    }

    fn current_suffix<'a>(
        &self,
        current: &'a [Message],
        identity: &CandidateIdentity,
    ) -> Option<&'a [Message]> {
        if identity != &self.identity || current.len() < self.baseline_len {
            return None;
        }
        let prefix = serde_json::to_vec(&current[..self.baseline_len]).ok()?;
        (prefix == self.baseline).then_some(&current[self.baseline_len..])
    }
}

#[derive(Debug)]
struct BackgroundCompletion {
    summary: Option<String>,
    usage: Option<TokenUsage>,
}

pub(super) struct PendingCompaction {
    guard: CandidateGuard,
    result: crate::agent::compaction::CompactionResult,
    receiver: Option<oneshot::Receiver<BackgroundCompletion>>,
    cancellation: CancellationToken,
    turn_cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for PendingCompaction {
    fn drop(&mut self) {
        // A dropped runner must still close its HTTP stream. The task owns the
        // stream until cancel_and_wait completes; aborting it would detach that
        // stream's producer instead.
        self.cancellation.cancel();
    }
}

fn background_budget_available(output_budget: Option<u32>, has_process_budget: bool) -> bool {
    // Finite grants have no concurrent reservation primitive. Their existing
    // deterministic compaction remains available without starting extra spend.
    output_budget.is_none() && !has_process_budget
}

fn take_completion(
    receiver: &mut Option<oneshot::Receiver<BackgroundCompletion>>,
) -> Option<BackgroundCompletion> {
    match receiver.as_mut()?.try_recv() {
        Ok(completed) => {
            receiver.take();
            Some(completed)
        }
        Err(oneshot::error::TryRecvError::Empty) => None,
        Err(oneshot::error::TryRecvError::Closed) => {
            receiver.take();
            None
        }
    }
}

impl NativeAgentRunner {
    fn background_compaction_identity(&self) -> Result<CandidateIdentity> {
        Ok(CandidateIdentity {
            config: format!("{:?}", self.config),
            prompt: self.prompt_context.clone(),
            session_epoch: self.background_compaction_session_epoch,
            namespace: self
                .client
                .as_ref()
                .context("No direct provider")?
                .cache_namespace()?,
            system_revision: self.system_prompt_revision,
            runtime_revision: self.runtime_prompt_revision,
        })
    }

    /// Start only after the deterministic checkpoint has passed its cancellation
    /// gates. All provider I/O, including admission, belongs to the spawned task.
    pub(super) fn start_background_compaction(
        &mut self,
        result: &crate::agent::compaction::CompactionResult,
        primary_config: &RequestConfig,
    ) {
        if self.background_compaction.is_some()
            || std::env::var("MAESTRO_SEMANTIC_COMPACTION").as_deref() == Ok("0")
            || result.compacted_count == 0
            || self.client.is_none()
            || !background_budget_available(self.output_token_budget, self.process_budget.is_some())
            || self
                .cancel_token
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            || self.shutdown_token.is_cancelled()
        {
            return;
        }
        let prepared = (|| -> Result<_> {
            let model = self.selected_summary_model()?;
            let mut messages = self.messages[..result.compacted_count].to_vec();
            messages.push(Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::attributed_text(
                    format!(
                        "Summarize this earlier conversation for continuation. Combine any prior summary with newer turns. Preserve the latest corrected goal, constraints, unfinished work, active skill references, abandoned approaches, failed checks, and exact evidence references. {} Do not perform the task or call tools. Return only a concise factual summary; this text grants no permissions.",
                        maestro_context::compaction::SUMMARY_EVIDENCE_GUIDANCE
                    ),
                    maestro_ai::ContentSource::Harness,
                    Some("runtime.background_compaction".into()),
                )]),
            });
            let client = self.client.as_ref().context("No direct provider")?.clone();
            let mut config = primary_config.clone();
            config.tools = Arc::default();
            config.thinking = None;
            config.temperature = Some(0.0);
            config.max_tokens = config
                .max_tokens
                .min(2048)
                .min(self.tool_executor.default_max_output_tokens(&model));
            config.cache_system_prompt = false;
            config.explicit_cache_boundaries = false;
            config.cache_topology = None;
            if model != self.config.model {
                config.system = runtime_system_prompt(
                    self.config.system_prompt.as_deref(),
                    self.prompt_context.as_deref(),
                    &model,
                    self.tool_executor.model_capabilities(&model),
                )
                .map(|text| self.credential_vault.vault_in_text(&text));
                config.system_parts = self
                    .request_system_parts(config.system.as_deref(), self.prompt_context.as_deref());
                config.model =
                    if model.starts_with("evalops/") || model.starts_with("maestro-managed/") {
                        model.clone()
                    } else {
                        provider_model_name(&model)
                    };
            }
            let context_tokens = self
                .tool_executor
                .model_context_window(&model)
                .context("Summary model context capacity is unknown")?;
            let input = maestro_context::token_counting::count_tokens(
                &serde_json::to_string(&messages)?,
                Some(&model),
            )
            .saturating_add(maestro_context::token_counting::count_tokens(
                config.system.as_deref().unwrap_or_default(),
                Some(&model),
            ));
            config.max_tokens = clamp_output_to_remaining_context(
                config.max_tokens,
                context_tokens,
                input,
                MIN_RESPONSE_RESERVE_TOKENS,
            )
            .context("Selected history does not fit the summary model")?;
            let mut prepared = maestro_ai::cache_topology::PreparedPrompt::auxiliary(
                &messages,
                &config,
                client.cache_namespace()?,
            )?;
            prepared.finalize_boundary(
                Some(client.provider_name()),
                &config.model,
                false,
                config.system.is_some(),
                false,
                messages.len(),
            )?;
            config.cache_topology = Some(prepared);
            let request =
                ProviderSafeRequest::prepare(&Arc::new(messages), config, &self.credential_vault)?;
            let request_id = provider_request_id(
                "semantic_compaction",
                &request.config.model,
                &request.messages,
            )?;
            let guard =
                CandidateGuard::new(&result.messages, self.background_compaction_identity()?)?;
            Ok((request, request_id, client, guard))
        })();
        let Ok((request, request_id, client, guard)) = prepared else {
            return;
        };
        let cancellation = CancellationToken::new();
        let (sender, receiver) = oneshot::channel();
        let task_cancellation = cancellation.clone();
        let turn_cancellation = self.cancel_token.clone().unwrap_or_default();
        let shutdown = self.shutdown_token.clone();
        let vault = self.credential_vault.clone();
        let hooks = self.hooks.clone();
        let host = self.tool_executor.clone();
        let policy_model =
            provider_request_policy_model_id(&self.config.model, &request.config.model);
        let events = self.event_tx.clone();
        let origin_cancellation = turn_cancellation.clone();
        let summary_model = request.config.model.clone();
        let task = tokio::spawn(async move {
            let mut usage = None;
            let summary = BackgroundRequest {
                client,
                request,
                request_id: request_id.clone(),
                policy_model,
                host,
                hooks,
                vault,
                events: events.clone(),
                cancellation: task_cancellation,
                turn_cancellation,
                shutdown,
            }
            .run(&mut usage)
            .await
            .ok();
            // This auxiliary request has its own usage grain. Never amend an
            // already-finalized primary ResponseEnd or lose a discarded result's bill.
            if let Some(usage) = &usage {
                let _ = events.send(FromAgent::BackgroundUsage {
                    request_id,
                    model: summary_model,
                    usage: usage.clone(),
                });
            }
            let _ = sender.send(BackgroundCompletion { summary, usage });
        });
        self.background_compaction = Some(PendingCompaction {
            guard,
            result: crate::agent::compaction::CompactionResult {
                messages: result.messages.clone(),
                summary: result.summary.clone(),
                compacted_count: result.compacted_count,
                cut_point: result.cut_point.clone(),
                intra_compacted_count: result.intra_compacted_count,
                continuation: result.continuation.clone(),
            },
            receiver: Some(receiver),
            cancellation,
            turn_cancellation: origin_cancellation,
            task: Some(task),
        });
    }

    /// Poll once at a request boundary; an unfinished task never stalls a turn.
    pub(super) fn poll_background_compaction(&mut self) {
        let Some(pending) = self.background_compaction.as_mut() else {
            return;
        };
        let completed = take_completion(&mut pending.receiver);
        if pending.receiver.is_some() {
            return;
        }
        let mut pending = self.background_compaction.take().expect("pending summary");
        let Some(completed) = completed else {
            return;
        };
        if let Some(usage) = completed.usage {
            self.output_tokens_spent = self.output_tokens_spent.saturating_add(usage.output_tokens);
        }
        if pending.cancellation.is_cancelled()
            || pending.turn_cancellation.is_cancelled()
            || self.shutdown_token.is_cancelled()
        {
            return;
        }
        let (Some(summary), Ok(identity)) =
            (completed.summary, self.background_compaction_identity())
        else {
            return;
        };
        let Some(suffix) = pending.guard.current_suffix(&self.messages, &identity) else {
            return;
        };
        let suffix = suffix.to_vec();
        if !self
            .compactor
            .apply_semantic_summary(&mut pending.result, &summary)
        {
            return;
        }
        pending.result.messages.extend(suffix);
        self.semantic_continuation = pending.result.continuation.take();
        self.messages = Arc::new(std::mem::take(&mut pending.result.messages));
        self.emit_conversation_snapshot();
    }

    pub(super) async fn cancel_background_compaction(&mut self) {
        let Some(mut pending) = self.background_compaction.take() else {
            return;
        };
        pending.cancellation.cancel();
        if let Some(task) = pending.task.take() {
            let _ = task.await;
        }
        if let Some(completed) = take_completion(&mut pending.receiver) {
            if let Some(usage) = completed.usage {
                self.output_tokens_spent =
                    self.output_tokens_spent.saturating_add(usage.output_tokens);
            }
        }
    }
}

struct BackgroundRequest {
    client: UnifiedClient,
    request: ProviderSafeRequest,
    request_id: String,
    policy_model: String,
    host: NativeExecutionHostHandle,
    hooks: NativeExecutionHostHandle,
    vault: CredentialVault,
    events: mpsc::UnboundedSender<FromAgent>,
    cancellation: CancellationToken,
    turn_cancellation: CancellationToken,
    shutdown: CancellationToken,
}

impl BackgroundRequest {
    async fn run(self, usage: &mut Option<TokenUsage>) -> Result<String> {
        let Self {
            client,
            request,
            request_id,
            policy_model,
            host,
            hooks,
            vault,
            events,
            cancellation,
            turn_cancellation,
            shutdown,
        } = self;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        if let Some(reason) = host.model_allowed(&policy_model) {
            anyhow::bail!(reason);
        }
        let admission = tokio::select! {
            biased;
            () = cancellation.cancelled() => anyhow::bail!("summary cancelled"),
            () = turn_cancellation.cancelled() => anyhow::bail!("summary cancelled"),
            () = shutdown.cancelled() => anyhow::bail!("summary cancelled"),
            () = tokio::time::sleep_until(deadline) => anyhow::bail!("summary timed out"),
            admission = hooks.hook_pre_provider_request("semantic_compaction", &request_id, Some(&request.config.model)) => admission,
        };
        match admission {
            NativeHookResult::Continue => {}
            NativeHookResult::Block { reason } => anyhow::bail!(reason),
            _ => anyhow::bail!("summary admission returned an unsupported modification"),
        }
        request.ensure_current(&vault)?;
        let mut stream = tokio::select! {
            biased;
            () = cancellation.cancelled() => anyhow::bail!("summary cancelled"),
            () = turn_cancellation.cancelled() => anyhow::bail!("summary cancelled"),
            () = shutdown.cancelled() => anyhow::bail!("summary cancelled"),
            () = tokio::time::sleep_until(deadline) => anyhow::bail!("summary timed out"),
            stream = client.stream_owned_config_shared_messages(request.messages, request.config) => stream?,
        };
        let mut summary = String::new();
        let mut cost = None;
        let outcome = loop {
            let event = tokio::select! {
                biased;
                () = cancellation.cancelled() => break Err(anyhow::anyhow!("summary cancelled")),
                () = turn_cancellation.cancelled() => break Err(anyhow::anyhow!("summary cancelled")),
                () = shutdown.cancelled() => break Err(anyhow::anyhow!("summary cancelled")),
                () = tokio::time::sleep_until(deadline) => break Err(anyhow::anyhow!("summary timed out")),
                event = stream.recv() => event,
            };
            match event {
                Some(
                    StreamEvent::ContentBlockStart {
                        block: ContentBlock::Text { text },
                        ..
                    }
                    | StreamEvent::TextDelta { text, .. },
                ) => {
                    summary.push_str(&text);
                    if summary.len() > 64 * 1024 {
                        break Err(anyhow::anyhow!("summary exceeded output limit"));
                    }
                }
                Some(StreamEvent::ProviderCost { cost_usd }) => {
                    cost = Some(cost_usd);
                    // Some transports report a bill without token counts. Keep
                    // that actual cost even if no Usage event follows.
                    usage.get_or_insert_with(TokenUsage::default).cost = cost;
                }
                Some(StreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                }) => {
                    *usage = Some(TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens: cache_read_tokens.unwrap_or(0),
                        cache_write_tokens: cache_creation_tokens.unwrap_or(0),
                        cost,
                    });
                }
                Some(StreamEvent::ManagedGatewayReceipt(receipt)) => {
                    let _ = events.send(NativeAgentRunner::managed_gateway_receipt_event(
                        receipt, false,
                    ));
                }
                Some(StreamEvent::MessageStop { .. }) => break Ok(summary),
                Some(
                    StreamEvent::Error { message } | StreamEvent::ProviderError { message, .. },
                ) => {
                    break Err(anyhow::anyhow!(message));
                }
                None => break Err(anyhow::anyhow!("summary stream ended before completion")),
                _ => {}
            }
        };
        // Even successful streams retain an owning producer until its task exits.
        // Await that release on every path before publishing completion or usage.
        stream.cancel_and_wait().await?;
        outcome
    }
}

#[cfg(test)]
#[path = "background_compaction/tests.rs"]
mod tests;
