//! Shared tool admission and execution for provider batches and script waves.

use super::*;

impl NativeAgentRunner {
    pub(super) async fn execute_tool_batch(
        &mut self,
        pending_tool_calls: Vec<(String, String, Value, Option<String>)>,
        scripted: bool,
    ) -> Result<(Vec<ContentBlock>, Vec<PendingMessage>)> {
        let mut tool_results: Vec<ContentBlock> = Vec::new();
        let mut deferred_steering: Vec<PendingMessage> = Vec::new();
        let mut deferred_tool_calls: Vec<DeferredToolCall> = Vec::new();
        let mut remaining_tool_calls: Vec<(String, String, serde_json::Value, Option<String>)> =
            Vec::new();
        let mut pending_tool_calls_iter = pending_tool_calls.into_iter();
        let mut pending_read_only_tool_calls: Vec<QueuedReadOnlyToolExecution> = Vec::new();
        let mut processed_any_tool = false;

        while let Some((call_id, tool_name, args, parse_error)) = pending_tool_calls_iter.next() {
            self.tool_response_coordinator.remove_cancelled(&call_id);
            if processed_any_tool {
                if !scripted && self.drain_pending_commands().await {
                    if !scripted && !tool_results.is_empty() {
                        self.messages_mut().push(Message {
                            role: Role::User,
                            content: MessageContent::Blocks(std::mem::take(&mut tool_results)),
                        });
                    }
                    if !scripted {
                        self.repair_orphaned_tool_calls();
                    }
                    return Err(anyhow::anyhow!("Request cancelled"));
                }
                if !scripted {
                    deferred_steering = self.dequeue_next_turn_messages(false);
                }
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
            let (args, rewrote_empty_bash) = normalize_post_hook_tool_args(&tool_name, args);
            if rewrote_empty_bash {
                let _ = self.event_tx.send(FromAgent::Status {
                    message:
                        "Received empty bash tool call; auto-filled command as \"pwd\" to proceed."
                            .to_string(),
                });
            }
            let missing = self.missing_required_tool_args(&tool_name, &args);
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
            let firewall_verdict = if is_external_tool || tool_key == agent_codemode::TOOL_NAME {
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
                    .hook_permission_request(&tool_name, &call_id, &args, "tool requires approval")
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

            let deferred_disposition =
                deferred_tool_call_disposition(requires_approval, !deferred_tool_calls.is_empty());
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
            invalidate_cache_after_serial_tool(&self.tool_executor, &tool_name_for_cache, true);
        }

        self.drain_read_only_tool_calls(&mut pending_read_only_tool_calls, &mut tool_results)
            .await?;

        // Every ToolCall event in this batch has been emitted. Now
        // execute the deferred suffix in model order, awaiting gated
        // decisions in FIFO order. Responses that arrive out of order
        // are stashed by wait_for_tool_response until their turn.
        let mut deferred_tool_calls_iter = std::mem::take(&mut deferred_tool_calls).into_iter();
        if self.take_active_operation_interruption() {
            let cancelled_ids = cancel_deferred_suffix(
                &self.event_tx,
                deferred_tool_calls_iter.by_ref(),
                &mut tool_results,
                self.tool_executor.managed_policy_metadata(),
            );
            self.tool_response_coordinator
                .discard_cancelled(&cancelled_ids);
            if self.codemode_cancel.is_some() {
                self.codemode_cancelled_calls.extend(cancelled_ids);
            }
        }
        while let Some(deferred_call) = deferred_tool_calls_iter.next() {
            match deferred_call {
                DeferredToolCall::AwaitApproval(mut call) => {
                    let approval_cancel = self
                        .codemode_cancel
                        .as_ref()
                        .unwrap_or(&self.shutdown_token)
                        .child_token();
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
                            if self.codemode_cancel.is_some() {
                                self.codemode_cancelled_calls.extend(cancelled_ids);
                            }
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
                        let missing = self.missing_required_tool_args(&call.tool_name, &args);
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
                        if let Some(reason) = approved_input_change_rejection(&call.args, &args) {
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
                            .with_managed_policy(self.tool_executor.managed_policy_metadata())
                        })
                    } else {
                        Some(
                            ToolExecution::denied(
                                &call.call_id,
                                &call.tool_name,
                                DenialReason::User,
                            )
                            .with_managed_policy(self.tool_executor.managed_policy_metadata()),
                        )
                    };
                    let tool_name_for_cache = call.tool_name.clone();
                    let result_block = self.finalize_tool_call_result(call, approved, result).await;
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
                    let missing = self.missing_required_tool_args(&call.tool_name, &args);
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
                            .execute_tool(&call.tool_name, &execution_args, &call.call_id, None)
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
                if self.codemode_cancel.is_some() {
                    self.codemode_cancelled_calls.extend(cancelled_ids);
                }
                break;
            }
        }

        if deferred_steering.is_empty() {
            if !scripted && self.drain_pending_commands().await {
                if !scripted && !tool_results.is_empty() {
                    self.messages_mut().push(Message {
                        role: Role::User,
                        content: MessageContent::Blocks(std::mem::take(&mut tool_results)),
                    });
                }
                if !scripted {
                    self.repair_orphaned_tool_calls();
                }
                return Err(anyhow::anyhow!("Request cancelled"));
            }
            if !scripted {
                deferred_steering = self.dequeue_next_turn_messages(false);
            }
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

        Ok((tool_results, deferred_steering))
    }
}
