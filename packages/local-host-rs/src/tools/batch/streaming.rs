//! Bounded cancellable batch execution with owner completion delivery.

use super::*;

impl BatchExecutor {
    pub(crate) async fn execute_with_cancel_at_generation(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        cancel_token: CancellationToken,
        generation: u64,
    ) -> Vec<BatchToolResult> {
        self.execute_with_cancel_and_completions_at_generation(
            calls,
            event_tx,
            cancel_token,
            generation,
            None,
            None,
        )
        .await
    }

    pub(crate) async fn execute_with_cancel_and_completions_at_generation(
        &self,
        calls: Vec<BatchToolCall>,
        event_tx: Option<mpsc::UnboundedSender<FromAgent>>,
        cancel_token: CancellationToken,
        generation: u64,
        completions: Option<mpsc::UnboundedSender<(String, ToolExecution)>>,
        shared_permits: Option<Arc<Semaphore>>,
    ) -> Vec<BatchToolResult> {
        let total = calls.len();
        if calls.is_empty() {
            return Vec::new();
        }

        if !self.config.continue_on_error {
            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let _ = tx.send(FromAgent::BatchStart { total });
                }
            }

            let mut results = Vec::with_capacity(total);
            let mut failed = false;
            let mut cancelled = false;

            for call in calls {
                if cancelled || cancel_token.is_cancelled() {
                    cancelled = true;
                    let result = BatchToolResult::cancelled(
                        call.call_id,
                        call.tool_name,
                        ExecutionPhase::Queued,
                    );
                    emit_batch_completion(event_tx.as_ref(), self.config.emit_events, &result);
                    results.push(result);
                    continue;
                }

                if failed {
                    results.push(BatchToolResult {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        result: ToolResult::failure("Skipped due to previous error in batch"),
                        execution: None,
                    });
                    continue;
                }

                if self.config.emit_events {
                    if let Some(tx) = event_tx.as_ref() {
                        let _ = tx.send(FromAgent::ToolStart {
                            call_id: call.call_id.clone(),
                        });
                    }
                }
                // Forward `cancel_token` into the per-call `cancel` slot too
                // (not just this `select!`): a call whose own tool (bash,
                // web_fetch, an inline tool) never observes the token
                // relies solely on the abandoned future's `Drop` for
                // cleanup -- which for a child process only kills the
                // immediate shell (`kill_on_drop`), not any descendants it
                // spawned.
                //
                // `call_id`/`tool_name` are cloned up front (rather than
                // borrowed from `call`) so they can still be moved into the
                // result below after `call_execution` -- which borrows
                // `call.args` -- is done being polled.
                let call_id = call.call_id.clone();
                let tool_name = call.tool_name.clone();
                // Scoped so `call_execution` (which borrows `call_id`,
                // `tool_name`, and `call.args`) is dropped before they are
                // moved into `BatchToolResult::from_execution` below.
                let execution = {
                    let call_execution = self.executor.execute_with_receipt_at_generation(
                        &tool_name,
                        &call.args,
                        None,
                        &call_id,
                        generation,
                        Some(cancel_token.clone()),
                    );
                    tokio::pin!(call_execution);
                    tokio::select! {
                        execution = &mut call_execution => execution,
                        () = cancel_token.cancelled() => {
                            cancelled = true;
                            // See `BATCH_CANCELLATION_GRACE_PERIOD`: give
                            // the call's own cancellation-aware cleanup
                            // (now that it holds the same token) a bounded
                            // window to run to completion instead of
                            // dropping it -- and whatever cleanup it was
                            // mid-way through -- the instant `cancelled()`
                            // resolves.
                            match tokio::time::timeout(
                                BATCH_CANCELLATION_GRACE_PERIOD,
                                &mut call_execution,
                            )
                            .await
                            {
                                Ok(execution) => execution,
                                Err(_) => ToolExecution::cancelled(
                                    &call_id,
                                    &tool_name,
                                    ExecutionSource::Native,
                                    ExecutionPhase::Running,
                                )
                                .with_managed_policy(crate::safety::managed_policy_metadata()),
                            }
                        }
                    }
                };
                let result = BatchToolResult::from_execution(call_id, tool_name, execution);

                if !result.result.success {
                    failed = true;
                }
                emit_batch_completion(event_tx.as_ref(), self.config.emit_events, &result);
                results.push(result);
            }

            if let Some(ref tx) = event_tx {
                if self.config.emit_events {
                    let successes = results.iter().filter(|r| r.result.success).count();
                    let failures = results.len() - successes;
                    let _ = tx.send(FromAgent::BatchEnd {
                        total: results.len(),
                        successes,
                        failures,
                    });
                }
            }

            return results;
        }

        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let _ = tx.send(FromAgent::BatchStart { total });
            }
        }

        let call_metadata: Vec<(String, String)> = calls
            .iter()
            .map(|call| (call.call_id.clone(), call.tool_name.clone()))
            .collect();
        let semaphore =
            shared_permits.unwrap_or_else(|| Arc::new(Semaphore::new(self.config.max_concurrency)));
        let mut task_set: JoinSet<(usize, BatchToolResult)> = JoinSet::new();
        let mut result_slots: Vec<Option<BatchToolResult>> =
            std::iter::repeat_with(|| None).take(total).collect();
        let mut started = vec![false; total];
        let mut cancelled = cancel_token.is_cancelled();

        if !cancelled {
            for (index, call) in calls.into_iter().enumerate() {
                let permit = tokio::select! {
                    permit = semaphore.clone().acquire_owned() => permit,
                    () = cancel_token.cancelled() => {
                        cancelled = true;
                        break;
                    }
                };

                let Ok(permit) = permit else {
                    cancelled = true;
                    break;
                };

                if self.config.emit_events {
                    if let Some(tx) = event_tx.as_ref() {
                        let _ = tx.send(FromAgent::ToolStart {
                            call_id: call.call_id.clone(),
                        });
                    }
                }
                started[index] = true;
                let executor = Arc::clone(&self.executor);
                let call_id = call.call_id;
                let tool_name = call.tool_name;
                let args = call.args;
                // Forward this call's own copy of the shared token so its
                // tool's cancellation-aware cleanup (process-tree kill,
                // etc.) has something to observe; see
                // `drain_with_grace_period_then_abort` for why the task set
                // gives it a bounded window to actually run before this
                // task is hard-aborted.
                let call_cancel_token = cancel_token.clone();

                let completions = completions.clone();
                task_set.spawn(async move {
                    let execution = executor
                        .execute_with_receipt_at_generation(
                            &tool_name,
                            &args,
                            None,
                            &call_id,
                            generation,
                            Some(call_cancel_token),
                        )
                        .await;

                    if let Some(completions) = completions {
                        let _ = completions.send((call_id.clone(), execution.clone()));
                    }
                    drop(permit);

                    (
                        index,
                        BatchToolResult::from_execution(call_id, tool_name, execution),
                    )
                });
            }
        }

        if cancelled {
            drain_with_grace_period_then_abort(&mut task_set, &mut result_slots).await;
        }

        while !task_set.is_empty() {
            if cancelled {
                record_joined_batch_result(task_set.join_next().await, &mut result_slots);
                continue;
            }

            tokio::select! {
                result = task_set.join_next() => {
                    record_joined_batch_result(result, &mut result_slots);
                }
                () = cancel_token.cancelled() => {
                    cancelled = true;
                    drain_with_grace_period_then_abort(&mut task_set, &mut result_slots).await;
                }
            }
        }

        for (index, slot) in result_slots.iter_mut().enumerate() {
            if slot.is_none() {
                let (call_id, tool_name) = &call_metadata[index];
                let phase = if started[index] {
                    ExecutionPhase::Running
                } else {
                    ExecutionPhase::Queued
                };
                *slot = Some(BatchToolResult::cancelled(
                    call_id.clone(),
                    tool_name.clone(),
                    phase,
                ));
            }
        }

        let results: Vec<BatchToolResult> = result_slots.into_iter().flatten().collect();

        if self.config.emit_events {
            for result in &results {
                emit_batch_completion(event_tx.as_ref(), true, result);
            }
        }

        if let Some(ref tx) = event_tx {
            if self.config.emit_events {
                let successes = results.iter().filter(|r| r.result.success).count();
                let failures = results.len() - successes;
                let _ = tx.send(FromAgent::BatchEnd {
                    total: results.len(),
                    successes,
                    failures,
                });
            }
        }

        results
    }
}
