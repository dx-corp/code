//! Per-read durable completion and result projection before script settlement.

use super::*;

impl NativeAgentRunner {
    pub(super) async fn drain_read_only_tool_calls(
        &mut self,
        pending: &mut Vec<QueuedReadOnlyToolExecution>,
        tool_results: &mut Vec<ContentBlock>,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        if pending
            .iter()
            .all(|call| call.tool_name.eq_ignore_ascii_case(classifier::TOOL_NAME))
        {
            return Box::pin(self.drain_classifier_wave(pending, tool_results)).await;
        }
        let pending_calls = std::mem::take(pending);
        let projection_start = tool_results.len();
        let proposal_order = pending_calls
            .iter()
            .enumerate()
            .map(|(index, call)| (call.call_id.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut operations = HashMap::new();
        for call in &pending_calls {
            let operation = self
                .begin_tool_operation(&call.call_id, &call.tool_name, &call.execution_args)
                .await
                .map_err(anyhow::Error::msg)?;
            operations.insert(call.call_id.clone(), operation);
        }
        let cancel_token = self
            .codemode_cancel
            .as_ref()
            .unwrap_or(&self.shutdown_token)
            .child_token();
        self.set_active_tool_cancel_token(Some(cancel_token.clone()), false);
        let wave_started = Instant::now();
        let calls = pending_calls
            .iter()
            .map(|call| super::super::native_host::NativeReadOnlyToolCall {
                call_id: call.call_id.clone(),
                tool_name: call.tool_name.clone(),
                args: call.execution_args.clone(),
            })
            .collect::<Vec<_>>();
        let host = self.tool_executor.clone();
        let event_tx = self.event_tx.clone();
        let (completion_tx, mut completion_rx) = tokio::sync::mpsc::unbounded_channel();
        let wave = host.execute_read_only_wave_stream(
            &calls,
            &event_tx,
            Some(cancel_token.clone()),
            completion_tx,
        );
        tokio::pin!(wave);
        let mut remaining = pending_calls
            .into_iter()
            .map(|call| (call.call_id.clone(), call))
            .collect::<HashMap<_, _>>();
        let mut completed = None;
        while completed.is_none() {
            enum ReadEvent {
                Result(Option<(String, Box<ToolExecution>)>),
                Wave(HashMap<String, ToolExecution>),
                Script(Option<agent_codemode::Event>),
            }
            let can_pipeline = self.codemode_session.is_some()
                && self.codemode_events.is_empty()
                && self.extensions.allows_independent_script_results();
            let event = tokio::select! {
                result = completion_rx.recv() => ReadEvent::Result(result.map(|(id, execution)| (id, Box::new(execution)))),
                results = &mut wave => ReadEvent::Wave(results),
                event = async { self.codemode_session.as_mut().unwrap().next().await }, if can_pipeline => ReadEvent::Script(event),
            };
            match event {
                ReadEvent::Result(Some((id, result))) => {
                    if let Some(call) = remaining.remove(&id) {
                        self.finish_read_only_tool_call(
                            call,
                            *result,
                            operations.remove(&id),
                            wave_started.elapsed().as_millis() as u64,
                            tool_results,
                        )
                        .await;
                    }
                }
                ReadEvent::Result(None) => {
                    completed = Some(wave.as_mut().await);
                }
                ReadEvent::Wave(results) => {
                    completed = Some(results);
                }
                ReadEvent::Script(Some(agent_codemode::Event::Calls { calls, reply }))
                    if calls
                        .iter()
                        .all(|call| self.codemode_pipeline_read(&call.name)) =>
                {
                    Box::pin(self.execute_codemode_calls(calls, reply))
                        .await
                        .map_err(anyhow::Error::msg)?;
                    self.set_active_tool_cancel_token(Some(cancel_token.clone()), false);
                }
                ReadEvent::Script(Some(event)) => {
                    self.codemode_events.push_back(event);
                }
                ReadEvent::Script(None) => { /* Owner wave still requires durable completion. */ }
            }
        }
        let mut completed = completed.unwrap();
        for (id, call) in remaining {
            let result = completed.remove(&id).unwrap_or_else(|| {
                ToolExecution::from_legacy(
                    &id,
                    &call.tool_name,
                    ExecutionSource::Native,
                    ToolResult::failure("Tool task did not return a result"),
                )
                .with_managed_policy(self.tool_executor.managed_policy_metadata())
            });
            self.finish_read_only_tool_call(
                call,
                result,
                operations.remove(&id),
                wave_started.elapsed().as_millis() as u64,
                tool_results,
            )
            .await;
        }
        tool_results[projection_start..].sort_by_key(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => proposal_order
                .get(tool_use_id)
                .copied()
                .unwrap_or(usize::MAX),
            _ => usize::MAX,
        });
        self.set_active_tool_cancel_token(None, false);
        Ok(())
    }

    pub(super) async fn finish_read_only_tool_call(
        &mut self,
        call: QueuedReadOnlyToolExecution,
        result: ToolExecution,
        operation: Option<maestro_runtime_contracts::ToolOperationRecord>,
        wave_duration_ms: u64,
        tool_results: &mut Vec<ContentBlock>,
    ) {
        if let Some(operation) = operation {
            self.record_tool_operation_outcome(operation, &result).await;
        }
        let content = self
            .credential_vault
            .vault_in_text(&if self.codemode_cancel.is_some() {
                result.raw_content()
            } else {
                result.model_content()
            });
        let is_error = result.is_error();
        let script_value = result.script_value.clone().unwrap_or_else(|| {
            serde_json::from_str(&content).unwrap_or(Value::String(content.clone()))
        });
        let accepted_mcp_error = result.script_value.is_some() && is_error;

        // Hooks receive the tool body before the model-facing envelope;
        // the shared dispatcher vaults credentials first.
        let hook_outcome = run_post_execution_hooks(
            &self.hooks,
            &self.credential_vault,
            PostExecutionHookInput {
                tool_name: &call.tool_name,
                call_id: &call.call_id,
                args: &call.args,
                raw_output: &result.raw_content(),
                is_error,
                duration_ms: result.receipt.duration_ms.unwrap_or(wave_duration_ms),
            },
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

        let before_extensions = self.credential_vault.vault_in_text(&final_content);
        let (final_content, reported_error) = self.apply_tool_result_extensions(
            &call.call_id,
            &call.tool_name,
            &call.safe_args,
            result.receipt.duration_ms.unwrap_or(wave_duration_ms),
            final_content,
            reported_error,
            Some(&result.receipt),
        );

        if hook_outcome.rejected.is_none() && (!reported_error || accepted_mcp_error) {
            self.retain_codemode_value(
                &call.call_id,
                &script_value,
                &before_extensions,
                &final_content,
                accepted_mcp_error,
            );
        }
        self.settle_codemode_result(&call.call_id, &final_content, reported_error);
        tool_results.push(ContentBlock::ToolResult {
            tool_use_id: call.call_id.clone(),
            content: final_content,
            is_error: Some(reported_error),
        });
        self.complete_tool_operation(&call.call_id).await;
    }
}
