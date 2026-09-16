use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::super::{FromAgent, ToolExecution};
use crate::agent::native_host::{
    NativeExecutionHostHandle, NativeReadOnlyToolCall, NativeToolAnnotations,
};

#[derive(Debug)]
pub(super) struct QueuedReadOnlyToolExecution {
    pub(super) call_id: String,
    pub(super) tool_name: String,
    pub(super) args: serde_json::Value,
    pub(super) safe_args: serde_json::Value,
    // Resolve before scheduling so tools receive concrete command arguments.
    pub(super) resolved_args: serde_json::Value,
    pub(super) extra_context: Option<String>,
}

fn is_known_native_read_only_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read"
            | "glob"
            | "grep"
            | "diff"
            | "list"
            | "find"
            | "search"
            | "repository_symbols"
            | "parallel_ripgrep"
            | "websearch"
            | "web_fetch"
            | "webfetch"
            | "read_image"
            | "mcp_list_resources"
            | "mcp_list_prompts"
            | "mcp_read_resource"
            | "mcp_get_prompt"
            | "vscode_get_diagnostics"
            | "vscode_get_definition"
            | "vscode_find_references"
            | "vscode_read_file_range"
            | "jetbrains_get_diagnostics"
            | "jetbrains_get_definition"
            | "jetbrains_find_references"
            | "jetbrains_read_file_range"
    )
}

pub(super) fn is_native_parallel_read_only_tool_call(
    tool_name: &str,
    requires_approval: bool,
    annotations: Option<&NativeToolAnnotations>,
    explicit_inline_read_only: bool,
) -> bool {
    if requires_approval {
        return false;
    }

    let tool_key = tool_name.to_lowercase();
    if is_known_native_read_only_tool(&tool_key) {
        return true;
    }

    if tool_key.starts_with("mcp__") {
        return annotations.is_some_and(|annotations| {
            annotations.read_only_hint == Some(true) && annotations.destructive_hint != Some(true)
        });
    }

    explicit_inline_read_only
}

pub(super) fn is_explicit_inline_read_only_tool(
    tool_name: &str,
    tool_executor: &NativeExecutionHostHandle,
) -> bool {
    tool_executor.is_explicit_inline_read_only_tool(tool_name)
}

pub(super) async fn execute_native_read_only_tool_wave(
    tool_executor: &NativeExecutionHostHandle,
    event_tx: &mpsc::UnboundedSender<FromAgent>,
    pending: &[QueuedReadOnlyToolExecution],
    cancel_token: Option<CancellationToken>,
) -> HashMap<String, ToolExecution> {
    let calls: Vec<NativeReadOnlyToolCall> = pending
        .iter()
        .map(|call| NativeReadOnlyToolCall {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            args: call.resolved_args.clone(),
        })
        .collect();
    tool_executor
        .execute_read_only_wave(&calls, event_tx, cancel_token)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::native_host::NativeToolAnnotations;

    #[test]
    fn test_native_parallel_read_only_classifier_preconditions() {
        assert!(is_native_parallel_read_only_tool_call(
            "read", false, None, false
        ));
        assert!(is_native_parallel_read_only_tool_call(
            "repository_symbols",
            false,
            None,
            false
        ));
        assert!(!is_native_parallel_read_only_tool_call(
            "read_probe",
            false,
            None,
            false
        ));
        assert!(is_native_parallel_read_only_tool_call(
            "read_probe",
            false,
            None,
            true
        ));

        assert!(!is_native_parallel_read_only_tool_call(
            "write", true, None, true
        ));
        assert!(!is_native_parallel_read_only_tool_call(
            "bash", false, None, false
        ));

        let read_only_mcp = NativeToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            ..Default::default()
        };
        assert!(is_native_parallel_read_only_tool_call(
            "mcp__repo__inspect",
            false,
            Some(&read_only_mcp),
            false
        ));

        let destructive_mcp = NativeToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(true),
            ..Default::default()
        };
        assert!(!is_native_parallel_read_only_tool_call(
            "mcp__repo__mutate",
            false,
            Some(&destructive_mcp),
            true
        ));
    }
}
