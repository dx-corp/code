use crate::chat_output::ChatEventWriter;
use crate::model_catalog::{available_models, resolve_model};
use crate::{AppState, ChatRequest, PendingToolResponseOwner, now_millis, now_rfc3339};
use maestro_runtime::{TokenUsage, ToolResult};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

pub(crate) const CODEX_SUBAGENT_WORK_GRAPH_SCHEMA: &str =
    "evalops.maestro.codex.subagent-workgraph.v1";
static CODEX_BRIDGE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static CODEX_HEADLESS_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn codex_app_server_model_id(model: &str) -> Option<String> {
    let trimmed = model.trim();
    let (provider, model_id) = trimmed.split_once('/')?;
    if provider != "openai-codex" {
        return None;
    }
    let model_id = model_id.trim();
    (!model_id.is_empty()).then(|| model_id.to_string())
}

pub(crate) async fn usage_provider_model(
    _chat: &ChatRequest,
    state: &AppState,
    agent_model: &str,
) -> (String, String) {
    // Account against the immutable model selected for this execution. The
    // user's current selection can change while a turn is running.
    if let Some((provider, model)) = agent_model.split_once('/') {
        return (provider.to_string(), model.to_string());
    }

    let registry = available_models(&state.config).await;
    resolve_model(agent_model, &registry)
        .map(|model| (model.provider, model.id))
        .unwrap_or_else(|| ("unknown".to_string(), agent_model.to_string()))
}

pub(crate) async fn record_usage_entry(
    state: &AppState,
    session_id: Option<&str>,
    provider: &str,
    model: &str,
    usage: Option<&TokenUsage>,
) {
    let Some(usage) = usage else {
        return;
    };
    let _persist = state.usage_persist_lock.lock().await;
    let path = &state.config.usage_file_path;
    persist_usage_entry(path, session_id, provider, model, usage).await;
}

async fn persist_usage_entry(
    path: &Path,
    session_id: Option<&str>,
    provider: &str,
    model: &str,
    usage: &TokenUsage,
) {
    let mut entries = tokio::fs::read_to_string(path)
        .await
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(&raw).ok())
        .unwrap_or_default();
    let mut entry = serde_json::json!({
        "timestamp": now_millis(),
        "provider": provider,
        "model": model,
        "tokensInput": usage.input_tokens,
        "tokensOutput": usage.output_tokens,
        "tokensCacheRead": usage.cache_read_tokens,
        "tokensCacheWrite": usage.cache_write_tokens
    });
    if let Value::Object(cost) =
        serde_json::to_value(crate::usage_cost::price_usage(provider, model, usage))
            .expect("usage cost contains only finite numbers")
    {
        entry
            .as_object_mut()
            .expect("usage entry is an object")
            .extend(cost);
    }
    if let Some(session_id) = session_id {
        entry["sessionId"] = Value::String(session_id.to_string());
    }
    entries.push(entry);
    if entries.len() > 10_000 {
        entries.drain(..entries.len() - 10_000);
    }
    if let Some(parent) = path.parent() {
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return;
        }
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&entries) {
        let _ = tokio::fs::write(path, bytes).await;
    }
}

#[cfg(test)]
#[path = "codex_bridge_usage_tests.rs"]
mod codex_bridge_usage_tests;

fn codex_app_server_cli_path() -> PathBuf {
    env::var("MAESTRO_CODEX_APP_SERVER_CLI")
        .map(PathBuf::from)
        .or_else(|_| env::current_exe())
        .unwrap_or_else(|_| PathBuf::from("maestro"))
}

#[cfg(test)]
pub(crate) fn codex_app_server_cli_path_from_start_dir(start_dir: &Path) -> PathBuf {
    let mut package_root_candidate = None;
    for dir in start_dir.ancestors() {
        let cli_path = dir.join("bin/maestro");
        if cli_path.exists() {
            return cli_path;
        }
        if package_root_candidate.is_none() && dir.join("package.json").exists() {
            package_root_candidate = Some(cli_path);
        }
    }
    package_root_candidate.unwrap_or_else(|| start_dir.join("bin/maestro"))
}

fn codex_bridge_command(cli_path: &Path) -> Command {
    #[cfg(test)]
    if cli_path.extension().and_then(|value| value.to_str()) == Some("js") {
        let mut command = Command::new("node");
        command.arg(cli_path);
        return command;
    }
    Command::new(cli_path)
}

fn codex_app_server_timeout() -> Duration {
    Duration::from_millis(
        env::var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(240_000),
    )
}

fn codex_app_server_shutdown_timeout() -> Duration {
    Duration::from_millis(
        env::var("MAESTRO_CODEX_APP_SERVER_SHUTDOWN_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1_000),
    )
}

fn codex_app_server_sandbox_mode() -> Option<String> {
    let bridge_override = env::var("MAESTRO_CODEX_APP_SERVER_SANDBOX").ok();
    let inherited = env::var("MAESTRO_SANDBOX_MODE").ok();
    codex_app_server_sandbox_mode_from_values(bridge_override.as_deref(), inherited.as_deref())
}

pub(crate) fn codex_app_server_sandbox_mode_from_values(
    bridge_override: Option<&str>,
    inherited: Option<&str>,
) -> Option<String> {
    bridge_override
        .or(inherited)
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .filter(|mode| !matches!(*mode, "default" | "inherit"))
        .map(str::to_string)
}

pub(crate) fn codex_app_server_approval_mode(session_mode: &str) -> &'static str {
    match session_mode {
        "auto" => "auto",
        "fail" => "fail",
        _ => "prompt",
    }
}

fn truncate_command_output(text: &str) -> String {
    const MAX_ERROR_CHARS: usize = 4_000;
    if text.chars().count() <= MAX_ERROR_CHARS {
        return text.to_string();
    }
    let truncated = text.chars().take(MAX_ERROR_CHARS).collect::<String>();
    format!("{truncated}\n... truncated ...")
}

#[derive(Clone)]
pub(crate) struct CodexBridgeOutput {
    pub(crate) text: String,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) tool_events: Vec<CodexBridgeToolEvent>,
}

#[derive(Clone)]
pub(crate) struct CodexBridgeToolEvent {
    pub(crate) event_type: &'static str,
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) display_name: Option<String>,
    pub(crate) summary_label: Option<String>,
    pub(crate) args: Value,
    pub(crate) result: Value,
    pub(crate) is_error: Option<bool>,
}

#[derive(Clone)]
pub(crate) struct CodexBridgeToolContext {
    tool_name: String,
    display_name: Option<String>,
    summary_label: Option<String>,
    args: Value,
}

pub(crate) fn assistant_output_from_jsonl(stdout: &str) -> Result<CodexBridgeOutput, String> {
    let mut current_role: Option<String> = None;
    let mut assistant_text: Option<String> = None;
    let mut assistant_stop_reason: Option<String> = None;
    let mut assistant_usage: Option<TokenUsage> = None;
    let mut tool_events = Vec::new();
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(tool_event) = codex_jsonl_tool_event_from_json(&event) {
            tool_events.push(tool_event);
            continue;
        }
        if event.get("type").and_then(Value::as_str) == Some("turn") {
            if event.get("phase").and_then(Value::as_str) == Some("start") {
                current_role = event
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            } else if event.get("phase").and_then(Value::as_str) == Some("end")
                && event.get("role").and_then(Value::as_str) == Some("assistant")
            {
                current_role = None;
            }
            continue;
        }
        if event.get("type").and_then(Value::as_str) == Some("item")
            && event.get("subtype").and_then(Value::as_str) == Some("message_complete")
        {
            let native_completion = current_role.is_none() && event.get("text").is_some();
            if current_role.as_deref() != Some("assistant") && !native_completion {
                continue;
            }
            let completion = event.get("data").unwrap_or(&event);
            if let Some(stop_reason) = completion.get("stopReason").and_then(Value::as_str) {
                assistant_stop_reason = Some(stop_reason.to_string());
            }
            if let Some(usage) = completion.get("usage").and_then(codex_usage_from_json) {
                assistant_usage = Some(usage);
            }
            if let Some(text) = completion.get("text").and_then(Value::as_str) {
                assistant_text = Some(text.to_string());
            }
        }
    }
    if assistant_stop_reason.as_deref() == Some("error") {
        return Err("Codex app-server bridge returned an error stop reason".to_string());
    }
    let text = assistant_text.ok_or_else(|| {
        "Codex app-server bridge completed without an assistant message".to_string()
    })?;
    if text.trim().is_empty() {
        return Err("Codex app-server bridge returned an empty assistant message".to_string());
    }
    Ok(CodexBridgeOutput {
        text,
        usage: assistant_usage,
        tool_events,
    })
}

#[cfg(test)]
pub(crate) fn assistant_text_from_jsonl(stdout: &str) -> Result<String, String> {
    assistant_output_from_jsonl(stdout).map(|output| output.text)
}

fn codex_jsonl_tool_event_from_json(event: &Value) -> Option<CodexBridgeToolEvent> {
    match event.get("type").and_then(Value::as_str)? {
        "item" => match event.get("subtype").and_then(Value::as_str)? {
            "tool_call" => {
                let data = event.get("data")?;
                let tool_call_id = data
                    .get("toolCallId")
                    .or_else(|| data.get("tool_call_id"))
                    .and_then(Value::as_str)?;
                let tool_name = data
                    .get("toolName")
                    .or_else(|| data.get("tool_name"))
                    .and_then(Value::as_str)?;
                let args = data
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                Some(codex_bridge_tool_start_event(tool_call_id, tool_name, args))
            }
            "tool_result" => {
                let data = event.get("data")?;
                let tool_call_id = data
                    .get("toolCallId")
                    .or_else(|| data.get("tool_call_id"))
                    .and_then(Value::as_str)?;
                let tool_name = data
                    .get("toolName")
                    .or_else(|| data.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let result = data.get("result").cloned().unwrap_or_else(|| {
                    codex_bridge_tool_result(tool_call_id, tool_name, false, None)
                });
                let is_error = data
                    .get("isError")
                    .or_else(|| data.get("is_error"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Some(codex_bridge_tool_end_event(
                    tool_call_id,
                    tool_name,
                    result,
                    is_error,
                ))
            }
            _ => None,
        },
        "item.started" => {
            let item = event.get("item")?;
            codex_collab_tool_event_from_jsonl_item(item, false)
        }
        "item.completed" => {
            let item = event.get("item")?;
            codex_collab_tool_event_from_jsonl_item(item, true)
        }
        _ => None,
    }
}

pub(crate) fn codex_headless_tool_event_from_json(event: &Value) -> Option<CodexBridgeToolEvent> {
    match event.get("type").and_then(Value::as_str)? {
        "tool_call" => {
            let tool_call_id = event.get("call_id").and_then(Value::as_str)?;
            let tool_name =
                canonical_codex_bridge_tool_name(event.get("tool").and_then(Value::as_str)?);
            let args = event
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            Some(codex_bridge_tool_start_event(
                tool_call_id,
                &tool_name,
                args,
            ))
        }
        "tool_end" => {
            let tool_call_id = event.get("call_id").and_then(Value::as_str)?;
            let tool_name = canonical_codex_bridge_tool_name(
                event.get("tool").and_then(Value::as_str).unwrap_or("tool"),
            );
            let success = event
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let details = codex_headless_tool_end_details(event);
            Some(codex_bridge_tool_end_event(
                tool_call_id,
                &tool_name,
                codex_bridge_tool_result(tool_call_id, &tool_name, success, Some(details)),
                !success,
            ))
        }
        _ => None,
    }
}

pub(crate) fn codex_headless_tool_event_from_json_with_context(
    event: &Value,
    contexts: &mut HashMap<String, CodexBridgeToolContext>,
) -> Option<CodexBridgeToolEvent> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    if event_type == "tool_call" {
        let tool_event = codex_headless_tool_event_from_json(event)?;
        contexts.insert(
            tool_event.tool_call_id.clone(),
            CodexBridgeToolContext {
                tool_name: tool_event.tool_name.clone(),
                display_name: tool_event.display_name.clone(),
                summary_label: tool_event.summary_label.clone(),
                args: tool_event.args.clone(),
            },
        );
        return Some(tool_event);
    }
    if event_type != "tool_end" {
        return None;
    }
    let tool_call_id = event.get("call_id").and_then(Value::as_str)?;
    let context = contexts.remove(tool_call_id);
    let mut tool_event = codex_headless_tool_event_from_json(event)?;
    if let Some(context) = context {
        tool_event.tool_name = context.tool_name;
        tool_event.display_name = context.display_name;
        tool_event.summary_label = context.summary_label;
        tool_event.args = context.args;
        let success = !tool_event.is_error.unwrap_or(true);
        let mut details = codex_headless_tool_end_details(event);
        if let Some(object) = details.as_object_mut() {
            object.insert("args".to_string(), tool_event.args.clone());
        }
        tool_event.result = codex_bridge_tool_result(
            &tool_event.tool_call_id,
            &tool_event.tool_name,
            success,
            Some(details),
        );
    }
    Some(tool_event)
}

fn codex_collab_tool_event_from_jsonl_item(
    item: &Value,
    completed: bool,
) -> Option<CodexBridgeToolEvent> {
    if item.get("type").and_then(Value::as_str) != Some("collab_tool_call") {
        return None;
    }
    let tool_call_id = item.get("id").and_then(Value::as_str)?;
    let tool = item.get("tool").and_then(Value::as_str)?;
    let canonical_tool = codex_canonical_collab_tool(tool);
    let tool_name = format!("codex.subagent.{canonical_tool}");
    let args = codex_collab_args_from_jsonl_item(item, &canonical_tool);
    if !completed {
        return Some(codex_bridge_tool_start_event(
            tool_call_id,
            &tool_name,
            args,
        ));
    }
    let is_error = item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status == "failed");
    let result = codex_bridge_tool_result(tool_call_id, &tool_name, !is_error, Some(args.clone()));
    Some(codex_bridge_tool_end_event(
        tool_call_id,
        &tool_name,
        result,
        is_error,
    ))
}

fn codex_collab_args_from_jsonl_item(item: &Value, canonical_tool: &str) -> Value {
    let child_run_ids = codex_collab_child_run_ids_from_item(item);
    let codex_work_graph =
        codex_collab_work_graph_from_jsonl_item(item, canonical_tool, &child_run_ids);
    serde_json::json!({
        "codexTool": canonical_tool,
        "status": item.get("status").cloned().unwrap_or(Value::Null),
        "senderThreadId": item.get("sender_thread_id").cloned().unwrap_or(Value::Null),
        "receiverThreadIds": item.get("receiver_thread_ids").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
        "childRunIds": child_run_ids,
        "codexWorkGraph": codex_work_graph,
        "prompt": item.get("prompt").cloned().unwrap_or(Value::Null),
        "model": item.get("model").cloned().unwrap_or(Value::Null),
        "reasoningEffort": item.get("reasoning_effort").cloned().unwrap_or(Value::Null),
        "agentsStates": item.get("agents_states").cloned().unwrap_or_else(|| Value::Object(Map::new()))
    })
}

fn codex_collab_child_run_ids_from_item(item: &Value) -> Value {
    for key in ["child_run_ids", "childRunIds"] {
        if let Some(ids) = item.get(key).and_then(Value::as_array) {
            let child_run_ids = ids
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| Value::String(id.to_string()))
                .collect::<Vec<_>>();
            if !child_run_ids.is_empty() {
                return Value::Array(child_run_ids);
            }
        }
    }

    let child_run_ids = item
        .get("receiver_thread_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| Value::String(format!("codex-thread:{id}")))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(child_run_ids)
}

fn string_array_from_json_value(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn string_array_from_object_aliases(object: &Map<String, Value>, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|key| {
            let values = string_array_from_json_value(object.get(*key));
            (!values.is_empty()).then_some(values)
        })
        .unwrap_or_default()
}

fn copy_string_array_alias(object: &mut Map<String, Value>, canonical_key: &str, aliases: &[&str]) {
    if !string_array_from_json_value(object.get(canonical_key)).is_empty() {
        return;
    }
    let values = string_array_from_object_aliases(object, aliases);
    if values.is_empty() {
        return;
    }
    object.insert(
        canonical_key.to_string(),
        Value::Array(values.into_iter().map(Value::String).collect()),
    );
}

fn copy_value_alias(object: &mut Map<String, Value>, canonical_key: &str, aliases: &[&str]) {
    if object.get(canonical_key).is_some() {
        return;
    }
    if let Some(value) = aliases.iter().find_map(|key| object.get(*key).cloned()) {
        object.insert(canonical_key.to_string(), value);
    }
}

fn normalize_codex_subagent_args_aliases(object: &mut Map<String, Value>) {
    copy_value_alias(object, "threadId", &["thread_id"]);
    copy_value_alias(object, "turnId", &["turn_id"]);
    copy_value_alias(object, "senderThreadId", &["sender_thread_id"]);
    copy_string_array_alias(object, "receiverThreadIds", &["receiver_thread_ids"]);
    copy_string_array_alias(object, "childRunIds", &["child_run_ids"]);
    copy_value_alias(object, "agentsStates", &["agents_states"]);
}

fn codex_collab_work_graph_from_jsonl_item(
    item: &Value,
    canonical_tool: &str,
    child_run_ids: &Value,
) -> Value {
    let receiver_thread_ids = string_array_from_json_value(item.get("receiver_thread_ids"));
    let child_run_ids = string_array_from_json_value(Some(child_run_ids));
    let target_count = receiver_thread_ids.len().max(child_run_ids.len());
    let child_runs = (0..target_count)
        .map(|index| {
            let thread_id = receiver_thread_ids.get(index).cloned();
            let child_run_id = child_run_ids
                .get(index)
                .cloned()
                .or_else(|| thread_id.as_ref().map(|id| format!("codex-thread:{id}")))
                .unwrap_or_else(|| format!("unknown-child-run-{index}"));
            let mut child_run = serde_json::json!({
                "edgeId": codex_collab_edge_id(
                    item.get("id").and_then(Value::as_str),
                    &child_run_id,
                    index,
                    canonical_tool,
                ),
                "targetIndex": index,
                "childRunId": child_run_id,
                "operation": canonical_tool,
            });
            if let (Some(object), Some(thread_id)) = (child_run.as_object_mut(), thread_id.as_ref())
            {
                object.insert("threadId".to_string(), Value::String(thread_id.clone()));
            }
            if let Some(status) = thread_id
                .as_ref()
                .and_then(|id| codex_collab_child_status_from_jsonl_item(item, id))
            {
                if let Some(object) = child_run.as_object_mut() {
                    object.insert("status".to_string(), Value::String(status));
                }
            }
            child_run
        })
        .collect::<Vec<_>>();
    let sender_thread_id = item.get("sender_thread_id").cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
        "toolCallId": item.get("id").cloned().unwrap_or(Value::Null),
        "tool": canonical_tool,
        "status": item.get("status").cloned().unwrap_or(Value::Null),
        "parent": {
            "threadId": sender_thread_id.clone(),
            "turnId": item.get("turn_id").cloned().unwrap_or(Value::Null),
            "senderThreadId": sender_thread_id,
        },
        "childRuns": child_runs,
    })
}

fn codex_collab_edge_id(
    tool_call_id: Option<&str>,
    child_run_id: &str,
    index: usize,
    canonical_tool: &str,
) -> String {
    format!(
        "{}:{index}:{canonical_tool}:{child_run_id}",
        tool_call_id.unwrap_or("unknown-tool-call")
    )
}

fn codex_collab_child_status_from_jsonl_item(item: &Value, thread_id: &str) -> Option<String> {
    let agent_states = item
        .get("agents_states")
        .or_else(|| item.get("agentsStates"))?
        .as_object()?;
    let agent_state = agent_states.get(thread_id)?.as_object()?;
    json_string_from_object(agent_state, &["status"])
}

fn codex_bridge_tool_args(tool_name: &str, args: Value, tool_call_id: Option<&str>) -> Value {
    if !tool_name.starts_with("codex.subagent.") {
        return args;
    }
    let mut object = match args {
        Value::Object(object) => object,
        other => return other,
    };
    normalize_codex_subagent_args_aliases(&mut object);
    if let Some(tool_call_id) = tool_call_id.filter(|id| !id.is_empty()) {
        let has_tool_call_id = object
            .get("toolCallId")
            .or_else(|| object.get("tool_call_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
        if !has_tool_call_id {
            object.insert(
                "toolCallId".to_string(),
                Value::String(tool_call_id.to_string()),
            );
        }
    }
    let canonical_tool = object
        .get("codexTool")
        .and_then(Value::as_str)
        .or_else(|| tool_name.strip_prefix("codex.subagent."))
        .map(codex_canonical_collab_tool);
    if let Some(canonical_tool) = canonical_tool {
        object.insert("codexTool".to_string(), Value::String(canonical_tool));
    }
    let has_child_run_ids = object
        .get("childRunIds")
        .and_then(Value::as_array)
        .is_some_and(|ids| {
            ids.iter()
                .any(|id| id.as_str().is_some_and(|id| !id.is_empty()))
        });
    if !has_child_run_ids {
        if let Some(ids) = object.get("child_run_ids").and_then(Value::as_array) {
            let child_run_ids = ids
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| Value::String(id.to_string()))
                .collect::<Vec<_>>();
            if !child_run_ids.is_empty() {
                object.insert("childRunIds".to_string(), Value::Array(child_run_ids));
            }
        }
    }
    let has_child_run_ids = object
        .get("childRunIds")
        .and_then(Value::as_array)
        .is_some_and(|ids| {
            ids.iter()
                .any(|id| id.as_str().is_some_and(|id| !id.is_empty()))
        });
    if !has_child_run_ids {
        let child_run_ids = object
            .get("receiverThreadIds")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(|id| Value::String(format!("codex-thread:{id}")))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        object.insert("childRunIds".to_string(), Value::Array(child_run_ids));
    }
    let has_work_graph = object
        .get("codexWorkGraph")
        .or_else(|| object.get("codex_work_graph"))
        .and_then(Value::as_object)
        .is_some();
    if !has_work_graph {
        object.insert(
            "codexWorkGraph".to_string(),
            codex_collab_work_graph_from_args_object(tool_name, &object),
        );
    }
    Value::Object(object)
}

fn codex_collab_work_graph_from_args_object(tool_name: &str, object: &Map<String, Value>) -> Value {
    let canonical_tool = codex_canonical_collab_tool(
        object
            .get("codexTool")
            .and_then(Value::as_str)
            .or_else(|| tool_name.strip_prefix("codex.subagent."))
            .unwrap_or(tool_name),
    );
    let receiver_thread_ids =
        string_array_from_object_aliases(object, &["receiverThreadIds", "receiver_thread_ids"]);
    let child_run_ids = string_array_from_object_aliases(object, &["childRunIds", "child_run_ids"]);
    let target_count = receiver_thread_ids.len().max(child_run_ids.len());
    let child_runs = (0..target_count)
        .map(|index| {
            let thread_id = receiver_thread_ids.get(index).cloned();
            let child_run_id = child_run_ids
                .get(index)
                .cloned()
                .or_else(|| thread_id.as_ref().map(|id| format!("codex-thread:{id}")))
                .unwrap_or_else(|| format!("unknown-child-run-{index}"));
            let mut child_run = serde_json::json!({
                "edgeId": codex_collab_edge_id(
                    object.get("toolCallId").and_then(Value::as_str),
                    &child_run_id,
                    index,
                    &canonical_tool,
                ),
                "targetIndex": index,
                "childRunId": child_run_id,
                "operation": canonical_tool,
            });
            if let (Some(object), Some(thread_id)) = (child_run.as_object_mut(), thread_id.as_ref())
            {
                object.insert("threadId".to_string(), Value::String(thread_id.clone()));
            }
            if let Some(status) = thread_id
                .as_ref()
                .and_then(|id| codex_collab_child_status_from_args_object(object, id))
            {
                if let Some(object) = child_run.as_object_mut() {
                    object.insert("status".to_string(), Value::String(status));
                }
            }
            child_run
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
        "toolCallId": object.get("toolCallId").cloned().unwrap_or(Value::Null),
        "tool": canonical_tool,
        "status": object.get("status").cloned().unwrap_or(Value::Null),
        "parent": {
            "threadId": object.get("threadId").or_else(|| object.get("thread_id")).cloned().unwrap_or(Value::Null),
            "turnId": object.get("turnId").or_else(|| object.get("turn_id")).cloned().unwrap_or(Value::Null),
            "senderThreadId": object.get("senderThreadId").or_else(|| object.get("sender_thread_id")).cloned().unwrap_or(Value::Null),
        },
        "childRuns": child_runs,
    })
}

fn codex_collab_child_status_from_args_object(
    object: &Map<String, Value>,
    thread_id: &str,
) -> Option<String> {
    let agent_states = object
        .get("agentsStates")
        .or_else(|| object.get("agents_states"))?
        .as_object()?;
    let agent_state = agent_states.get(thread_id)?.as_object()?;
    json_string_from_object(agent_state, &["status"])
}

pub(crate) fn json_string_from_object(
    object: &Map<String, Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn codex_canonical_collab_tool(tool: &str) -> String {
    match tool {
        "spawn_agent" | "spawnAgent" => "spawnAgent".to_string(),
        "send_input" | "sendInput" => "sendInput".to_string(),
        "resume_agent" | "resume_subagent" | "resumeAgent" | "resumeSubagent" => {
            "resumeAgent".to_string()
        }
        "wait_agent" | "waitAgent" | "wait" => "wait".to_string(),
        "close_agent" | "closeAgent" => "closeAgent".to_string(),
        other => other.to_string(),
    }
}

fn canonical_codex_bridge_tool_name(tool_name: &str) -> String {
    tool_name
        .strip_prefix("codex.subagent.")
        .map(|tool| format!("codex.subagent.{}", codex_canonical_collab_tool(tool)))
        .unwrap_or_else(|| tool_name.to_string())
}

fn codex_bridge_tool_start_event(
    tool_call_id: &str,
    tool_name: &str,
    args: Value,
) -> CodexBridgeToolEvent {
    let args = codex_bridge_tool_args(tool_name, args, Some(tool_call_id));
    CodexBridgeToolEvent {
        event_type: "tool_execution_start",
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        display_name: codex_bridge_tool_display_name(tool_name),
        summary_label: codex_bridge_tool_summary_label(tool_name, &args),
        args,
        result: Value::Null,
        is_error: None,
    }
}

fn codex_bridge_tool_end_event(
    tool_call_id: &str,
    tool_name: &str,
    result: Value,
    is_error: bool,
) -> CodexBridgeToolEvent {
    CodexBridgeToolEvent {
        event_type: "tool_execution_end",
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        display_name: codex_bridge_tool_display_name(tool_name),
        summary_label: codex_bridge_tool_summary_label(tool_name, &Value::Null),
        args: Value::Object(Map::new()),
        result,
        is_error: Some(is_error),
    }
}

fn codex_bridge_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    success: bool,
    details: Option<Value>,
) -> Value {
    let text = if success {
        format!("{tool_name} completed")
    } else {
        format!("{tool_name} failed")
    };
    let mut result = serde_json::json!({
        "role": "toolResult",
        "toolCallId": tool_call_id,
        "toolName": tool_name,
        "content": [{ "type": "text", "text": text }],
        "isError": !success,
        "timestamp": now_rfc3339()
    });
    if let Some(details) = details {
        result["details"] = details;
    }
    result
}

fn codex_headless_tool_end_details(event: &Value) -> Value {
    let mut details = event
        .get("details")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(error_code) = event.get("error_code").and_then(Value::as_str) {
        details.insert(
            "errorCode".to_string(),
            Value::String(error_code.to_string()),
        );
    }
    if let Some(approval_request_id) = event.get("approval_request_id").and_then(Value::as_str) {
        details.insert(
            "approvalRequestId".to_string(),
            Value::String(approval_request_id.to_string()),
        );
    }
    if let Some(governed_outcome) = event.get("governed_outcome").and_then(Value::as_str) {
        details.insert(
            "governedOutcome".to_string(),
            Value::String(governed_outcome.to_string()),
        );
    }
    Value::Object(details)
}

fn codex_bridge_tool_display_name(tool_name: &str) -> Option<String> {
    tool_name
        .strip_prefix("codex.subagent.")
        .map(|tool| format!("Codex subagent: {}", codex_collab_human_tool(tool)))
}

fn codex_bridge_tool_summary_label(tool_name: &str, args: &Value) -> Option<String> {
    let tool = tool_name.strip_prefix("codex.subagent.")?;
    let count = args
        .get("receiverThreadIds")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let target = match count {
        0 => return Some(codex_collab_human_tool(tool).to_string()),
        1 => "1 agent".to_string(),
        n => format!("{n} agents"),
    };
    Some(format!("{} {target}", codex_collab_human_tool(tool)))
}

fn codex_collab_human_tool(tool: &str) -> &'static str {
    match tool {
        "spawnAgent" => "spawn agent",
        "sendInput" => "send input",
        "resumeAgent" => "resume agent",
        "closeAgent" => "close agent",
        "wait" => "wait",
        _ => "subagent",
    }
}

fn codex_usage_from_json(usage: &Value) -> Option<TokenUsage> {
    let input_tokens = value_u64_field(usage, &["input", "tokensInput", "input_tokens"]);
    let output_tokens = value_u64_field(usage, &["output", "tokensOutput", "output_tokens"]);
    let cache_read_tokens = value_u64_field(
        usage,
        &["cacheRead", "tokensCacheRead", "cache_read_tokens"],
    );
    let cache_write_tokens = value_u64_field(
        usage,
        &["cacheWrite", "tokensCacheWrite", "cache_write_tokens"],
    );
    let cost = usage
        .get("cost")
        .and_then(|cost| {
            cost.get("total")
                .and_then(Value::as_f64)
                .or_else(|| cost.as_f64())
        })
        .or_else(|| usage.get("costTotal").and_then(Value::as_f64));

    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_write_tokens == 0
        && cost.is_none()
    {
        return None;
    }

    Some(TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost,
    })
}

fn value_u64_field(value: &Value, names: &[&str]) -> u64 {
    names
        .iter()
        .find_map(|name| {
            value
                .get(*name)
                .and_then(Value::as_u64)
                .or_else(|| value.get(*name).and_then(Value::as_f64).map(|n| n as u64))
        })
        .unwrap_or(0)
}

pub(crate) const CODEX_BRIDGE_DIRECT_ARG_MAX_BYTES: usize = 64 * 1024;

pub(crate) struct CodexBridgePrompt {
    pub(crate) argument: String,
    pub(crate) temp_dir: PathBuf,
}

pub(crate) fn codex_bridge_prompt_body(prompt: &str, attachment_paths: &[String]) -> String {
    let mut body = format!("# Maestro Rust Codex bridge request\n\n{prompt}\n");
    if !attachment_paths.is_empty() {
        body.push_str("\n## Attachment files\n\n");
        body.push_str(
            "The user uploaded the following files. Inspect these paths with tools as needed before answering:\n",
        );
        for path in attachment_paths {
            body.push_str(&format!("- {path}\n"));
        }
    }
    body
}

fn unique_temp_name(prefix: &str, counter: &AtomicU64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let counter = counter.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{now}-{counter}", process::id())
}

pub(crate) fn sandbox_visible_temp_dir(cwd: &Path, prefix: &str, counter: &AtomicU64) -> PathBuf {
    let name = unique_temp_name(prefix, counter);
    if codex_app_server_sandbox_mode().as_deref() == Some("docker") {
        cwd.join(format!(".{name}"))
    } else {
        env::temp_dir().join(name)
    }
}

pub(crate) fn codex_bridge_temp_dir(cwd: &Path) -> PathBuf {
    sandbox_visible_temp_dir(cwd, "maestro-codex-bridge", &CODEX_BRIDGE_TEMP_COUNTER)
}

async fn run_codex_bridge_command(
    mut command: Command,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<std::process::Output, String> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run Codex app-server bridge: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture Codex app-server bridge stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture Codex app-server bridge stderr".to_string())?;
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });

    let waited = tokio::select! {
        result = tokio::time::timeout(codex_app_server_timeout(), child.wait()) => result,
        _ = async { match cancel { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err("Codex turn stopped by its authenticated owner".to_string());
        }
    };
    let status = match waited {
        Ok(status) => status
            .map_err(|error| format!("failed to wait for Codex app-server bridge: {error}"))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err("Codex app-server request timed out".to_string());
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| format!("failed to join Codex app-server stdout reader: {error}"))?
        .map_err(|error| format!("failed to read Codex app-server stdout: {error}"))?;
    let stderr = stderr_task
        .await
        .map_err(|error| format!("failed to join Codex app-server stderr reader: {error}"))?
        .map_err(|error| format!("failed to read Codex app-server stderr: {error}"))?;

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

pub(crate) async fn prepare_codex_bridge_prompt(
    cwd: &Path,
    prompt: &str,
    attachment_paths: &[String],
) -> Result<CodexBridgePrompt, String> {
    let temp_dir = codex_bridge_temp_dir(cwd);
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|error| format!("failed to create Codex bridge prompt directory: {error}"))?;
    let body = codex_bridge_prompt_body(prompt, attachment_paths);
    if body.len() <= CODEX_BRIDGE_DIRECT_ARG_MAX_BYTES {
        return Ok(CodexBridgePrompt {
            argument: body,
            temp_dir,
        });
    }

    let prompt_path = temp_dir.join("prompt.md");
    tokio::fs::write(&prompt_path, body)
        .await
        .map_err(|error| format!("failed to write Codex bridge prompt file: {error}"))?;
    Ok(CodexBridgePrompt {
        argument: format!(
            "Use the read tool to read {}. Follow the request in that file. Reply only with the final answer.",
            prompt_path.display()
        ),
        temp_dir,
    })
}

#[cfg(test)]
pub(crate) async fn run_codex_app_server_cli(
    cwd: &Path,
    model: &str,
    approval_mode: &str,
    prompt: &str,
    attachment_paths: &[String],
) -> Result<CodexBridgeOutput, String> {
    run_codex_app_server_cli_cancellable(cwd, model, approval_mode, prompt, attachment_paths, None)
        .await
}

pub(crate) async fn run_codex_app_server_cli_cancellable(
    cwd: &Path,
    model: &str,
    approval_mode: &str,
    prompt: &str,
    attachment_paths: &[String],
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<CodexBridgeOutput, String> {
    let cli_path = codex_app_server_cli_path();
    if !cli_path.exists() {
        return Err(format!(
            "Codex app-server bridge requires executable {}. Set MAESTRO_CODEX_APP_SERVER_CLI to override it.",
            cli_path.display()
        ));
    }

    let bridge_prompt = prepare_codex_bridge_prompt(cwd, prompt, attachment_paths).await?;
    let sandbox_mode = codex_app_server_sandbox_mode();
    let mut command = codex_bridge_command(&cli_path);
    command
        .arg("exec")
        .arg("--model")
        .arg(model)
        .arg("--json")
        .arg("--no-session")
        .arg("--approval-mode")
        .arg(approval_mode);
    if let Some(sandbox_mode) = sandbox_mode {
        command.arg("--sandbox").arg(sandbox_mode);
    }
    command
        .arg(&bridge_prompt.argument)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("MAESTRO_TELEMETRY_DISABLED", "1")
        .env(
            "MAESTRO_USAGE_FILE",
            bridge_prompt.temp_dir.join("usage.json"),
        );

    let output_result = run_codex_bridge_command(command, cancel).await;
    let _ = tokio::fs::remove_dir_all(&bridge_prompt.temp_dir).await;
    let output = output_result?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        return assistant_output_from_jsonl(&stdout);
    }

    let mut message = format!(
        "Codex app-server bridge failed with exit code {}",
        output.status.code().unwrap_or(1)
    );
    if !stderr.is_empty() {
        message.push_str(&format!(": {}", truncate_command_output(&stderr)));
    } else if !stdout.is_empty() {
        message.push_str(&format!(": {}", truncate_command_output(&stdout)));
    }
    Err(message)
}

#[derive(Clone, Copy)]
pub(crate) enum CodexBridgeTransport {
    Sse,
    WebSocket,
}

pub(crate) async fn send_codex_bridge_event(
    stream: &mut impl ChatEventWriter,
    transport: CodexBridgeTransport,
    value: &Value,
) -> Result<(), String> {
    stream.send_event(transport, value).await
}

pub(crate) async fn send_codex_bridge_tool_event(
    stream: &mut impl ChatEventWriter,
    transport: CodexBridgeTransport,
    event: &CodexBridgeToolEvent,
) -> Result<(), String> {
    let mut object = Map::new();
    object.insert(
        "type".to_string(),
        Value::String(event.event_type.to_string()),
    );
    object.insert(
        "toolCallId".to_string(),
        Value::String(event.tool_call_id.clone()),
    );
    object.insert(
        "toolName".to_string(),
        Value::String(event.tool_name.clone()),
    );
    if let Some(display_name) = &event.display_name {
        object.insert(
            "displayName".to_string(),
            Value::String(display_name.clone()),
        );
    }
    if let Some(summary_label) = &event.summary_label {
        object.insert(
            "summaryLabel".to_string(),
            Value::String(summary_label.clone()),
        );
    }
    match event.event_type {
        "tool_execution_start" => {
            object.insert("args".to_string(), event.args.clone());
        }
        "tool_execution_end" => {
            object.insert("result".to_string(), event.result.clone());
            object.insert(
                "isError".to_string(),
                Value::Bool(event.is_error.unwrap_or(false)),
            );
        }
        _ => return Ok(()),
    }
    send_codex_bridge_event(stream, transport, &Value::Object(object)).await
}

pub(crate) fn codex_headless_usage_from_json(event: &Value) -> Option<TokenUsage> {
    let usage = event.get("usage")?;
    let input_tokens = value_u64_field(usage, &["input_tokens", "inputTokens", "input"]);
    let output_tokens = value_u64_field(usage, &["output_tokens", "outputTokens", "output"]);
    let cache_read_tokens = value_u64_field(
        usage,
        &["cache_read_tokens", "cacheReadTokens", "cacheRead"],
    );
    let cache_write_tokens = value_u64_field(
        usage,
        &["cache_write_tokens", "cacheWriteTokens", "cacheWrite"],
    );
    let cost = usage
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .or_else(|| usage.get("totalCostUsd").and_then(Value::as_f64))
        .or_else(|| usage.get("costTotal").and_then(Value::as_f64));
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_write_tokens == 0
        && cost.is_none()
    {
        return None;
    }
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost,
    })
}

async fn write_codex_headless_message(
    stdin: &mut tokio::process::ChildStdin,
    value: &Value,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| format!("failed to serialize Codex headless message: {error}"))?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| format!("failed to write Codex headless message: {error}"))
}

fn codex_headless_approval_response(
    request_id: &str,
    approved: bool,
    result: Option<&ToolResult>,
) -> Value {
    let mut result_value = if let Some(result) = result {
        serde_json::json!({
            "success": result.success,
            "output": result.output,
            "error": result.error
        })
    } else if approved {
        serde_json::json!({
            "success": true,
            "output": "Approved"
        })
    } else {
        serde_json::json!({
            "success": false,
            "output": "",
            "error": "Denied by user"
        })
    };
    if result_value.get("error").is_some_and(Value::is_null) {
        result_value
            .as_object_mut()
            .map(|object| object.remove("error"));
    }
    serde_json::json!({
        "type": "server_request_response",
        "request_id": request_id,
        "request_type": "approval",
        "approved": approved,
        "result": result_value
    })
}

fn codex_headless_run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let counter = CODEX_HEADLESS_RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("run-{}-{now}-{counter}", process::id())
}

pub(crate) fn codex_headless_pending_request_id(
    session_id: Option<&str>,
    run_id: &str,
    request_id: &str,
) -> String {
    format!(
        "codex:{}:{}:{}",
        session_id.unwrap_or("default"),
        run_id,
        request_id
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_codex_headless_approval_request(
    stream: &mut impl ChatEventWriter,
    transport: CodexBridgeTransport,
    state: &AppState,
    session_id: Option<&str>,
    run_id: &str,
    request: &Value,
    pending_approval_ids: &Arc<Mutex<Vec<String>>>,
    stdin: &mut tokio::process::ChildStdin,
) -> Result<(), String> {
    let native_tool_call = request.get("type").and_then(Value::as_str) == Some("tool_call");
    if !native_tool_call && request.get("request_type").and_then(Value::as_str) != Some("approval")
    {
        return Ok(());
    }
    let child_request_id = request
        .get(if native_tool_call {
            "call_id"
        } else {
            "request_id"
        })
        .and_then(Value::as_str)
        .or_else(|| request.get("call_id").and_then(Value::as_str))
        .ok_or_else(|| "Codex headless approval request missing request_id".to_string())?
        .to_string();
    let external_request_id =
        codex_headless_pending_request_id(session_id, run_id, child_request_id.as_str());
    let tool_name = request
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let args = request
        .get("args")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let reason = request
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("Tool execution requires approval")
        .to_string();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    {
        // Publish sender and owner together, so a concurrent legacy resume
        // cannot deliver a native approval before its binding is visible.
        let mut senders = state.pending_tool_responses.lock().await;
        let mut owners = state.pending_tool_response_sessions.lock().await;
        senders.insert(external_request_id.clone(), sender);
        if let Some(session_id) = session_id {
            owners.insert(
                external_request_id.clone(),
                stream.native_turn().map_or_else(
                    || PendingToolResponseOwner::Session(session_id.to_string()),
                    |turn| turn.pending_request_owner(),
                ),
            );
        }
    }
    pending_approval_ids
        .lock()
        .await
        .push(external_request_id.clone());

    send_codex_bridge_event(
        stream,
        transport,
        &serde_json::json!({
            "type": "action_approval_required",
            "request": {
                "id": external_request_id,
                "toolName": tool_name,
                "args": args,
                "reason": reason
            }
        }),
    )
    .await?;

    let owner = stream.native_turn();
    let decision = tokio::select! {
        decision = receiver.recv() => decision,
        _ = async { match &owner { Some(owner) => owner.cancel.cancelled().await, None => std::future::pending().await } } => {
            return Err("Codex turn stopped while awaiting its existing approval owner".to_string());
        }
    };
    let Some((_call_id, approved, result, _source, _consumed)) = decision else {
        return Err("Codex headless approval request closed before decision".to_string());
    };
    let response = if native_tool_call {
        // An approval is a decision, not a fabricated tool execution result.
        // Without a client result the native owner executes the admitted tool.
        let mut response = serde_json::json!({
            "type": "tool_response",
            "call_id": child_request_id,
            "approved": approved,
        });
        if let Some(execution_id) = request.get("tool_execution_id") {
            response["tool_execution_id"] = execution_id.clone();
        }
        if let Some(result) = result.as_ref() {
            response["result"] = serde_json::json!({
                "success": result.success,
                "output": result.output,
                "error": result.error,
            });
        }
        response
    } else {
        codex_headless_approval_response(&child_request_id, approved, result.as_ref())
    };
    write_codex_headless_message(stdin, &response).await
}

async fn cleanup_codex_headless_approvals(
    state: &AppState,
    pending_approval_ids: &Arc<Mutex<Vec<String>>>,
) {
    let ids = pending_approval_ids.lock().await.clone();
    if ids.is_empty() {
        return;
    }
    let mut pending = state.pending_tool_responses.lock().await;
    let mut owners = state.pending_tool_response_sessions.lock().await;
    for id in ids {
        pending.remove(&id);
        owners.remove(&id);
    }
}

pub(crate) fn codex_headless_hello() -> Value {
    serde_json::json!({
        "type": "hello",
        "protocol_version": maestro_local_host::headless::HEADLESS_PROTOCOL_VERSION,
        "client_info": { "name": "maestro-rust-control-plane" },
        "capabilities": { "server_requests": ["approval"] },
        "role": "controller"
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_codex_app_server_headless_cli(
    stream: &mut impl ChatEventWriter,
    transport: CodexBridgeTransport,
    state: &AppState,
    session_id: Option<&str>,
    cwd: &Path,
    model: &str,
    prompt: &str,
    attachment_paths: &[String],
) -> Result<CodexBridgeOutput, String> {
    let cli_path = codex_app_server_cli_path();
    if !cli_path.exists() {
        return Err(format!(
            "Codex app-server bridge requires executable {}. Set MAESTRO_CODEX_APP_SERVER_CLI to override it.",
            cli_path.display()
        ));
    }

    let bridge_prompt = prepare_codex_bridge_prompt(cwd, prompt, attachment_paths).await?;
    let sandbox_mode = codex_app_server_sandbox_mode();
    let mut command = codex_bridge_command(&cli_path);
    // The app-server wire uses an unqualified ID, but native routing needs
    // the subscription provider. A bare ID would select the metered API.
    command
        .arg("headless")
        .env("MAESTRO_MODEL", format!("openai-codex/{model}"))
        .env("MAESTRO_GATEWAY_BACKGROUND_SCOPE_REQUIRED", "1");
    if let Some(sandbox_mode) = sandbox_mode {
        command.env("MAESTRO_SANDBOX_MODE", sandbox_mode);
    }
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("NO_COLOR", "1")
        .env("MAESTRO_TELEMETRY_DISABLED", "1")
        .env(
            "MAESTRO_USAGE_FILE",
            bridge_prompt.temp_dir.join("usage.json"),
        );

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run Codex headless bridge: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to capture Codex headless stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture Codex headless stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture Codex headless stderr".to_string())?;
    let stderr_task = tokio::spawn(async move {
        // The protocol carries actionable errors. Drain diagnostics without
        // retaining an unbounded buffer or disclosing provider credentials.
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await
    });
    let pending_approval_ids = Arc::new(Mutex::new(Vec::new()));
    let approval_run_id = codex_headless_run_id();
    let request_timeout = codex_app_server_timeout();
    let shutdown_timeout = codex_app_server_shutdown_timeout();
    let mut lines = BufReader::new(stdout).lines();
    let native_owner = stream.native_turn();

    let request_result = async {
        write_codex_headless_message(&mut stdin, &codex_headless_hello()).await?;
        write_codex_headless_message(
            &mut stdin,
            &serde_json::json!({
                "type": "init",
                "approval_mode": "prompt"
            }),
        )
        .await?;
        write_codex_headless_message(
            &mut stdin,
            &serde_json::json!({
                "type": "prompt",
                "content": bridge_prompt.argument
            }),
        )
        .await?;

        let mut assistant_text = String::new();
        let mut assistant_usage = None;
        let mut separate_response = false;
        let mut tool_events = Vec::new();
        let mut tool_contexts: HashMap<String, CodexBridgeToolContext> = HashMap::new();
        loop {
            let next_line = tokio::select! {
                result = tokio::time::timeout(request_timeout, lines.next_line()) => result,
                _ = async { match &native_owner { Some(owner) => owner.cancel.cancelled().await, None => std::future::pending().await } } => {
                    return Err("Codex turn stopped by its authenticated owner".to_string());
                }
            };
            let line = match next_line {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    return Err(format!("failed to read Codex headless output: {error}"));
                }
                Err(_) => return Err("Codex app-server request timed out".to_string()),
            };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("response_start") => {
                    separate_response = !assistant_text.is_empty();
                }
                Some("response_chunk")
                    if !event
                        .get("is_thinking")
                        .and_then(Value::as_bool)
                        .unwrap_or(false) =>
                {
                    if let Some(content) = event.get("content").and_then(Value::as_str) {
                        if separate_response && !content.is_empty() {
                            assistant_text.push_str("\n\n");
                            separate_response = false;
                        }
                        assistant_text.push_str(content);
                    }
                }
                Some("response_end") => {
                    // A model response may end before its tools request approval.
                    // Only the native turn boundary proves coding completion.
                    assistant_usage = codex_headless_usage_from_json(&event);
                }
                Some("turn_completed") => {
                    let _ = write_codex_headless_message(
                        &mut stdin,
                        &serde_json::json!({ "type": "shutdown" }),
                    )
                    .await;
                    if assistant_text.trim().is_empty() {
                        return Err(
                            "Codex headless bridge returned an empty assistant message".to_string()
                        );
                    }
                    return Ok(CodexBridgeOutput {
                        text: assistant_text,
                        usage: assistant_usage,
                        tool_events,
                    });
                }
                Some("tool_call") | Some("tool_end") => {
                    // Codex can keep one response ID across commentary, a
                    // native tool, and the final answer. Preserve that visible
                    // tool boundary even without another response_start.
                    separate_response = !assistant_text.is_empty();
                    if let Some(tool_event) =
                        codex_headless_tool_event_from_json_with_context(&event, &mut tool_contexts)
                    {
                        tool_events.push(tool_event);
                    }
                    if event.get("type").and_then(Value::as_str) == Some("tool_call")
                        && event.get("requires_approval").and_then(Value::as_bool) == Some(true)
                    {
                        handle_codex_headless_approval_request(
                            stream,
                            transport,
                            state,
                            session_id,
                            &approval_run_id,
                            &event,
                            &pending_approval_ids,
                            &mut stdin,
                        )
                        .await?;
                    }
                }
                Some("server_request") => {
                    handle_codex_headless_approval_request(
                        stream,
                        transport,
                        state,
                        session_id,
                        &approval_run_id,
                        &event,
                        &pending_approval_ids,
                        &mut stdin,
                    )
                    .await?;
                }
                Some("error" | "provider_error") => {
                    let message = event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex headless bridge error");
                    if event.get("type").and_then(Value::as_str) == Some("provider_error")
                        || event.get("fatal").and_then(Value::as_bool).unwrap_or(false)
                        || event
                            .get("terminal")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    {
                        return Err(message.to_string());
                    }
                }
                Some("turn_interrupted") => {
                    return Err(event
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("Native coding turn interrupted")
                        .to_owned());
                }
                _ => {}
            }
        }
        Err("Codex headless bridge exited without an assistant response".to_string())
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&bridge_prompt.temp_dir).await;
    cleanup_codex_headless_approvals(state, &pending_approval_ids).await;
    let output = request_result;
    if output.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = stderr_task.await;
        return output;
    }
    match tokio::time::timeout(shutdown_timeout, child.wait()).await {
        Ok(Ok(_status)) => {}
        Ok(Err(error)) => {
            return Err(format!("failed to wait for Codex headless bridge: {error}"));
        }
        Err(_) => {
            let _ = child.kill().await;
        }
    }
    let _stderr = stderr_task
        .await
        .map_err(|error| format!("failed to join Codex headless stderr reader: {error}"))?
        .map_err(|error| format!("failed to read Codex headless stderr: {error}"))?;
    output
}
