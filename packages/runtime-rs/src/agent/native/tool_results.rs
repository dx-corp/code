//! Tool dispatch, result projection, and completion bookkeeping.

use super::*;

pub(super) fn tool_operation_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |duration| {
            duration.as_millis().min(u64::MAX as u128) as u64
        })
}

impl NativeAgentRunner {
    async fn begin_tool_operation(
        &self,
        call_id: &str,
        tool_name: &str,
        admitted_arguments: &Value,
    ) -> Result<maestro_runtime_contracts::ToolOperationRecord, String> {
        let admission = self
            .tool_executor
            .tool_operation_admission(tool_name, admitted_arguments);
        let planned = maestro_runtime_contracts::ToolOperationRecord::planned(
            call_id,
            tool_name,
            admitted_arguments.clone(),
            admission.idempotency_key,
            admission.replay_policy,
            tool_operation_now_ms(),
        )
        .map_err(|error| error.to_string())?;
        self.hooks
            .hook_record_tool_operation(&planned)
            .await
            .map_err(|error| format!("failed to persist planned tool operation: {error}"))?;
        let pending = planned
            .effect_pending(tool_operation_now_ms())
            .map_err(|error| error.to_string())?;
        self.hooks
            .hook_record_tool_operation(&pending)
            .await
            .map_err(|error| format!("failed to persist pending tool effect: {error}"))?;
        Ok(pending)
    }

    pub(super) async fn record_tool_operation_outcome(
        &self,
        pending: maestro_runtime_contracts::ToolOperationRecord,
        execution: &ToolExecution,
    ) {
        let outcome = maestro_runtime_contracts::ToolOperationOutcome::new(
            execution.model_content(),
            execution.is_error(),
            Some(execution.receipt.clone()),
        );
        let ready = match pending.outcome_ready(outcome, tool_operation_now_ms()) {
            Ok(ready) => ready,
            Err(error) => {
                self.tool_executor.report_diagnostic(format!(
                    "tool operation outcome rejected for {}: {error}",
                    execution.receipt.call_id
                ));
                return;
            }
        };
        if let Err(error) = self.hooks.hook_record_tool_operation(&ready).await {
            self.tool_executor.report_diagnostic(format!(
                "tool operation outcome persistence failed for {}: {error}",
                execution.receipt.call_id
            ));
        }
    }

    pub(super) async fn complete_tool_operation(&self, call_id: &str) {
        let records = match self.hooks.hook_load_tool_operations().await {
            Ok(records) => records,
            Err(error) => {
                self.tool_executor.report_diagnostic(format!(
                    "tool operation completion load failed for {call_id}: {error}"
                ));
                return;
            }
        };
        let Some(ready) = records.into_iter().find(|record| {
            record.call_id == call_id
                && record.phase == maestro_runtime_contracts::ToolOperationPhase::OutcomeReady
        }) else {
            return;
        };
        let completed = match ready.completed(tool_operation_now_ms()) {
            Ok(completed) => completed,
            Err(error) => {
                self.tool_executor.report_diagnostic(format!(
                    "tool operation completion rejected for {call_id}: {error}"
                ));
                return;
            }
        };
        if let Err(error) = self.hooks.hook_record_tool_operation(&completed).await {
            self.tool_executor.report_diagnostic(format!(
                "tool operation completion persistence failed for {call_id}: {error}"
            ));
        }
    }

    pub(super) async fn replay_tool_operation(
        &mut self,
        pending: maestro_runtime_contracts::ToolOperationRecord,
    ) -> ToolExecution {
        let call_id = pending.call_id.clone();
        let tool_name = pending.tool_name.clone();
        let args = tool_args_for_execution(&pending.admitted_arguments);
        let started = Instant::now();
        let cancel = self.shutdown_token.child_token();
        let terminal_drain_required =
            native_tool_requires_terminal_drain(&self.tool_executor, &tool_name, &args);
        self.set_active_tool_cancel_token(Some(cancel.clone()), terminal_drain_required);
        let execution = self
            .tool_executor
            .execute_tool(
                &tool_name,
                &args,
                Some(&self.event_tx),
                &call_id,
                NativeToolExecutionOptions {
                    cancel,
                    approved_inline_env: None,
                },
            )
            .await;
        let execution = self
            .tool_executor
            .with_managed_policy(execution.with_duration(started.elapsed().as_millis() as u64));
        self.set_active_tool_cancel_token(None, false);
        invalidate_cache_after_serial_tool(&self.tool_executor, &tool_name, true);
        self.record_tool_operation_outcome(pending, &execution)
            .await;
        execution
    }

    pub(super) async fn execute_recall_output(
        &mut self,
        args: &Value,
        call_id: &str,
    ) -> ToolExecution {
        let event_tx = self.event_tx.clone();
        let emit = move |execution: &ToolExecution| {
            let _ = event_tx.send(FromAgent::ToolStart {
                call_id: call_id.to_string(),
            });
            let result = execution.to_legacy();
            if !result.output.is_empty() {
                let _ = event_tx.send(FromAgent::ToolOutput {
                    call_id: call_id.to_string(),
                    content: result.output.clone(),
                });
            }
            let _ = event_tx.send(FromAgent::ToolEnd {
                call_id: call_id.to_string(),
                success: result.success,
                result: Some(result),
                receipt: Some(execution.receipt.clone()),
            });
        };

        let requested_id = args
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if requested_id.is_empty() {
            let execution = ToolExecution::from_legacy(
                call_id,
                "recall_output",
                ExecutionSource::Native,
                ToolResult::failure("recall_output requires a tool-call id"),
            )
            .with_managed_policy(self.tool_executor.managed_policy_metadata());
            emit(&execution);
            return execution;
        }

        let durable_content = self.messages.iter().rev().find_map(|message| {
            let MessageContent::Blocks(blocks) = &message.content else {
                return None;
            };
            blocks.iter().find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == requested_id => Some(content.clone()),
                _ => None,
            })
        });
        let spill_path = self
            .semantic_continuation
            .as_ref()
            .and_then(|continuation| {
                continuation
                    .tool_outputs
                    .iter()
                    .rev()
                    .find(|reference| reference.tool_call_id == requested_id)
                    .map(|reference| reference.path.clone())
            });
        let spilled_content = if let Some(path) = spill_path.as_deref() {
            tokio::fs::read_to_string(path).await.ok()
        } else {
            None
        };
        let (source, Some(content)) = spilled_content
            .map(|content| ("spill", Some(content)))
            .unwrap_or_else(|| ("history", durable_content))
        else {
            let execution = ToolExecution::from_legacy(
                call_id,
                "recall_output",
                ExecutionSource::Native,
                ToolResult::failure(format!("No retained output exists for `{requested_id}`")),
            )
            .with_managed_policy(self.tool_executor.managed_policy_metadata());
            emit(&execution);
            return execution;
        };

        let offset = args
            .get("offsetChars")
            .or_else(|| args.get("offset_chars"))
            .and_then(Value::as_u64)
            .map_or(0, |value| value as usize);
        let max_chars = args
            .get("maxChars")
            .or_else(|| args.get("max_chars"))
            .and_then(Value::as_u64)
            .map_or(20_000, |value| value.clamp(1, 100_000) as usize);
        let total_chars = content.chars().count();
        let recalled = content
            .chars()
            .skip(offset)
            .take(max_chars)
            .collect::<String>();
        let result = ToolResult::success(recalled).with_details(json!({
            "recalledToolCallId": requested_id,
            "source": source,
            "offsetChars": offset,
            "returnedChars": content.chars().skip(offset).take(max_chars).count(),
            "totalChars": total_chars,
            "truncated": offset.saturating_add(max_chars) < total_chars,
        }));
        let execution =
            ToolExecution::from_legacy(call_id, "recall_output", ExecutionSource::Native, result)
                .with_managed_policy(self.tool_executor.managed_policy_metadata());
        emit(&execution);
        execution
    }

    pub(super) fn execute_tool_search(&mut self, args: &Value, call_id: &str) -> ToolExecution {
        let emit = |execution: &ToolExecution| {
            let _ = self.event_tx.send(FromAgent::ToolStart {
                call_id: call_id.to_string(),
            });
            let result = execution.to_legacy();
            if !result.output.is_empty() {
                let _ = self.event_tx.send(FromAgent::ToolOutput {
                    call_id: call_id.to_string(),
                    content: result.output.clone(),
                });
            }
            let _ = self.event_tx.send(FromAgent::ToolEnd {
                call_id: call_id.to_string(),
                success: result.success,
                result: Some(result),
                receipt: Some(execution.receipt.clone()),
            });
        };

        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let exact_names = args
            .get("names")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_ascii_lowercase)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        if query.is_empty() && exact_names.is_empty() {
            let execution = ToolExecution::from_legacy(
                call_id,
                "tool_search",
                ExecutionSource::Native,
                ToolResult::failure("tool_search requires query or names"),
            )
            .with_managed_policy(self.tool_executor.managed_policy_metadata());
            emit(&execution);
            return execution;
        }

        let terms = query.split_whitespace().collect::<Vec<_>>();
        let mut candidates = self
            .tools
            .iter()
            .filter_map(|(name, definition)| {
                let name_lower = name.to_ascii_lowercase();
                if name_lower == "tool_search"
                    || self.tool_executor.is_reserved_tool(name)
                    || !tool_search_profile_allows(
                        self.tool_profile,
                        name,
                        &self.explicitly_allowed_tools,
                    )
                    || !tool_is_visible_to_model(
                        name,
                        self.goal_tools_visible,
                        self.include_ide_tools,
                    )
                {
                    return None;
                }
                let description = definition.tool.description.to_ascii_lowercase();
                let exact = exact_names.contains(&name_lower);
                let mut score = if exact { 1_000 } else { 0 };
                for term in &terms {
                    if name_lower.contains(term) {
                        score += 50;
                    }
                    if description.contains(term) {
                        score += 10;
                    }
                }
                (score > 0).then_some((score, name_lower, definition.tool.description.clone()))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));

        let max_results = args
            .get("maxResults")
            .and_then(Value::as_u64)
            .map_or(8, |value| value.clamp(1, 16) as usize);
        let selected = candidates.into_iter().take(max_results).collect::<Vec<_>>();
        if selected.is_empty() {
            let execution = ToolExecution::from_legacy(
                call_id,
                "tool_search",
                ExecutionSource::Native,
                ToolResult::failure(format!("No tools matched `{query}`")),
            )
            .with_managed_policy(self.tool_executor.managed_policy_metadata());
            emit(&execution);
            return execution;
        }

        let mut activated = Vec::new();
        let mut lines = Vec::with_capacity(selected.len() + 1);
        lines.push("Activated tools for the next turn:".to_string());
        for (_, name, description) in selected {
            if self.active_tool_names.insert(name.clone()) {
                activated.push(name.clone());
            }
            lines.push(format!("- {name}: {description}"));
        }
        if !activated.is_empty() {
            self.model_tool_cache = None;
            self.refresh_runtime_audit();
        }
        let result = ToolResult::success(lines.join("\n")).with_details(json!({
            "activated": activated,
            "nextTurn": true,
        }));
        let execution =
            ToolExecution::from_legacy(call_id, "tool_search", ExecutionSource::Native, result)
                .with_managed_policy(self.tool_executor.managed_policy_metadata());
        emit(&execution);
        execution
    }
    /// Execute a tool using the `ToolExecutor`
    pub(super) async fn execute_tool(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        call_id: &str,
        approved_inline_env: Option<&HashMap<String, String>>,
    ) -> ToolExecution {
        if tool_name.eq_ignore_ascii_case("spawn_subagent") {
            let mut parent_record =
                crate::agent::compaction::build_continuation_record(&self.messages);
            if let Some(previous) = &self.semantic_continuation {
                parent_record.merge_previous(previous);
            }
            self.tool_executor
                .set_subagent_parent_requests(parent_record.user_requests);
        }
        let operation = match self.begin_tool_operation(call_id, tool_name, args).await {
            Ok(operation) => operation,
            Err(error) => {
                return ToolExecution::from_legacy(
                    call_id,
                    tool_name,
                    ExecutionSource::Native,
                    ToolResult::failure(format!(
                        "Tool was not executed because its durable operation record failed: {error}"
                    )),
                )
                .with_managed_policy(self.tool_executor.managed_policy_metadata());
            }
        };
        let started = Instant::now();
        let span = tool_span_for_call(tool_name, Some(call_id));
        if tool_name.eq_ignore_ascii_case("recall_output") {
            let execution = self.execute_recall_output(args, call_id).await;
            self.record_tool_operation_outcome(operation, &execution)
                .await;
            record_outcome(
                &span,
                if execution.is_error() {
                    "error"
                } else {
                    "success"
                },
                started.elapsed(),
                execution.is_error().then_some("tool_error"),
            );
            return execution;
        }
        if tool_name.eq_ignore_ascii_case("tool_search") {
            let execution = span.in_scope(|| self.execute_tool_search(args, call_id));
            self.record_tool_operation_outcome(operation, &execution)
                .await;
            record_outcome(
                &span,
                if execution.is_error() {
                    "error"
                } else {
                    "success"
                },
                started.elapsed(),
                execution.is_error().then_some("tool_error"),
            );
            return execution;
        }

        let cancel = self.shutdown_token.child_token();
        let terminal_drain_required =
            native_tool_requires_terminal_drain(&self.tool_executor, tool_name, args);
        self.set_active_tool_cancel_token(Some(cancel.clone()), terminal_drain_required);
        // Timed here because this is the one place the runner owns a single
        // tool's execution. `ExecutionReceipt::duration_ms` had no producer, so
        // the documented `durationMs` hook field could never be populated.
        let execution = self
            .tool_executor
            .execute_tool(
                tool_name,
                args,
                Some(&self.event_tx),
                call_id,
                NativeToolExecutionOptions {
                    cancel,
                    approved_inline_env,
                },
            )
            .instrument(span.clone())
            .await;
        let execution = self
            .tool_executor
            .with_managed_policy(execution.with_duration(started.elapsed().as_millis() as u64));
        self.set_active_tool_cancel_token(None, false);
        // Direct Codex/native dispatch does not pass through the main turn's
        // serial-tool boundary. Keep the warm executor honest after a Bash,
        // inline, MCP, or other side-effecting call from that path too.
        invalidate_cache_after_serial_tool(&self.tool_executor, tool_name, true);
        if tool_name.eq_ignore_ascii_case("update_goal")
            && execution.receipt.source == ExecutionSource::Native
        {
            if let Some(visible) = goal_tools_visible_from_execution(&execution) {
                self.set_goal_tools_visible(visible);
            }
        }
        record_outcome(
            &span,
            if execution.is_error() {
                "error"
            } else {
                "success"
            },
            started.elapsed(),
            execution.is_error().then_some("tool_error"),
        );
        self.record_tool_operation_outcome(operation, &execution)
            .await;
        execution
    }
    /// Build the model result for a decided tool call. Approved local tools
    /// without a result execute here; caller-owned tools fail without one.
    /// Post-execution hooks and bookkeeping only observe supplied results or
    /// local execution.
    pub(super) async fn finalize_tool_call_result(
        &mut self,
        call: ToolCallContext,
        approved: bool,
        result: Option<ToolExecution>,
    ) -> ContentBlock {
        let ToolCallContext {
            call_id,
            tool_name,
            args,
            safe_args,
            extra_context,
            pre_hook_args: _,
            initial_firewall_verdict: _,
            approval_inline_env,
        } = call;
        if !approved {
            // The user's refusal is remembered for the rest of this turn, so
            // an identical retry is answered without prompting again.
            self.denial_memory.record(&tool_name, &safe_args);
        }
        let mut result = result;
        let caller_owns_execution = approved
            && result.is_none()
            && self
                .external_tools
                .contains(&tool_name.to_ascii_lowercase());
        if caller_owns_execution {
            // A caller-owned approval without a result is a failed handoff.
            // Surface that failure to the model without treating it as a
            // locally executed tool: post-execution hooks, workflow updates,
            // and result extensions must only observe real execution.
            let result = ToolExecution::from_legacy(
                &call_id,
                &tool_name,
                ExecutionSource::RemoteClient,
                ToolResult::failure("Tool task did not return a result"),
            )
            .with_managed_policy(self.tool_executor.managed_policy_metadata());
            let session_id = self.hooks.hook_session_id().await;
            let spill_dir = model_tool_spill_dir_for_active_tools(
                Some(&self.tool_executor),
                &self.active_tool_names,
                &self.config.cwd,
                session_id.as_deref(),
                self.owns_persistent_tool_spills,
            );
            let content = self.tool_executor.clamp_tool_output(
                &result.model_content(),
                &tool_name,
                spill_dir.as_deref(),
            );
            return ContentBlock::ToolResult {
                tool_use_id: call_id,
                content: content.content,
                is_error: Some(true),
            };
        }
        if approved && result.is_none() {
            let execution_args = tool_args_for_execution(&safe_args);
            let approved_environment = approval_inline_env
                .as_ref()
                .map(|context| &context.environment);
            result = Some(
                self.execute_tool(&tool_name, &execution_args, &call_id, approved_environment)
                    .await,
            );
        }

        let result = result.unwrap_or_else(|| {
            if approved {
                ToolExecution::from_legacy(
                    &call_id,
                    &tool_name,
                    ExecutionSource::Native,
                    ToolResult::failure("Tool task did not return a result"),
                )
                .with_managed_policy(self.tool_executor.managed_policy_metadata())
            } else {
                ToolExecution::denied(&call_id, &tool_name, DenialReason::User)
                    .with_managed_policy(self.tool_executor.managed_policy_metadata())
            }
        });

        // Model-facing bound. The renderer clamp in `tool_output` never
        // covered this path, so a single large tool result went into
        // conversation history verbatim. Spill above 40 KB, sanitize control
        // characters (NUL included) on every result.
        let session_id = self.hooks.hook_session_id().await;
        let spill_dir = model_tool_spill_dir_for_active_tools(
            Some(&self.tool_executor),
            &self.active_tool_names,
            &self.config.cwd,
            session_id.as_deref(),
            self.owns_persistent_tool_spills,
        );
        let content = self.tool_executor.clamp_tool_output(
            &result.model_content(),
            &tool_name,
            spill_dir.as_deref(),
        );
        let is_error = result.is_error();

        // Bash bounds its own model projection before the outer clamp runs.
        // Retain its original capture, not just a possible spill of that tail.
        let captured_path = match &result.receipt.details {
            crate::agent::protocol::ToolReceiptDetails::BuiltIn(crate::ToolDetails::Bash(
                details,
            )) if matches!(result.receipt.source, ExecutionSource::Native) => {
                details.full_output_path.clone()
            }
            _ => None,
        };
        for path in captured_path.into_iter().chain(
            content
                .saved_path
                .map(|path| path.to_string_lossy().into_owned()),
        ) {
            let outputs = &mut self
                .semantic_continuation
                .get_or_insert_with(Default::default)
                .tool_outputs;
            if !outputs
                .iter()
                .any(|output| output.tool_call_id == call_id && output.path == path)
            {
                outputs.push(crate::agent::compaction::ToolOutputReference {
                    tool_call_id: call_id.clone(),
                    path,
                });
            }
        }
        let content = content.content;

        let hook_outcome = if approved {
            // Execute hooks only for tools that were allowed to run.
            // Hooks contract on raw tool output, not the model-facing
            // envelope (see `ToolExecution::raw_content`).
            run_post_execution_hooks(
                &self.hooks,
                &tool_name,
                &call_id,
                &args,
                &result.raw_content(),
                is_error,
                result.receipt.duration_ms.unwrap_or(0),
            )
            .await
        } else {
            PostExecutionHooks::default()
        };
        // The gate's verdict changes what the model is told, not what the
        // workflow bookkeeping below records: the tool really did run.
        let reported_error = is_error || hook_outcome.rejected.is_some();

        // Append injected context if any. A `PostToolUse` hook's context was
        // computed and then dropped, so a hook that returned `contextToAdd`
        // had no effect on the request that followed.
        let mut result_content = append_hook_context(
            &self.hooks,
            content,
            NativeHookEvent::PreToolUse,
            extra_context.as_deref(),
        );
        result_content = append_hook_context(
            &self.hooks,
            result_content,
            NativeHookEvent::PostToolUse,
            hook_outcome.context.as_deref(),
        );
        if let Some(reason) = &hook_outcome.rejected {
            result_content =
                format!("{result_content}\n\n[Eval gate rejected this result: {reason}]");
        }

        if approved {
            if let Err(err) = apply_workflow_state_hooks(
                &tool_name,
                &call_id,
                &args,
                &mut self.workflow_state,
                is_error,
            ) {
                // Append workflow hook error to content instead of replacing it
                // to preserve successful tool output
                result_content = format!("{}\n\n[Workflow error: {}]", result_content, err.message);
            }
        }

        // Hand the finished call to the extensions. The `doom-loop` tenant
        // records it here, which is where `SafetyController::record_tool_call`
        // used to be called directly.
        let (result_content, reported_error) = self.apply_tool_result_extensions(
            &call_id,
            &tool_name,
            &safe_args,
            result.receipt.duration_ms.unwrap_or(0),
            result_content,
            reported_error,
            Some(&result.receipt),
        );

        self.complete_tool_operation(&call_id).await;
        ContentBlock::ToolResult {
            tool_use_id: call_id,
            content: result_content,
            is_error: Some(reported_error),
        }
    }
    pub(super) async fn bound_final_tool_results(&mut self, results: &mut [ContentBlock]) {
        let session_id = self.hooks.hook_session_id().await;
        let spill_dir = model_tool_spill_dir_for_active_tools(
            Some(&self.tool_executor),
            &self.active_tool_names,
            &self.config.cwd,
            session_id.as_deref(),
            self.owns_persistent_tool_spills,
        );
        for block in results {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            else {
                continue;
            };
            // No prose-based dispatch: the name is only a safe spill-file label.
            let output =
                self.tool_executor
                    .clamp_tool_output(content, "tool-result", spill_dir.as_deref());
            *content = output.content;
            if let Some(path) = output.saved_path {
                let reference = crate::agent::compaction::ToolOutputReference {
                    tool_call_id: tool_use_id.clone(),
                    path: path.to_string_lossy().into_owned(),
                };
                let references = &mut self
                    .semantic_continuation
                    .get_or_insert_with(Default::default)
                    .tool_outputs;
                if !references.contains(&reference) {
                    references.push(reference);
                }
            }
        }
    }
    pub(super) fn retain_file_operation(
        &mut self,
        call_id: &str,
        execution: &crate::agent::protocol::ExecutionReceipt,
    ) {
        if let Some(operation) = successful_file_operation(call_id, execution) {
            let operations = &mut self
                .semantic_continuation
                .get_or_insert_with(Default::default)
                .file_operations;
            if !operations.contains(&operation) {
                operations.push(operation);
            }
        }
    }
    pub(super) fn cancel_remaining_deferred_if_interrupted(
        &mut self,
        deferred_calls: &mut impl Iterator<Item = DeferredToolCall>,
        tool_results: &mut Vec<ContentBlock>,
    ) -> bool {
        if !self.take_active_operation_interruption() {
            return false;
        }
        let cancelled_ids = cancel_deferred_suffix(
            &self.event_tx,
            deferred_calls,
            tool_results,
            self.tool_executor.managed_policy_metadata(),
        );
        self.tool_response_coordinator
            .discard_cancelled(&cancelled_ids);
        true
    }
    pub(super) async fn drain_read_only_tool_calls(
        &mut self,
        pending: &mut Vec<QueuedReadOnlyToolExecution>,
        tool_results: &mut Vec<ContentBlock>,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let pending_calls = std::mem::take(pending);
        let mut operations = HashMap::new();
        for call in &pending_calls {
            let operation = self
                .begin_tool_operation(&call.call_id, &call.tool_name, &call.execution_args)
                .await
                .map_err(anyhow::Error::msg)?;
            operations.insert(call.call_id.clone(), operation);
        }
        let cancel_token = CancellationToken::new();
        self.set_active_tool_cancel_token(Some(cancel_token.clone()), false);
        // These calls run concurrently in one batch, so the batch is the only
        // interval this path can measure. Each call is reported with the batch
        // elapsed, which is an upper bound on its own -- documented in
        // `docs/design/HOOKS_SYSTEM.md` so a hook reading `durationMs` knows
        // what it is looking at.
        let wave_started = Instant::now();
        let mut results_by_call_id = execute_native_read_only_tool_wave(
            &self.tool_executor,
            &self.event_tx,
            &pending_calls,
            Some(cancel_token),
        )
        .await;
        let wave_duration_ms = wave_started.elapsed().as_millis() as u64;
        self.set_active_tool_cancel_token(None, false);

        for call in pending_calls {
            let result = results_by_call_id.remove(&call.call_id).unwrap_or_else(|| {
                ToolExecution::from_legacy(
                    &call.call_id,
                    &call.tool_name,
                    ExecutionSource::Native,
                    ToolResult::failure("Tool task did not return a result"),
                )
                .with_managed_policy(self.tool_executor.managed_policy_metadata())
            });
            if let Some(operation) = operations.remove(&call.call_id) {
                self.record_tool_operation_outcome(operation, &result).await;
            }
            let content = result.model_content();
            let is_error = result.is_error();

            // Hooks contract on raw tool output, not the model-facing
            // envelope (see `ToolExecution::raw_content`).
            let hook_outcome = run_post_execution_hooks(
                &self.hooks,
                &call.tool_name,
                &call.call_id,
                &call.args,
                &result.raw_content(),
                is_error,
                result.receipt.duration_ms.unwrap_or(wave_duration_ms),
            )
            .await;
            let reported_error = is_error || hook_outcome.rejected.is_some();

            let mut final_content = append_hook_context(
                &self.hooks,
                content,
                NativeHookEvent::PreToolUse,
                call.extra_context.as_deref(),
            );
            final_content = append_hook_context(
                &self.hooks,
                final_content,
                NativeHookEvent::PostToolUse,
                hook_outcome.context.as_deref(),
            );
            if let Some(reason) = &hook_outcome.rejected {
                final_content =
                    format!("{final_content}\n\n[Eval gate rejected this result: {reason}]");
            }

            if let Err(err) = apply_workflow_state_hooks(
                &call.tool_name,
                &call.call_id,
                &call.args,
                &mut self.workflow_state,
                is_error,
            ) {
                final_content = format!("{}\n\n[Workflow error: {}]", final_content, err.message);
            }

            let (final_content, reported_error) = self.apply_tool_result_extensions(
                &call.call_id,
                &call.tool_name,
                &call.safe_args,
                result.receipt.duration_ms.unwrap_or(wave_duration_ms),
                final_content,
                reported_error,
                Some(&result.receipt),
            );

            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: call.call_id.clone(),
                content: final_content,
                is_error: Some(reported_error),
            });
            self.complete_tool_operation(&call.call_id).await;
        }

        Ok(())
    }
}
