//! Nested script admission and independently settled read replies.

use super::*;

impl NativeAgentRunner {
    pub(super) async fn next_codemode_event(&mut self) -> Option<agent_codemode::Event> {
        if let Some(event) = self.codemode_events.pop_front() {
            return Some(event);
        }
        self.codemode_session.as_mut()?.next().await
    }

    pub(super) fn codemode_pipeline_read(&self, name: &str) -> bool {
        let key = name.to_ascii_lowercase();
        let Some(definition) = self.tools.get(&key) else {
            return false;
        };
        if self.external_tools.contains(&key) {
            return false;
        }
        is_native_parallel_read_only_tool_call(
            name,
            definition.requires_approval,
            self.tool_executor.tool_annotations(name).as_ref(),
            is_explicit_inline_read_only_tool(name, &self.tool_executor),
        )
    }

    pub(super) fn settle_codemode_result(&mut self, call_id: &str, content: &str, is_error: bool) {
        if !self.extensions.allows_independent_script_results() {
            return;
        }
        if let Some((reply, index)) = self.codemode_replies.remove(call_id) {
            let value = self.codemode_response(call_id, content.to_owned(), is_error);
            let _ = reply.send(vec![(index, value)]);
        }
    }

    pub(super) async fn execute_codemode_calls(
        &mut self,
        calls: Vec<agent_codemode::Call>,
        reply: std::sync::mpsc::Sender<agent_codemode::Reply>,
    ) -> Result<(), String> {
        let call_id = self
            .codemode_parent_call_id
            .clone()
            .ok_or("Missing script parent")?;
        let admitted = self
            .codemode_catalog()
            .iter()
            .map(|tool| tool.name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        if self
            .codemode_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
            || self.take_active_operation_interruption()
        {
            if let Some(cancel) = &self.codemode_cancel {
                cancel.cancel();
            }
            let _ = reply.send(
                calls
                    .into_iter()
                    .map(|call| (call.index, Err("Script cancelled".to_owned())))
                    .collect(),
            );
            return Err(
                "Script cancelled; calls already executed retain their receipts.".to_owned(),
            );
        }
        for call in &calls {
            let id = format!("{call_id}/{}", call.index);
            self.codemode_progress.insert(
                id.clone(),
                super::super::protocol::CodeModeChildProgress {
                    call_id: id,
                    tool: call.name.clone(),
                    status: None,
                    duration_ms: None,
                },
            );
        }
        self.emit_codemode_progress();
        let ids = calls
            .iter()
            .map(|call| (format!("{call_id}/{}", call.index), call.index))
            .collect::<HashMap<_, _>>();
        for (id, index) in &ids {
            self.codemode_replies
                .insert(id.clone(), (reply.clone(), *index));
        }
        let mut batch = Vec::with_capacity(calls.len());
        for call in calls {
            let parse_error = if !admitted.contains(&call.name.to_ascii_lowercase()) {
                Some(format!(
                    "Tool `{}` is not available in this script",
                    call.name
                ))
            } else {
                self.codemode_tool_budget
                    .admit_tool(&call.name, &call.args)
                    .err()
                    .map(str::to_owned)
            };
            batch.push((
                format!("{call_id}/{}", call.index),
                call.name,
                call.args,
                parse_error,
            ));
        }
        let process_admission = self
            .process_budget
            .as_ref()
            .map(|state| {
                state
                    .lock()
                    .map_err(|_| "process budget poisoned".to_owned())?
                    .admit_tools(batch.len())
                    .map_err(str::to_owned)
            })
            .transpose();
        if let Err(reason) = process_admission {
            // Refuse each proposal through the same result/journal
            // path without scheduling any executable call.
            for (_, _, _, refusal) in &mut batch {
                *refusal = Some(reason.clone());
            }
        }
        let batch_arguments = batch
            .iter()
            .map(|(id, name, args, _)| (id.clone(), (name.clone(), args.clone())))
            .collect::<HashMap<_, _>>();
        // Boxing breaks the async call graph: execute_tool can enter
        // this method, but the catalog prevents recursive scripts.
        let mut results = match Box::pin(self.execute_tool_batch(batch, true)).await {
            Ok((results, _)) => results,
            Err(error) => {
                let _ = reply.send(
                    ids.into_values()
                        .map(|index| (index, Err(error.to_string())))
                        .collect(),
                );
                return Err(error.to_string());
            }
        };
        // Calls refused before dispatch also need a durable refusal,
        // while dispatched calls already carry their owner's receipt.
        for block in &results {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            else {
                continue;
            };
            if self.codemode_journaled.contains(tool_use_id) {
                continue;
            }
            let Some((name, args)) = batch_arguments.get(tool_use_id) else {
                continue;
            };
            let execution = if self.codemode_cancelled_calls.contains(tool_use_id) {
                ToolExecution::cancelled(
                    tool_use_id,
                    name,
                    ExecutionSource::Native,
                    ExecutionPhase::Queued,
                )
            } else {
                ToolExecution::denied(
                    tool_use_id,
                    name,
                    DenialReason::ActionFirewall {
                        message: content.clone(),
                    },
                )
            }
            .with_managed_policy(self.tool_executor.managed_policy_metadata());
            match self.begin_tool_operation(tool_use_id, name, args).await {
                Ok(operation) => {
                    self.record_tool_operation_outcome(operation, &execution)
                        .await;
                    self.complete_tool_operation(tool_use_id).await;
                }
                Err(error) => self.tool_executor.report_diagnostic(error),
            }
        }
        self.apply_tool_batch_end_extensions(&mut results);
        let responses = results
            .into_iter()
            .filter_map(|block| {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = block
                else {
                    return None;
                };
                let index = *ids.get(&tool_use_id)?;
                Some((
                    index,
                    self.codemode_response(&tool_use_id, content, is_error == Some(true)),
                ))
            })
            .collect();
        let _ = reply.send(responses);
        Ok(())
    }
}
