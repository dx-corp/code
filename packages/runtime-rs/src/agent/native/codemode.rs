//! Native script composition over the existing per-call execution boundary.

use super::*;

pub(super) fn register(
    tools: &mut HashMap<String, ToolDefinition>,
    allowed: Option<&HashSet<String>>,
) {
    if allowed.is_none_or(|names| names.contains(agent_codemode::TOOL_NAME)) {
        tools.insert(
            agent_codemode::TOOL_NAME.to_owned(),
            ToolDefinition {
                tool: Tool::new(agent_codemode::TOOL_NAME, agent_codemode::DESCRIPTION)
                    .with_schema(agent_codemode::schema()),
                requires_approval: false,
            },
        );
    }
}

pub(super) fn output_schema(tool: &Tool, vault: &CredentialVault) -> Option<Value> {
    // MCP outputSchema describes structuredContent, while this owner returns
    // flattened content. Declaring that JavaScript result would be misleading.
    if tool.name.starts_with("mcp__") {
        return None;
    }
    tool.output_schema
        .as_ref()
        .map(|schema| vault.vault_in_json(schema))
}

pub(super) fn vault_script_json(value: &Value, vault: &CredentialVault) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) => Value::String(vault.vault_in_text(text)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| vault_script_json(value, vault))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(values) => {
            let mut safe = serde_json::Map::new();
            for (key, value) in values {
                if safe
                    .insert(vault.vault_in_text(key), vault_script_json(value, vault)?)
                    .is_some()
                {
                    return Err("Script state keys collide after credential sanitization".into());
                }
            }
            Value::Object(safe)
        }
        _ => value.clone(),
    })
}

impl NativeAgentRunner {
    pub(super) fn emit_codemode_progress(&self) {
        if let Some(call_id) = &self.codemode_parent_call_id {
            let _ = self.event_tx.send(FromAgent::CodeModeProgress {
                call_id: call_id.clone(),
                children: self.codemode_progress.values().cloned().collect(),
            });
        }
    }

    pub(super) fn queue_codemode_images(
        &mut self,
        call_id: &str,
        images: &[maestro_runtime_contracts::tool_operation::CodeModeImage],
    ) {
        for (index, image) in images.iter().enumerate() {
            let owner = maestro_ai::ToolImageOwner {
                call_id: call_id.to_owned(),
                index: index as u32,
            };
            let already_projected = self.semantic_continuation.as_ref().is_some_and(|record| record.projected_tool_images.contains(&owner))
                || self.messages.iter().any(|message| match &message.content {
                MessageContent::Blocks(blocks) => blocks.iter().any(|block| matches!(block,
                    ContentBlock::Image { source: ImageSource::Base64 { owner: Some(existing), .. } } if existing == &owner)),
                _ => false,
            });
            if !already_projected
                && !self
                    .codemode_projected_images
                    .iter()
                    .any(|(existing, _)| existing == &owner)
            {
                self.codemode_projected_images.push((owner, image.clone()));
            }
        }
    }

    pub(super) fn project_codemode_images(&mut self) {
        let images = std::mem::take(&mut self.codemode_projected_images);
        if !images.is_empty() {
            self.messages_mut().push(Message {
                role: Role::User,
                content: MessageContent::Blocks(
                    images
                        .into_iter()
                        .map(|(owner, image)| ContentBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: image.mime_type,
                                data: image.data,
                                owner: Some(owner),
                            },
                        })
                        .collect(),
                ),
            });
        }
    }

    /// Commit new script state/media only after the existing final projection
    /// controls accept the outer result, and before its terminal event.
    pub(super) async fn finalize_codemode_attachments(
        &mut self,
        execution: &mut ToolExecution,
        reported_error: bool,
    ) -> Result<(), String> {
        if !self
            .codemode_pending_operation
            .as_ref()
            .is_some_and(|operation| {
                operation.call_id == execution.receipt.call_id
                    && execution
                        .receipt
                        .tool_name
                        .eq_ignore_ascii_case(agent_codemode::TOOL_NAME)
            })
        {
            return Ok(());
        }
        let operation = self.codemode_pending_operation.take().unwrap();
        if reported_error {
            execution.codemode_store = None;
            execution.images.clear();
        }
        let persisted = self
            .try_record_tool_operation_outcome(operation, execution)
            .await;
        if persisted.is_ok() {
            if let Some(commit) = &execution.codemode_store {
                self.codemode_store = commit.clone();
            }
            self.queue_codemode_images(&execution.receipt.call_id, &execution.images);
        } else {
            execution.codemode_store = None;
            execution.images.clear();
        }
        let mut result = execution.to_legacy();
        if reported_error || persisted.is_err() {
            result.success = false;
        }
        let _ = self.event_tx.send(FromAgent::ToolOutput {
            call_id: execution.receipt.call_id.clone(),
            content: execution.raw_content(),
        });
        let _ = self.event_tx.send(FromAgent::ToolEnd {
            call_id: execution.receipt.call_id.clone(),
            success: result.success,
            result: Some(result),
            receipt: Some(execution.receipt.clone()),
        });
        persisted
    }

    /// Runtime-owned tools and caller schemas cannot depend on a concrete
    /// local registry to check their required fields.
    pub(super) fn missing_required_tool_args(&self, name: &str, args: &Value) -> Vec<String> {
        let mut missing = self.tool_executor.missing_required(name, args);
        // Native hosts own validation, including supported argument aliases.
        // Runtime and caller-owned tools need their declared required fields
        // checked here because they do not use the native host registry.
        if let Some(definition) = self.tools.get(&name.to_ascii_lowercase()).filter(|_| {
            name.eq_ignore_ascii_case(agent_codemode::TOOL_NAME)
                || self.external_tools.contains(&name.to_ascii_lowercase())
        }) {
            if let Some(required) = definition
                .tool
                .input_schema
                .get("required")
                .and_then(Value::as_array)
            {
                for field in required.iter().filter_map(Value::as_str) {
                    if args.get(field).is_none_or(Value::is_null)
                        && !missing.iter().any(|existing| existing == field)
                    {
                        missing.push(field.to_owned());
                    }
                }
            }
        }
        if name.eq_ignore_ascii_case(agent_codemode::TOOL_NAME)
            && args
                .get("code")
                .and_then(Value::as_str)
                .is_none_or(|code| code.trim().is_empty() || code.len() > 65536)
            && !missing.iter().any(|field| field == "code")
        {
            missing.push("code (1 to 65536 bytes)".to_owned());
        }
        missing
    }

    pub(super) fn codemode_catalog(&self) -> Vec<agent_codemode::Tool> {
        let excluded = self
            .runtime_audit
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .excluded_context_tools
            .clone();
        let mut tools = self
            .tools
            .values()
            .filter(|definition| {
                let name = definition.tool.name.to_ascii_lowercase();
                name != agent_codemode::TOOL_NAME
                    && name != "ask_user"
                    && !excluded.contains(&name)
                    && (name != classifier::TOOL_NAME
                        || (self.client.is_some() && !self.model_route.uses_app_server()))
                    && tool_is_visible_to_model(
                        &name,
                        self.goal_tools_visible,
                        self.include_ide_tools,
                    )
                    && tool_search_profile_allows(
                        self.tool_profile,
                        &name,
                        &self.explicitly_allowed_tools,
                    )
            })
            .map(|definition| agent_codemode::Tool {
                name: definition.tool.name.clone(),
                description: self
                    .credential_vault
                    .vault_in_text(&definition.tool.description),
                schema: self
                    .credential_vault
                    .vault_in_json(&definition.tool.input_schema),
                output_schema: output_schema(&definition.tool, &self.credential_vault),
                namespace: Some(if definition.tool.name.starts_with("mcp__") {
                    definition
                        .tool
                        .name
                        .split("__")
                        .take(2)
                        .collect::<Vec<_>>()
                        .join("__")
                } else if self
                    .external_tools
                    .contains(&definition.tool.name.to_ascii_lowercase())
                {
                    "client".into()
                } else {
                    "native".into()
                }),
                model_operation: (definition.tool.name == classifier::TOOL_NAME
                    && !self.external_tools.contains(classifier::TOOL_NAME))
                .then_some(agent_codemode::ModelOperation::Classify),
                model_binding: (definition.tool.name == classifier::TOOL_NAME
                    && !self.external_tools.contains(classifier::TOOL_NAME))
                .then(|| agent_codemode::ModelBinding {
                    owner: "maestro-native".into(),
                    provider: self
                        .client
                        .as_ref()
                        .map(|client| client.provider_name().to_owned())
                        .unwrap_or_default(),
                    model: self.config.model.clone(),
                }),
            })
            .collect::<Vec<_>>();
        tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub(super) async fn execute_codemode(&mut self, args: &Value, call_id: &str) -> ToolExecution {
        let _ = self.event_tx.send(FromAgent::ToolStart {
            call_id: call_id.to_owned(),
        });
        let started = Instant::now();
        self.codemode_indeterminate = false;
        self.codemode_pending_store = None;
        self.codemode_pending_images.clear();
        self.codemode_progress.clear();
        let result = self.run_codemode(args, call_id).await;
        let result = if self.codemode_indeterminate {
            let emitted = result.error.as_deref().unwrap_or(&result.output);
            ToolResult::failure(format!("{emitted}\nA nested tool has an unknown outcome. Reconcile its receipt before retrying; script completion does not establish effect completion."))
                .with_details(json!({"remoteOutcome":"unknown","requiresReconciliation":true,"retryable":false}))
        } else {
            result
        };
        let mut execution = ToolExecution::from_legacy(
            call_id,
            agent_codemode::TOOL_NAME,
            ExecutionSource::Native,
            result,
        )
        .with_duration(started.elapsed().as_millis() as u64)
        .with_managed_policy(self.tool_executor.managed_policy_metadata());
        if matches!(execution.outcome, ToolOutcome::Succeeded { .. }) {
            execution.codemode_store = self.codemode_pending_store.take();
            execution.images = std::mem::take(&mut self.codemode_pending_images);
        } else {
            self.codemode_pending_store = None;
            self.codemode_pending_images.clear();
        }
        execution
    }

    async fn run_codemode(&mut self, args: &Value, call_id: &str) -> ToolResult {
        if self.codemode_cancel.is_some() {
            return ToolResult::failure(
                "Recursive codemode is unavailable; compose calls in the current script.",
            );
        }
        if !args
            .as_object()
            .is_some_and(|object| object.len() == 1 && object.contains_key("code"))
        {
            return ToolResult::failure("codemode accepts exactly one field: code");
        }
        let Some(code) = args.get("code").and_then(Value::as_str) else {
            return ToolResult::failure("codemode requires a code string");
        };
        let catalog = self.codemode_catalog();
        let admitted = catalog
            .iter()
            .map(|tool| tool.name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let cancel = self.shutdown_token.child_token();
        let deadline_cancel = cancel.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            deadline_cancel.cancel();
        });
        self.codemode_journaled.clear();
        self.codemode_cancelled_calls.clear();
        self.codemode_indeterminate = false;
        self.codemode_cancel = Some(cancel.clone());
        self.codemode_parent_call_id = Some(call_id.to_owned());
        let mut session = agent_codemode::Session::start_with_store(
            code.to_owned(),
            catalog,
            &cancel,
            Duration::from_secs(60),
            self.codemode_store.values.clone(),
        );
        let outcome = loop {
            self.set_active_tool_cancel_token(Some(cancel.clone()), false);
            let event = session.next().await;
            self.set_active_tool_cancel_token(None, false);
            match event {
                Some(agent_codemode::Event::Done(report)) => {
                    let content = self.credential_vault.vault_in_text(&report.content());
                    if report.error.is_none()
                        && !self.codemode_indeterminate
                        && !cancel.is_cancelled()
                    {
                        if !report.store_writes.is_empty() {
                            let mut values = self.codemode_store.values.clone();
                            if let Err(error) = report.store_writes.apply(&mut values) {
                                break ToolResult::failure(error);
                            }
                            // Vault keys and nested JSON keys as well as values before persistence.
                            let values = match serde_json::to_value(values)
                                .map_err(|error| error.to_string())
                                .and_then(|value| vault_script_json(&value, &self.credential_vault))
                                .and_then(|value| {
                                    serde_json::from_value::<agent_codemode::Store>(value)
                                        .map_err(|error| error.to_string())
                                }) {
                                Ok(values) => values,
                                Err(error) => break ToolResult::failure(error),
                            };
                            if let Err(error) = agent_codemode::validate_store(&values) {
                                break ToolResult::failure(error);
                            }
                            let Some(revision) = self.codemode_store.revision.checked_add(1) else {
                                break ToolResult::failure("Script state revision exhausted");
                            };
                            self.codemode_pending_store = Some(
                                maestro_runtime_contracts::tool_operation::CodeModeStoreCommit {
                                    revision,
                                    values,
                                },
                            );
                        }
                        self.codemode_pending_images = report
                            .blocks
                            .into_iter()
                            .filter_map(|block| match block {
                                agent_codemode::OutputBlock::Image { mime_type, data } => {
                                    Some(maestro_runtime_contracts::tool_operation::CodeModeImage {
                                        mime_type,
                                        data,
                                    })
                                }
                                agent_codemode::OutputBlock::Text { .. } => None,
                            })
                            .collect();
                    }
                    break if report.error.is_some() {
                        ToolResult::failure(content)
                    } else {
                        ToolResult::success(content)
                    };
                }
                Some(agent_codemode::Event::Calls { calls, reply }) => {
                    if cancel.is_cancelled() || self.take_active_operation_interruption() {
                        cancel.cancel();
                        let _ = reply.send(
                            calls
                                .into_iter()
                                .map(|call| (call.index, Err("Script cancelled".to_owned())))
                                .collect(),
                        );
                        break ToolResult::failure(
                            "Script cancelled; calls already executed retain their receipts.",
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
                            break ToolResult::failure(error.to_string());
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
                                if is_error == Some(true) {
                                    Err(content)
                                } else {
                                    Ok(serde_json::from_str(&content)
                                        .unwrap_or(Value::String(content)))
                                },
                            ))
                        })
                        .collect();
                    let _ = reply.send(responses);
                }
                None => {
                    break ToolResult::failure("Script worker closed without a completion result");
                }
            }
        };
        timer.abort();
        let was_cancelled = cancel.is_cancelled();
        cancel.cancel();
        self.codemode_cancel = None;
        self.codemode_parent_call_id = None;
        if was_cancelled {
            outcome.with_details(json!({"cancelled":true}))
        } else {
            outcome
        }
    }
}
