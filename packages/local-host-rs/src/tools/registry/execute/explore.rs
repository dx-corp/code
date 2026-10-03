//! Bounded exploration with the native hook pipeline for each operation.

use super::*;
use crate::hooks::{HookResult, IntegratedHookSystem};

impl ToolExecutor {
    /// Execute bounded local exploration while applying the native hook
    /// pipeline to every nested operation.
    pub(super) async fn execute_explore(
        &self,
        args: &serde_json::Value,
        hook_event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        cancel: Option<CancellationToken>,
        mut hooks: Option<&mut IntegratedHookSystem>,
    ) -> ToolResult {
        let operations = match args.get("operations").and_then(Value::as_array) {
            Some(operations) if !operations.is_empty() => operations,
            _ => {
                return ToolResult::failure(
                    "explore requires a non-empty operations array".to_string(),
                );
            }
        };
        if operations.len() > 8 {
            return ToolResult::failure("explore accepts at most 8 operations");
        }

        struct PreparedExploreOperation {
            tool_name: String,
            args: Value,
            call_id: String,
            extra_context: Option<String>,
            skip_reason: Option<String>,
        }

        let mut prepared = Vec::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            let operation_object = match operation.as_object() {
                Some(operation) => operation,
                None => return ToolResult::failure("Each explore operation must be an object"),
            };
            let sub_tool = operation_object
                .get("tool")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if !matches!(
                sub_tool.as_str(),
                "read" | "glob" | "grep" | "find" | "list" | "search" | "parallel_ripgrep" | "diff"
            ) {
                return ToolResult::failure(format!(
                    "Unsupported explore operation: {sub_tool}. Only local read/search tools are allowed"
                ));
            }
            let sub_args = match operation_object.get("args") {
                Some(Value::Object(args)) => Value::Object(args.clone()),
                _ => return ToolResult::failure("Each explore operation requires an args object"),
            };
            let sub_call_id = format!("{call_id}:explore:{index}");
            let mut effective_args = sub_args;
            let mut extra_context = None;
            let mut skip_reason = None;
            if let Some(hooks) = hooks.as_deref_mut() {
                match hooks.execute_pre_tool_use(&sub_tool, &sub_call_id, &effective_args) {
                    HookResult::Block { reason } => {
                        if let Some(tx) = hook_event_tx {
                            let _ = tx.send(FromAgent::HookBlocked {
                                call_id: sub_call_id.clone(),
                                tool: sub_tool.clone(),
                                reason: reason.clone(),
                            });
                        }
                        skip_reason = Some(format!("Tool blocked by hook: {reason}"));
                    }
                    HookResult::ModifyInput { new_input } => {
                        effective_args = new_input;
                    }
                    HookResult::InjectContext { context } => {
                        extra_context = Some(context);
                    }
                    HookResult::Continue => {}
                }
            }
            if skip_reason.is_none() {
                let missing = self.missing_required(&sub_tool, &effective_args);
                if !missing.is_empty() {
                    skip_reason = Some(format!(
                        "Missing required fields for tool '{}': {}",
                        sub_tool,
                        missing.join(", ")
                    ));
                }
            }
            prepared.push(PreparedExploreOperation {
                tool_name: sub_tool,
                args: effective_args,
                call_id: sub_call_id,
                extra_context,
                skip_reason,
            });
        }

        let executions = prepared.into_iter().map(|operation| {
            let cancel = cancel.clone();
            async move {
                if let Some(reason) = operation.skip_reason.clone() {
                    return (operation, ToolResult::failure(reason), false);
                }
                let result = Box::pin(self.execute_at_generation(
                    &operation.tool_name,
                    &operation.args,
                    None,
                    &operation.call_id,
                    generation,
                    cancel,
                ))
                .await;
                (operation, result, true)
            }
        });
        let results = futures::future::join_all(executions).await;

        let mut success = true;
        let output = results
            .into_iter()
            .enumerate()
            .map(|(index, (operation, mut result, executed))| {
                if executed {
                    if let Some(hooks) = hooks.as_deref_mut() {
                        let post_output = result.error.as_ref().map_or_else(
                            || result.output.clone(),
                            |error| {
                                if result.output.is_empty() {
                                    error.clone()
                                } else {
                                    format!("{}\n{error}", result.output)
                                }
                            },
                        );
                        // Sub-operations of one tool call are timed as a
                        // group by the caller, not individually, so this
                        // reports 0 rather than inventing a per-operation
                        // duration.
                        let _ = hooks.execute_post_tool_use(
                            &operation.tool_name,
                            &operation.call_id,
                            &operation.args,
                            &post_output,
                            !result.success,
                            0,
                        );
                    }
                }
                if let Some(context) = operation.extra_context {
                    result.output = if result.output.is_empty() {
                        context
                    } else {
                        format!("{}\n\n{context}", result.output)
                    };
                }
                success &= result.success;
                serde_json::json!({
                    "index": index,
                    "success": result.success,
                    "output": result.output,
                    "error": result.error,
                })
            })
            .collect::<Vec<_>>();
        let output = serde_json::to_string_pretty(&output)
            .unwrap_or_else(|_| "explore results could not be serialized".to_string());
        if success {
            ToolResult::success(output)
        } else {
            ToolResult {
                success: false,
                output,
                error: Some("One or more explore operations failed".to_string()),
                details: None,
            }
        }
    }
}
