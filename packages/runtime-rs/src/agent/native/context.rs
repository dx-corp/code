//! Request context, compaction, and continuation orchestration.

use super::*;

impl NativeAgentRunner {
    /// Interrupted streams do not reach post-response compaction. Bound their
    /// accumulated history at the interruption boundary, preserving the
    /// exact original requests in the same atomic checkpoint as the summary.
    pub(super) fn compact_interrupted_native_history(&mut self) {
        // Accepted notes must reach a provider verbatim before they can be summarized.
        if !self.pending_user_note_texts.is_empty()
            || self.model_route.uses_app_server()
            || !self.compactor.should_auto_compact(&self.messages)
        {
            return;
        }
        self.repair_orphaned_tool_calls();
        let started = Instant::now();
        let mut result = self.compactor.compact_with_tokens(&self.messages);
        if !result.was_compacted() {
            return;
        }
        self.prepare_continuation(&mut result);
        let _ = self.event_tx.send(FromAgent::CompactionMeasured {
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        });
        emit_compaction_event(
            &self.event_tx,
            &self.messages,
            result
                .summary
                .as_deref()
                .unwrap_or("Interrupted history compacted"),
            result.cut_point.as_ref(),
            result.continuation.as_ref(),
            true,
        );
        if let Some(record) = result.continuation {
            self.semantic_continuation = Some(record);
        }
        self.messages = Arc::new(result.messages);
        self.emit_conversation_snapshot();
    }

    /// Apply a session transition to the hook system.
    ///
    /// Dispatches `SessionEnd` for the session being left and `SessionStart`
    /// for the one being entered, and stamps the new id onto every subsequent
    /// hook payload. The end fires before the id changes so its payload names
    /// the session that actually ended.
    ///
    /// Both events are advisory: their results are logged by the hook system
    /// and cannot block a session transition the user has already made.
    pub(super) async fn apply_session_context(
        &mut self,
        session_id: Option<String>,
        transcript_path: Option<String>,
        reason: &str,
        owns_persistent_tool_spills: bool,
        preserve_compacted_checkpoint: bool,
    ) {
        self.owns_persistent_tool_spills = owns_persistent_tool_spills && session_id.is_some();
        let previous_session = self.hooks.hook_session_id().await;
        if previous_session == session_id {
            self.hooks
                .hook_set_session_context(session_id, transcript_path)
                .await;
            return;
        }
        if previous_session.is_some() {
            let mut audit = self
                .runtime_audit
                .write()
                .unwrap_or_else(|p| p.into_inner());
            let retain_checkpoint = preserve_compacted_checkpoint
                && session_id.is_some()
                && audit.request_cache.as_ref().is_some_and(|snapshot| {
                    snapshot.cache_topology.as_ref().is_some_and(|topology| {
                        topology.transition
                            == maestro_ai::cache_topology::CacheTransition::HistoryRewritten
                    })
                });
            if !retain_checkpoint {
                audit.request_cache = None;
                audit.cache_reuse = None;
            }
            if let Some(record) = &mut self.semantic_continuation {
                record.tool_outputs.clear();
            }
        }
        self.runtime_audit
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .excluded_context_tools
            .clear();
        self.model_tool_cache = None;
        self.refresh_runtime_audit();
        // A live app-server thread is bound to the prior explicit session.
        // Drop it before changing identity so the next turn resolves the
        // session-aware persistent binding instead of reusing that thread.
        self.codex_session = None;
        self.codex_correlations.reset();
        self.tool_executor.reset_coding_turn();
        if self.hooks.hook_session_id().await.is_some() {
            let _ = self.hooks.hook_on_session_end(reason).await;
        }
        self.hooks
            .hook_set_session_context(session_id.clone(), transcript_path)
            .await;
        if session_id.is_some() {
            let _ = self.hooks.hook_on_session_start(reason).await;
        }
    }
    /// Output allowance for the request this runner is about to build.
    ///
    /// Without a cumulative budget this is the configured per-request
    /// `max_tokens`. With one, the request is additionally clamped to the part
    /// of the budget the run has not spent, so a run that calls tools cannot be
    /// granted the full allowance again on every request.
    ///
    /// The floor of 1 keeps the request valid for providers that reject
    /// `max_tokens: 0`. A run that has reached its budget is stopped by the
    /// caller that set it; this function does not end turns.
    pub(super) fn remaining_output_token_allowance(&self) -> u32 {
        output_token_allowance(
            self.config.max_tokens,
            self.output_token_budget,
            self.output_tokens_spent,
        )
    }
    /// Build request configuration
    pub(super) async fn build_config(
        &mut self,
        request_messages: &[Message],
        include_tools: bool,
    ) -> Result<RequestConfig> {
        self.build_config_with_usage(request_messages, include_tools)
            .await
            .map(|(config, _)| config)
    }

    pub(super) async fn build_config_with_usage(
        &mut self,
        request_messages: &[Message],
        include_tools: bool,
    ) -> Result<(RequestConfig, crate::agent::RequestContextUsage)> {
        if let Some(state) = self.process_budget.as_ref() {
            if !include_tools {
                anyhow::bail!("process grants do not admit auxiliary model requests");
            }
            state
                .lock()
                .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                .admit_request()
                .map_err(anyhow::Error::msg)?;
        }
        if include_tools {
            self.apply_requested_boost().await?;
        }
        // These values are cached for the runner lifetime and updated through
        // explicit commands when app-owned goal state changes.
        let goal_tools_visible = self.goal_tools_visible;
        let include_ide_tools = self.include_ide_tools;
        let cached_tools = self
            .model_tool_cache
            .as_ref()
            .filter(|cache| {
                cache.goal_tools_visible == goal_tools_visible
                    && cache.include_ide_tools == include_ide_tools
                    && cache.active_tool_names == self.active_tool_names
            })
            .map(|cache| Arc::clone(&cache.tools));
        let tools = if !include_tools {
            Arc::new(Vec::new())
        } else if let Some(tools) = cached_tools {
            tools
        } else {
            let definitions = effective_tool_definitions(
                &self.tools,
                &self.active_tool_names,
                &self.external_tools,
                goal_tools_visible,
                include_ide_tools,
            );
            let tools = Arc::new(
                definitions
                    .into_iter()
                    .map(|definition| definition.tool)
                    .collect(),
            );
            self.model_tool_cache = Some(ModelToolCache {
                goal_tools_visible,
                include_ide_tools,
                active_tool_names: self.active_tool_names.clone(),
                tools: Arc::clone(&tools),
            });
            tools
        };
        let excluded = self
            .runtime_audit
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .excluded_context_tools
            .clone();
        let tools = if excluded.is_empty() {
            tools
        } else {
            Arc::new(
                tools
                    .iter()
                    .filter(|tool| !excluded.contains(&tool.name.to_ascii_lowercase()))
                    .cloned()
                    .collect(),
            )
        };
        self.tool_executor.set_subagent_parent_model(
            self.config.model.clone(),
            crate::agent::model_dynamics::thinking_level(
                self.config.thinking_enabled,
                self.config.thinking_budget,
            )
            .label()
            .to_owned(),
        );
        let thinking = if self.config.thinking_enabled {
            Some(ThinkingConfig::enabled(self.config.thinking_budget))
        } else {
            None
        };

        let system = runtime_system_prompt(
            self.config.system_prompt.as_deref(),
            if include_tools {
                None
            } else {
                self.prompt_context.as_deref()
            },
            &self.config.model,
            self.tool_executor.model_capabilities(&self.config.model),
        );
        self.refresh_runtime_audit_with_prompt(system.clone());

        let configured_model = self.config.model.trim();
        let model = if ["evalops/", "maestro-managed/"].iter().any(|prefix| {
            configured_model
                .get(..prefix.len())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        }) {
            // Preserve the managed namespace for telemetry and let the
            // managed OpenAI boundary strip it immediately before dispatch.
            configured_model.to_string()
        } else {
            provider_model_name(configured_model)
        };

        let mut max_tokens = self.remaining_output_token_allowance();
        if let Some(context_tokens) = self
            .tool_executor
            .is_local_model(&self.config.model)
            .then(|| self.tool_executor.model_context_window(&self.config.model))
            .flatten()
            .filter(|tokens| *tokens > 0)
        {
            let estimated_input_tokens = self
                .compactor
                .estimate_tokens(request_messages)
                .saturating_add(
                    system
                        .as_deref()
                        .map_or(0, maestro_context::token_estimation::estimate_tokens),
                )
                .saturating_add(
                    maestro_context::token_estimation::estimate_tokens_from_json(tools.as_ref()),
                )
                .saturating_add(if include_tools {
                    self.prompt_context
                        .as_deref()
                        .map_or(0, maestro_context::token_estimation::estimate_tokens)
                } else {
                    0
                });
            max_tokens = clamp_output_to_remaining_context(
                max_tokens,
                context_tokens,
                estimated_input_tokens,
            )
            .with_context(|| {
                format!(
                    "Local model request input estimate ({estimated_input_tokens} tokens) fills the live {context_tokens}-token context; reduce the prompt/history/tools or increase the runtime context"
                )
            })?;
        }

        let cache_system_prompt = self.client.as_ref().is_some_and(|client| {
            maestro_ai::supports_explicit_prompt_caching(client.provider(), &model)
        });
        let mut config = RequestConfig {
            model,
            max_tokens,
            temperature: if self.config.thinking_enabled {
                None // Temperature must be 1 or omitted for thinking
            } else {
                Some(0.7)
            },
            system,
            tools,
            thinking,
            cache_topology: None,
            cache_system_prompt,
        };
        let mut audit = self
            .runtime_audit
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if include_tools {
            let namespace = self
                .client
                .as_ref()
                .map(|client| client.cache_namespace())
                .transpose()?
                .unwrap_or_else(|| "local".into());
            let previous = audit
                .request_cache
                .as_ref()
                .and_then(|snapshot| snapshot.cache_topology.as_ref());
            config.cache_topology = Some(
                maestro_ai::cache_topology::PreparedPrompt::prepare(
                    request_messages,
                    &config,
                    namespace,
                    previous,
                )?
                .with_volatile_tail(self.prompt_context.clone()),
            );
        }
        let snapshot = maestro_context::token_counting::RequestCacheSnapshot::from_request(
            &config,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        if include_tools {
            audit.cache_reuse = audit
                .request_cache
                .as_ref()
                .map(|previous| snapshot.compare(previous));
            audit.request_cache = Some(snapshot);
        }
        let usage = crate::agent::RequestContextUsage::from_request(
            request_messages,
            &config,
            self.compactor.counter(),
        );
        audit.request_context = Some(usage.clone());
        Ok((config, usage))
    }
    pub(super) fn prepare_compacted_checkpoint(
        &mut self,
        previous_config: &RequestConfig,
    ) -> Result<()> {
        let messages = resolve_provider_history_shared(&self.messages, &self.credential_vault)?;
        let mut config = previous_config.clone();
        let namespace = self
            .client
            .as_ref()
            .map(|client| client.cache_namespace())
            .transpose()?
            .unwrap_or_else(|| "local".into());
        let mut audit = self
            .runtime_audit
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let previous = audit
            .request_cache
            .as_ref()
            .and_then(|snapshot| snapshot.cache_topology.as_ref());
        config.cache_topology = Some(
            maestro_ai::cache_topology::PreparedPrompt::prepare(
                &messages, &config, namespace, previous,
            )?
            .with_volatile_tail(self.prompt_context.clone()),
        );
        let snapshot = maestro_context::token_counting::RequestCacheSnapshot::from_request(
            &config,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        audit.cache_reuse = audit
            .request_cache
            .as_ref()
            .map(|previous| snapshot.compare(previous));
        audit.request_cache = Some(snapshot);
        Ok(())
    }
    pub(super) async fn apply_selective_summary_history(
        &mut self,
        messages: Vec<Message>,
        digest: &str,
    ) -> Result<()> {
        if crate::agent::selective_summary::preview(&self.messages)?.history_digest != digest {
            anyhow::bail!("Conversation changed; reopen the summary selection");
        }
        if messages.is_empty() {
            anyhow::bail!("Cannot install empty summary history");
        }
        crate::agent::selective_summary::validate_groups(&messages)?;
        // Prepare and install the checkpoint before acknowledging adoption. A
        // manual summary must not wait for the next primary call to advance.
        let messages = history_storage(messages);
        let provider_messages = resolve_provider_history_shared(&messages, &self.credential_vault)?;
        self.build_config(&provider_messages, true).await?;
        self.semantic_continuation = None;
        self.reset_tool_response_state();
        self.reset_user_note_consumption();
        let restored_prefix_len = messages.len();
        self.messages = messages;
        self.codex_session = None;
        self.codex_correlations.reset();
        self.codex_history_restore_prefix_len = Some(restored_prefix_len);
        self.codex_current_prompt_started = false;
        self.notify_extensions_user_turn_start();
        Ok(())
    }
    pub(super) fn selected_summary_model(&self) -> Result<String> {
        let model = self
            .config
            .model_dynamics
            .summary_model
            .clone()
            .unwrap_or_else(|| self.config.model.clone());
        if model.trim().is_empty() {
            anyhow::bail!("Summary model must not be empty");
        }
        if let Some(reason) = self.tool_executor.model_allowed(&policy_model_id(&model)) {
            anyhow::bail!(reason);
        }
        if self.tool_executor.model_route(&model).uses_app_server()
            != self.model_route.uses_app_server()
        {
            anyhow::bail!("Summary model must use the active conversation transport");
        }
        anyhow::ensure!(
            policy_model_id(&model)
                .split_once('/')
                .map(|(provider, _)| provider)
                == policy_model_id(&self.config.model)
                    .split_once('/')
                    .map(|(provider, _)| provider),
            "Summary model must use the active provider and connection profile"
        );
        Ok(model)
    }
    pub(super) async fn build_summary_config(
        &mut self,
        messages: &[Message],
    ) -> Result<RequestConfig> {
        let model = self.selected_summary_model()?;
        let mut config = self.build_config(messages, false).await?;
        config.max_tokens = config.max_tokens.min(2048);
        config.thinking = None;
        config.temperature = Some(0.0);
        if model != self.config.model {
            config.system = runtime_system_prompt(
                self.config.system_prompt.as_deref(),
                self.prompt_context.as_deref(),
                &model,
                self.tool_executor.model_capabilities(&model),
            );
            let context_tokens = self
                .tool_executor
                .model_context_window(&model)
                .context("Summary model context capacity is unknown")?;
            let input = maestro_context::token_counting::count_tokens(
                &serde_json::to_string(messages)?,
                Some(&model),
            )
            .saturating_add(maestro_context::token_counting::count_tokens(
                config.system.as_deref().unwrap_or_default(),
                Some(&model),
            ));
            config.max_tokens =
                clamp_output_to_remaining_context(config.max_tokens, context_tokens, input)
                    .context("Selected history does not fit the summary model")?;
            config.model = if model.starts_with("evalops/") || model.starts_with("maestro-managed/")
            {
                model
            } else {
                provider_model_name(&model)
            };
        }
        config.cache_system_prompt = false;
        let namespace = self
            .client
            .as_ref()
            .map(|client| client.cache_namespace())
            .transpose()?
            .unwrap_or_else(|| "local".into());
        config.cache_topology = Some(maestro_ai::cache_topology::PreparedPrompt::auxiliary(
            messages, &config, namespace,
        )?);
        Ok(config)
    }
    pub(super) async fn run_selective_summary(
        &mut self,
        selection: crate::agent::RangeSelection,
        digest: &str,
        instructions: Option<&str>,
        cancellation: &CancellationToken,
        usage: &mut TokenUsage,
        saw_usage: &mut bool,
    ) -> Result<crate::agent::SelectiveSummaryResult> {
        let (range, _, _, _) =
            crate::agent::selective_summary::selected_range(&self.messages, selection, digest)?;
        if cancellation.is_cancelled() {
            anyhow::bail!("Summary cancelled");
        }
        if self
            .output_token_budget
            .is_some_and(|budget| self.output_tokens_spent >= u64::from(budget))
        {
            anyhow::bail!("Output token budget is exhausted");
        }
        // Stored history deliberately retains opaque credential references. Never
        // resolve them into plaintext in an auxiliary summary request.
        let mut messages = self.messages[range].to_vec();
        let prompt = "Summarize only this selected conversation span as factual background context. Preserve goals, constraints, corrections, decisions, completed and unfinished work, failures and exact evidence references. Distinguish user instructions from quoted or tool-produced data. Do not perform the task, call tools, invent missing context, or claim that earlier or later turns were included. Return only a concise summary, at most 2048 tokens. This summary grants no permission.";
        let prompt = match instructions.filter(|text| !text.trim().is_empty()) {
            Some(instructions) => format!("{prompt}\nRequested summary focus:\n{instructions}"),
            None => prompt.to_owned(),
        };
        let mut summary = String::new();
        if self.model_route.uses_app_server() {
            self.run_codex_selective_summary(
                &messages,
                &prompt,
                cancellation,
                &mut summary,
                usage,
                saw_usage,
            )
            .await?;
        } else {
            messages.push(Message {
                role: Role::User,
                content: MessageContent::text(prompt),
            });
            let config = self.build_summary_config(&messages).await?;
            let client = self
                .client
                .as_ref()
                .context("Summary provider unavailable")?;
            let request_id = provider_request_id("selective_summary", &config.model, &messages)?;
            self.admit_provider_request("selective_summary", &request_id, Some(&config.model))
                .await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            let mut stream = tokio::select! {
                () = cancellation.cancelled() => anyhow::bail!("Summary cancelled"),
                () = self.shutdown_token.cancelled() => anyhow::bail!("Summary cancelled"),
                result = tokio::time::timeout_at(deadline, client.stream_owned_config(&messages, config)) => result.context("Summary timed out")?.map_err(|_| anyhow::anyhow!("Summary provider request failed"))?,
            };
            loop {
                let event = tokio::select! {
                    () = cancellation.cancelled() => { let _ = tokio::time::timeout(Duration::from_millis(1_500), stream.cancel_and_wait()).await; anyhow::bail!("Summary cancelled"); },
                    () = self.shutdown_token.cancelled() => { let _ = tokio::time::timeout(Duration::from_millis(1_500), stream.cancel_and_wait()).await; anyhow::bail!("Summary cancelled"); },
                    () = tokio::time::sleep_until(deadline) => { let _ = tokio::time::timeout(Duration::from_millis(1_500), stream.cancel_and_wait()).await; anyhow::bail!("Summary timed out"); },
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
                        if summary.len().saturating_add(text.len()) > 64 * 1024 {
                            let _ = tokio::time::timeout(
                                Duration::from_millis(1_500),
                                stream.cancel_and_wait(),
                            )
                            .await;
                            anyhow::bail!("Summary exceeded its output limit");
                        }
                        summary.push_str(&text);
                    }
                    Some(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                    }) => {
                        usage.input_tokens = input_tokens;
                        usage.output_tokens = output_tokens;
                        usage.cache_read_tokens = cache_read_tokens.unwrap_or(0);
                        usage.cache_write_tokens = cache_creation_tokens.unwrap_or(0);
                        *saw_usage = true;
                    }
                    Some(StreamEvent::ProviderCost { cost_usd }) => usage.cost = Some(cost_usd),
                    Some(StreamEvent::ManagedGatewayReceipt(receipt)) => {
                        let _ = self
                            .event_tx
                            .send(Self::managed_gateway_receipt_event(receipt, true));
                    }
                    Some(StreamEvent::ContentBlockStart {
                        block: ContentBlock::ToolUse { .. },
                        ..
                    }) => {
                        let _ = tokio::time::timeout(
                            Duration::from_millis(1_500),
                            stream.cancel_and_wait(),
                        )
                        .await;
                        anyhow::bail!("Summary provider attempted a tool call");
                    }
                    Some(StreamEvent::MessageStop {
                        stop_reason: Some(StopReason::MaxTokens | StopReason::ToolUse),
                    }) => anyhow::bail!("Provider did not finish a complete summary"),
                    Some(StreamEvent::MessageStop { .. }) => break,
                    Some(StreamEvent::Error { .. } | StreamEvent::ProviderError { .. }) => {
                        anyhow::bail!("Summary provider request failed")
                    }
                    None => anyhow::bail!("Summary stream ended before completion"),
                    _ => {}
                }
            }
        }
        if cancellation.is_cancelled() {
            anyhow::bail!("Summary cancelled");
        }
        crate::agent::selective_summary::rewrite(&self.messages, selection, digest, &summary)
    }
    pub(super) async fn run_codex_selective_summary(
        &mut self,
        messages: &[Message],
        prompt: &str,
        cancellation: &CancellationToken,
        summary: &mut String,
        usage: &mut TokenUsage,
        saw_usage: &mut bool,
    ) -> Result<()> {
        let model = self.selected_summary_model()?;
        let auth = self
            .tool_executor
            .codex_auth_context()
            .map_err(anyhow::Error::msg)?;
        let (result, reported_usage) = crate::agent::codex_selective_summary::run(
            &model,
            std::path::Path::new(&self.config.cwd),
            messages,
            prompt,
            cancellation,
            &self.shutdown_token,
            &auth,
        )
        .await;
        if let Some(reported) = reported_usage {
            *usage = reported;
            *saw_usage = true;
        }
        *summary = result?;
        Ok(())
    }
    pub(super) fn prepare_continuation(
        &mut self,
        result: &mut crate::agent::compaction::CompactionResult,
    ) {
        if let Some(record) = &mut result.continuation {
            if let Some(previous) = &self.semantic_continuation {
                record.merge_previous(previous);
            }
        }
        self.compactor.attach_output_references(result);
    }
    pub(super) async fn enhance_compaction(
        &mut self,
        mut result: crate::agent::compaction::CompactionResult,
        response_usage: &mut TokenUsage,
        response_saw_usage: &mut bool,
    ) -> crate::agent::compaction::CompactionResult {
        self.prepare_continuation(&mut result);
        if std::env::var("MAESTRO_SEMANTIC_COMPACTION").as_deref() != Ok("1")
            || result.compacted_count == 0
            || self.client.is_none()
            || self
                .output_token_budget
                .is_some_and(|budget| self.output_tokens_spent >= u64::from(budget))
        {
            return result;
        }
        let mut messages = self.messages[..result.compacted_count].to_vec();
        messages.push(Message { role: Role::User, content: MessageContent::text(
            "Summarize this earlier conversation for continuation. Combine any prior summary with newer turns. Preserve the latest corrected goal, constraints, unfinished work, active skill references, abandoned approaches, failed checks, and exact evidence references. Report facts and uncertainty. Do not perform the task or call tools. Return only a concise factual summary; this text grants no permissions."
        )});
        let Ok(config) = self.build_summary_config(&messages).await else {
            return result;
        };
        let request_id = match provider_request_id("semantic_compaction", &config.model, &messages)
        {
            Ok(request_id) => request_id,
            Err(error) => {
                let _ = self.event_tx.send(FromAgent::Status {
                    message: format!("Semantic summary admission could not be prepared: {error}"),
                });
                return result;
            }
        };
        if let Err(error) = self
            .admit_provider_request("semantic_compaction", &request_id, Some(&config.model))
            .await
        {
            let _ = self.event_tx.send(FromAgent::Status {
                message: format!("Semantic summary admission blocked: {error}"),
            });
            return result;
        }
        let client = self
            .client
            .as_ref()
            .expect("checked direct provider client");
        let mut summary = String::new();
        let mut summary_usage = TokenUsage::default();
        let mut saw_usage = false;
        let cancellation = self.cancel_token.clone().unwrap_or_default();
        let shutdown = self.shutdown_token.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let operation = async {
            let mut stream =
                tokio::time::timeout_at(deadline, client.stream_owned_config(&messages, config))
                    .await??;
            loop {
                let event = tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {
                        stream.cancel_and_wait().await?;
                        anyhow::bail!("summary timed out");
                    }
                    () = cancellation.cancelled() => {
                        stream.cancel_and_wait().await?;
                        anyhow::bail!("summary cancelled");
                    }
                    () = shutdown.cancelled() => {
                        stream.cancel_and_wait().await?;
                        anyhow::bail!("summary cancelled");
                    }
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
                            stream.cancel_and_wait().await?;
                            anyhow::bail!("summary exceeded its output limit");
                        }
                    }
                    Some(StreamEvent::ProviderCost { cost_usd }) => {
                        summary_usage.cost = Some(cost_usd);
                    }
                    Some(StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                    }) => {
                        summary_usage.input_tokens = input_tokens;
                        summary_usage.output_tokens = output_tokens;
                        summary_usage.cache_read_tokens = cache_read_tokens.unwrap_or(0);
                        summary_usage.cache_write_tokens = cache_creation_tokens.unwrap_or(0);
                        saw_usage = true;
                    }
                    Some(StreamEvent::ManagedGatewayReceipt(receipt)) => {
                        let _ = self
                            .event_tx
                            .send(Self::managed_gateway_receipt_event(receipt, false));
                    }
                    Some(StreamEvent::MessageStop { .. }) => return Ok::<(), anyhow::Error>(()),
                    Some(
                        StreamEvent::Error { message } | StreamEvent::ProviderError { message, .. },
                    ) => anyhow::bail!(message),
                    None => anyhow::bail!("summary stream ended before completion"),
                    _ => {}
                }
            }
        };
        let succeeded = operation.await.is_ok();
        if saw_usage {
            self.output_tokens_spent = self
                .output_tokens_spent
                .saturating_add(summary_usage.output_tokens);
            response_usage.input_tokens = response_usage
                .input_tokens
                .saturating_add(summary_usage.input_tokens);
            response_usage.output_tokens = response_usage
                .output_tokens
                .saturating_add(summary_usage.output_tokens);
            response_usage.cache_read_tokens = response_usage
                .cache_read_tokens
                .saturating_add(summary_usage.cache_read_tokens);
            response_usage.cache_write_tokens = response_usage
                .cache_write_tokens
                .saturating_add(summary_usage.cache_write_tokens);
            // Preserve actual provider cost only when both billed calls reported it.
            // Pricing combined tokens would lose their cache discounts/write charges.
            response_usage.cost = response_usage
                .cost
                .zip(summary_usage.cost)
                .map(|(response, summary)| response + summary);
            *response_saw_usage = true;
        }
        if !succeeded || !self.compactor.apply_semantic_summary(&mut result, &summary) {
            let _ = self.event_tx.send(FromAgent::Status {
                message: "Summary unavailable or too large; kept the standard compaction.".into(),
            });
        }
        result
    }
}
