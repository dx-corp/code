use serde_json::{Map, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{ATTACHMENT_TEMP_COUNTER, AppState, now_rfc3339};

use super::tasks::{
    A2A_LEGACY_CONTROL_PLANE_LEDGER_PEER, A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY,
    A2A_RUNTIME_GATEWAY_LEDGER_DISPLAY_NAME, A2A_RUNTIME_GATEWAY_LEDGER_PEER, a2a_agent_message,
    a2a_task_is_terminal, a2a_task_status_state, a2a_task_status_timestamp, a2a_task_value,
    canonical_a2a_task_state, generate_a2a_id,
};

pub(crate) const A2A_LEDGER_LOCK_RETRY_MS: u64 = 25;
const A2A_LEDGER_LOCK_STALE_MS: u64 = 30_000;
const A2A_LEDGER_LOCK_TIMEOUT_MS: u64 = A2A_LEDGER_LOCK_STALE_MS + A2A_LEDGER_LOCK_RETRY_MS;
// This directory/metadata protocol is mirrored by the TUI ledger writer.
// Both processes must continue to claim the same `<ledger>.lock` directory.
const A2A_LEDGER_LOCK_OWNER_FILE: &str = "owner";
pub(crate) const A2A_LEDGER_LOCK_HEARTBEAT_FILE: &str = "heartbeat";
/// Bound on manual symlink following in `resolve_a2a_task_ledger_path`, so a
/// cyclic alias reports an error instead of looping forever.
const MAX_A2A_TASK_LEDGER_SYMLINK_HOPS: usize = 32;

#[cfg(test)]
tokio::task_local! {
    // Only the store future scoped by the lock-ordering fixture observes this.
    static LOCK_RETRY_PROBE: std::cell::RefCell<Option<tokio::sync::oneshot::Sender<()>>>;
}

async fn resolve_a2a_task_ledger_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve relative A2A task ledger path: {error}"))?
            .join(path)
    };
    let mut missing = Vec::<OsString>::new();
    let mut cursor = absolute;
    let mut hops = 0usize;
    loop {
        match tokio::fs::symlink_metadata(&cursor).await {
            Ok(metadata) => match tokio::fs::canonicalize(&cursor).await {
                Ok(mut resolved) => {
                    for component in missing.iter().rev() {
                        resolved.push(component);
                    }
                    if missing.is_empty() && metadata.is_dir() {
                        return Err(format!(
                            "A2A task ledger path {} is a directory",
                            path.display()
                        ));
                    }
                    return Ok(resolved);
                }
                // A symlink whose target does not exist. The SQLite ledger
                // never creates the JSON boundary path, so an alias naming it
                // is normally dangling. Follow the link by hand so the TUI and
                // this writer still derive the same lock and database.
                Err(error) if error.kind() == ErrorKind::NotFound && metadata.is_symlink() => {
                    hops += 1;
                    if hops > MAX_A2A_TASK_LEDGER_SYMLINK_HOPS {
                        return Err(format!(
                            "A2A task ledger path {} exceeds the symlink hop limit",
                            path.display()
                        ));
                    }
                    let target = tokio::fs::read_link(&cursor).await.map_err(|error| {
                        format!("resolve A2A task ledger path {}: {error}", cursor.display())
                    })?;
                    cursor = match cursor.parent() {
                        Some(parent) if target.is_relative() => parent.join(target),
                        _ => target,
                    };
                }
                Err(error) => {
                    return Err(format!(
                        "resolve A2A task ledger path {}: {error}",
                        cursor.display()
                    ));
                }
            },
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let component = cursor
                    .file_name()
                    .ok_or_else(|| format!("resolve A2A task ledger path {}", path.display()))?
                    .to_os_string();
                missing.push(component);
                cursor = cursor
                    .parent()
                    .ok_or_else(|| format!("resolve A2A task ledger parent {}", path.display()))?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(format!(
                    "inspect A2A task ledger path {}: {error}",
                    cursor.display()
                ));
            }
        }
    }
}

pub(crate) async fn load_a2a_tasks(path: &Path) -> HashMap<String, Value> {
    let path = match resolve_a2a_task_ledger_path(path).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "failed to resolve A2A task ledger {}: {error}",
                path.display()
            );
            return HashMap::new();
        }
    };
    let Some(parsed) = read_a2a_task_ledger_value(&path).await else {
        return HashMap::new();
    };
    a2a_task_ledger_entries(&parsed)
        .into_iter()
        .filter_map(|entry| a2a_task_from_ledger_entry(&entry))
        .filter_map(|task| {
            let task_id = task.get("id").and_then(Value::as_str)?.trim().to_string();
            (!task_id.is_empty()).then_some((task_id, task))
        })
        .collect()
}

async fn read_a2a_task_ledger_value(path: &Path) -> Option<Value> {
    let store_path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || maestro_a2a_ledger::load(&store_path)).await {
        Ok(Ok(parsed)) => Some(parsed),
        Ok(Err(error)) => {
            eprintln!(
                "failed to read A2A task database {}: {error}",
                path.display()
            );
            None
        }
        Err(error) => {
            eprintln!(
                "failed to join A2A task database read {}: {error}",
                path.display()
            );
            None
        }
    }
}

fn a2a_task_ledger_entries(ledger: &Value) -> Vec<Value> {
    ledger
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn a2a_task_from_ledger_entry(entry: &Value) -> Option<Value> {
    let peer = entry.get("peer").and_then(Value::as_str);
    if peer.is_some_and(|peer| !a2a_ledger_peer_is_runtime_gateway(peer)) {
        return None;
    }
    if let Some(task) = entry.get("a2aTask").and_then(Value::as_object) {
        let task = Value::Object(task.clone());
        if task.get("id").and_then(Value::as_str).is_some() {
            return Some(a2a_task_with_ledger_evidence(task, entry));
        }
    }
    if entry.get("id").and_then(Value::as_str).is_some()
        && entry.get("status").and_then(Value::as_object).is_some()
    {
        return Some(entry.clone());
    }
    if !peer.is_some_and(a2a_ledger_peer_is_runtime_gateway) {
        return None;
    }
    let task_id = entry.get("taskId").and_then(Value::as_str)?;
    let context_id = entry
        .get("contextId")
        .and_then(Value::as_str)
        .unwrap_or("maestro-runtime-gateway");
    let state = entry
        .get("state")
        .and_then(Value::as_str)
        .map(canonical_a2a_task_state)
        .unwrap_or_else(|| "TASK_STATE_UNKNOWN".to_string());
    let updated_at = entry
        .get("updatedAt")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(now_rfc3339);
    let status_message_text = entry
        .get("responseText")
        .and_then(Value::as_str)
        .or_else(|| entry.get("text").and_then(Value::as_str))
        .unwrap_or("Restored A2A task from Maestro ledger.");
    let status_message = a2a_agent_message(context_id, status_message_text);
    let history = entry
        .get("transcript")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| a2a_message_from_ledger_transcript(context_id, item))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let metadata = a2a_task_metadata_from_ledger_entry(entry);
    let mut task = a2a_task_value(
        task_id,
        context_id,
        &state,
        status_message,
        history,
        Vec::new(),
        metadata,
    );
    task["status"]["timestamp"] = Value::String(updated_at);
    Some(task)
}

fn a2a_task_metadata_from_ledger_entry(entry: &Value) -> Value {
    let mut metadata = entry
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    a2a_merge_ledger_work_graph_into_metadata(&mut metadata, entry);
    metadata
}

fn a2a_task_with_ledger_evidence(mut task: Value, entry: &Value) -> Value {
    let context_id = task
        .get("contextId")
        .and_then(Value::as_str)
        .or_else(|| entry.get("contextId").and_then(Value::as_str))
        .unwrap_or("maestro-runtime-gateway")
        .to_string();
    let Some(task_object) = task.as_object_mut() else {
        return task;
    };
    let metadata = task_object
        .entry("metadata".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    a2a_merge_ledger_work_graph_into_metadata(metadata, entry);
    a2a_merge_ledger_status_into_task(task_object, entry, &context_id);
    if let Some(history) = a2a_history_from_ledger_transcript(&context_id, entry) {
        a2a_merge_ledger_history_into_task(task_object, history);
    }
    task
}

fn a2a_history_from_ledger_transcript(context_id: &str, entry: &Value) -> Option<Vec<Value>> {
    let history = entry
        .get("transcript")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| a2a_message_from_ledger_transcript(context_id, item))
        .collect::<Vec<_>>();
    (!history.is_empty()).then_some(history)
}

fn a2a_merge_ledger_history_into_task(
    task_object: &mut Map<String, Value>,
    ledger_history: Vec<Value>,
) {
    let history = task_object
        .entry("history".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !history.is_array() {
        *history = Value::Array(ledger_history);
        return;
    }
    let Some(history_array) = history.as_array_mut() else {
        return;
    };
    for message in ledger_history {
        a2a_merge_ledger_message_into_history(history_array, message);
    }
}

fn a2a_merge_ledger_message_into_history(history: &mut Vec<Value>, candidate: Value) {
    if let Some(message_id) = candidate.get("messageId").and_then(Value::as_str) {
        if let Some(index) = history.iter().position(|message| {
            message
                .get("messageId")
                .and_then(Value::as_str)
                .is_some_and(|existing_id| existing_id == message_id)
        }) {
            if let Some(existing_message) = history.get_mut(index) {
                a2a_refresh_history_message(existing_message, &candidate);
            }
            return;
        }
    }
    if !history
        .iter()
        .any(|message| a2a_messages_have_same_role_text(message, &candidate))
    {
        history.push(candidate);
    }
}

fn a2a_messages_have_same_role_text(existing: &Value, candidate: &Value) -> bool {
    let candidate_role = candidate
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let candidate_text = a2a_message_value_text(candidate);
    existing
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role.eq_ignore_ascii_case(candidate_role))
        && a2a_message_value_text(existing) == candidate_text
}

fn a2a_refresh_history_message(existing: &mut Value, candidate: &Value) {
    if a2a_messages_have_same_role_text(existing, candidate) {
        return;
    }
    if let (Some(existing_object), Some(candidate_object)) =
        (existing.as_object_mut(), candidate.as_object())
    {
        for (key, value) in candidate_object {
            if key == "parts" {
                // Transcript-derived candidates only carry plain-text parts.
                // Merge so attachment or file parts already stored on the
                // embedded history message are preserved while the plain-text
                // part is refreshed from the candidate.
                let merged = a2a_merge_history_parts(existing_object.get("parts"), value);
                existing_object.insert("parts".to_string(), merged);
                continue;
            }
            existing_object.insert(key.clone(), value.clone());
        }
        return;
    }
    *existing = candidate.clone();
}

/// Merge existing history parts with a transcript-derived candidate. Non-text
/// parts (attachments, data payloads) are preserved from the existing message;
/// plain-text parts are refreshed from the candidate so a stale summary does
/// not outlive a top-level responseText or status update.
fn a2a_merge_history_parts(existing: Option<&Value>, candidate: &Value) -> Value {
    let Some(existing_parts) = existing.and_then(Value::as_array) else {
        return candidate.clone();
    };
    let candidate_parts = candidate.as_array();
    // Preserve existing parts that carry no text (attachments, data payloads).
    // Drop existing parts that have a `text` field — whether pure plain-text or
    // decorated with extra keys like `partId` — so stale summary text does not
    // survive alongside the refreshed transcript text.
    let mut merged: Vec<Value> = existing_parts
        .iter()
        .filter(|part| !a2a_history_part_has_text(part))
        .cloned()
        .collect();
    if let Some(candidate_parts) = candidate_parts {
        for part in candidate_parts {
            if a2a_history_part_is_plain_text(part) {
                merged.push(part.clone());
            }
        }
    }
    Value::Array(merged)
}

/// True when the part has a `text` field (plain or decorated). Used to decide
/// which existing parts to drop during a transcript-driven refresh.
fn a2a_history_part_has_text(part: &Value) -> bool {
    part.as_object()
        .is_some_and(|object| object.get("text").is_some_and(Value::is_string))
}

fn a2a_history_part_is_plain_text(part: &Value) -> bool {
    let Some(object) = part.as_object() else {
        return false;
    };
    if !object.get("text").is_some_and(Value::is_string) {
        return false;
    }
    match object.len() {
        1 => true,
        2 => object
            .get("mediaType")
            .and_then(Value::as_str)
            .is_some_and(|media_type| media_type.eq_ignore_ascii_case("text/plain")),
        _ => false,
    }
}

fn a2a_merge_ledger_work_graph_into_metadata(metadata: &mut Value, entry: &Value) {
    let Some(work_graph) = entry
        .get("workGraph")
        .filter(|work_graph| !work_graph.is_null())
        .filter(|work_graph| a2a_ledger_work_graph_has_evidence(work_graph))
        .cloned()
    else {
        return;
    };
    if !metadata.is_object() {
        *metadata = Value::Object(Map::new());
    }
    if let Some(metadata_object) = metadata.as_object_mut() {
        metadata_object.insert("workGraph".to_string(), work_graph);
    }
}

fn a2a_ledger_work_graph_has_evidence(work_graph: &Value) -> bool {
    let Some(object) = work_graph.as_object() else {
        return false;
    };
    let string_field = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    let number_field = |key: &str| object.get(key).and_then(Value::as_u64).is_some();
    let array_field = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|values| !values.is_empty())
    };
    string_field("state")
        || number_field("itemCount")
        || number_field("activeItemCount")
        || number_field("blockedItemCount")
        || number_field("waitingItemCount")
        || number_field("childRunCount")
        || array_field("childRunIds")
        || number_field("toolCallCount")
        || number_field("pendingToolCallCount")
        || array_field("toolExecutionIds")
        || number_field("waitItemCount")
        || array_field("waitIds")
        || object
            .get("stateCounts")
            .and_then(Value::as_object)
            .is_some_and(|counts| !counts.is_empty())
        || string_field("correlationPath")
        || object
            .get("codexSubagents")
            .is_some_and(a2a_codex_subagent_work_graph_has_evidence)
}

fn a2a_codex_subagent_work_graph_has_evidence(work_graph: &Value) -> bool {
    let Some(object) = work_graph.as_object() else {
        return false;
    };
    ["toolCallIds", "childRunIds", "threadIds", "edges"]
        .iter()
        .any(|key| {
            object
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|values| !values.is_empty())
        })
        || object.get("edgeCount").and_then(Value::as_u64).is_some()
}

fn a2a_merge_ledger_status_into_task(
    task_object: &mut Map<String, Value>,
    entry: &Value,
    context_id: &str,
) {
    let Some(state) = entry.get("state").and_then(Value::as_str) else {
        return;
    };
    let response_text = entry
        .get("responseText")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|response_text| !response_text.is_empty())
        .map(str::to_string)
        .or_else(|| a2a_latest_agent_transcript_text(entry))
        .or_else(|| a2a_latest_agent_history_text(task_object));
    let status = task_object
        .entry("status".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !status.is_object() {
        *status = Value::Object(Map::new());
    }
    let Some(status_object) = status.as_object_mut() else {
        return;
    };
    status_object.insert(
        "state".to_string(),
        Value::String(canonical_a2a_task_state(state)),
    );
    if let Some(updated_at) = entry.get("updatedAt").and_then(Value::as_str) {
        status_object.insert(
            "timestamp".to_string(),
            Value::String(updated_at.to_string()),
        );
    }
    if let Some(response_text) = response_text {
        status_object.insert(
            "message".to_string(),
            a2a_agent_message(context_id, &response_text),
        );
    }
}

/// Latest agent message text from the embedded task history. Used as a
/// fallback so a reloaded ledger row does not keep a stale "still working"
/// status message after the top-level state has advanced to completion.
fn a2a_latest_agent_history_text(task_object: &Map<String, Value>) -> Option<String> {
    task_object
        .get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| {
            message
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| {
                    role.eq_ignore_ascii_case("ROLE_AGENT") || role.eq_ignore_ascii_case("agent")
                })
        })
        .and_then(a2a_message_value_text)
}

fn a2a_latest_agent_transcript_text(entry: &Value) -> Option<String> {
    entry
        .get("transcript")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|item| {
            item.get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| {
                    role.eq_ignore_ascii_case("agent") || role.eq_ignore_ascii_case("ROLE_AGENT")
                })
        })
        .and_then(|item| item.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn a2a_message_from_ledger_transcript(context_id: &str, item: &Value) -> Option<Value> {
    let text = item.get("text").and_then(Value::as_str)?.trim();
    if text.is_empty() {
        return None;
    }
    let role = match item.get("role").and_then(Value::as_str) {
        Some(role)
            if role.eq_ignore_ascii_case("ROLE_AGENT") || role.eq_ignore_ascii_case("agent") =>
        {
            "ROLE_AGENT"
        }
        _ => "ROLE_USER",
    };
    let message_id = item
        .get("messageId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| generate_a2a_id("maestro-message"));
    Some(serde_json::json!({
        "messageId": message_id,
        "contextId": context_id,
        "role": role,
        "parts": [{ "text": text, "mediaType": "text/plain" }]
    }))
}

pub(crate) async fn persist_a2a_tasks(state: &AppState) {
    let _guard = state.a2a_task_persist_lock.lock().await;
    let path = match resolve_a2a_task_ledger_path(&state.config.a2a_tasks_file_path).await {
        Ok(path) => path,
        Err(error) => {
            eprintln!("failed to resolve A2A task ledger: {error}");
            return;
        }
    };
    let file_lock = match acquire_a2a_task_ledger_file_lock(&path).await {
        Ok(file_lock) => file_lock,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    let heartbeat_task =
        spawn_a2a_task_ledger_lock_heartbeat(&file_lock, a2a_task_ledger_lock_heartbeat_interval());
    let result = persist_a2a_tasks_locked(state, &path, &file_lock).await;
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    release_a2a_task_ledger_file_lock(file_lock).await;
    if let Err(error) = result {
        eprintln!("{error}");
    }
}

pub(crate) async fn persist_a2a_tasks_locked(
    state: &AppState,
    path: &Path,
    file_lock: &A2ATaskLedgerFileLock,
) -> Result<(), String> {
    if !a2a_task_ledger_lock_is_owned(&file_lock.path, &file_lock.token).await {
        return Err(format!(
            "lost A2A task ledger lock ownership before updating {}",
            path.display()
        ));
    }
    let tasks = state
        .a2a_tasks
        .lock()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        maestro_a2a_ledger::update(&path, |existing_ledger| {
            merge_runtime_gateway_tasks(existing_ledger, &tasks);
            Ok(())
        })
    })
    .await
    .map_err(|error| format!("failed to join A2A task database write: {error}"))?
    .map_err(|error| format!("failed to write A2A task database: {error}"))
}

fn merge_runtime_gateway_tasks(existing_ledger: &mut Value, tasks: &[Value]) {
    let existing_entries = a2a_task_ledger_entries(existing_ledger);
    let local_task_ids = tasks
        .iter()
        .filter_map(|task| task.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let mut retained_entries = existing_entries
        .iter()
        .filter(|entry| {
            if a2a_ledger_entry_is_raw_a2a_task(entry) {
                return false;
            }
            if a2a_ledger_entry_is_runtime_gateway(entry) {
                let task_id = ledger_entry_task_id(entry);
                if task_id.is_empty() {
                    return true;
                }
                return !local_task_ids.contains(&task_id);
            }
            true
        })
        .cloned()
        .collect::<Vec<_>>();
    let existing_runtime_gateway_entries = existing_entries
        .into_iter()
        .filter(a2a_ledger_entry_is_runtime_gateway)
        .filter_map(|entry| {
            let task_id = entry.get("taskId").and_then(Value::as_str)?.to_string();
            Some((task_id, entry))
        })
        .collect::<HashMap<_, _>>();
    let mut runtime_gateway_entries = tasks
        .iter()
        .cloned()
        .filter_map(|task| {
            let task_id = task.get("id").and_then(Value::as_str)?;
            let existing = existing_runtime_gateway_entries.get(task_id);
            Some(a2a_ledger_entry_from_task(&task, existing))
        })
        .collect::<Vec<_>>();
    retained_entries.append(&mut runtime_gateway_entries);
    retained_entries.sort_by(|left, right| {
        ledger_entry_updated_at(left)
            .cmp(ledger_entry_updated_at(right))
            .then_with(|| ledger_entry_task_id(left).cmp(ledger_entry_task_id(right)))
    });
    let mut persisted_ledger = match existing_ledger.take() {
        Value::Object(entries) => entries,
        _ => Map::new(),
    };
    persisted_ledger.insert("tasks".to_string(), Value::Array(retained_entries));
    *existing_ledger = Value::Object(persisted_ledger);
}

async fn ensure_a2a_task_ledger_directory(path: &Path) -> Result<(), String> {
    let mut create_result = tokio::fs::create_dir(path).await;
    if create_result
        .as_ref()
        .is_err_and(|error| error.kind() == ErrorKind::NotFound)
    {
        if let Some(ancestor) = path.parent() {
            tokio::fs::create_dir_all(ancestor).await.map_err(|error| {
                format!(
                    "failed to create A2A task ledger directory {}: {error}",
                    ancestor.display()
                )
            })?;
        }
        create_result = tokio::fs::create_dir(path).await;
    }

    match create_result {
        Ok(()) => restrict_a2a_task_ledger_directory(path).await,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = tokio::fs::metadata(path).await.map_err(|metadata_error| {
                format!(
                    "failed to inspect A2A task ledger directory {}: {metadata_error}",
                    path.display()
                )
            })?;
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(format!(
                    "A2A task ledger parent {} is not a directory",
                    path.display()
                ))
            }
        }
        Err(error) => Err(format!(
            "failed to create A2A task ledger directory {}: {error}",
            path.display()
        )),
    }
}

async fn restrict_a2a_task_ledger_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|error| {
                format!(
                    "failed to restrict A2A task ledger directory {}: {error}",
                    path.display()
                )
            })?;
    }
    Ok(())
}
pub(crate) struct A2ATaskLedgerFileLock {
    pub(crate) path: PathBuf,
    pub(crate) token: String,
    /// Advisory lock on the owner inode. Stale reclaimers acquire the same
    /// fence before removing the directory, closing the check-to-rename race.
    owner_file: Option<File>,
    #[cfg(test)]
    release_probe: Option<A2ATaskLedgerReleaseProbe>,
}

#[cfg(test)]
struct A2ATaskLedgerReleaseProbe {
    checked: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

pub(crate) async fn acquire_a2a_task_ledger_file_lock(
    path: &Path,
) -> Result<A2ATaskLedgerFileLock, String> {
    let path = resolve_a2a_task_ledger_path(path).await?;
    if let Some(parent) = path.parent() {
        ensure_a2a_task_ledger_directory(parent).await?;
    }
    let lock_path = a2a_task_ledger_lock_path(&path);
    let token = format!(
        "{}:{}",
        process::id(),
        ATTACHMENT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let deadline = Instant::now() + Duration::from_millis(A2A_LEDGER_LOCK_TIMEOUT_MS);
    loop {
        match tokio::fs::create_dir(&lock_path).await {
            Ok(()) => {
                if let Err(error) = restrict_a2a_task_ledger_lock_directory(&lock_path).await {
                    let _ = tokio::fs::remove_dir_all(&lock_path).await;
                    return Err(error);
                }
                if let Err(error) = write_a2a_task_ledger_lock_metadata(&lock_path, &token).await {
                    let _ = tokio::fs::remove_dir_all(&lock_path).await;
                    return Err(error);
                }
                // A contender can observe the owner file between its
                // creation and the creator's first non-blocking flock. Keep
                // the directory owner and retry rather than deleting a lock
                // whose inode another process may already have fenced.
                let owner_file = loop {
                    match try_acquire_a2a_task_ledger_owner_fence(&lock_path) {
                        Ok(Some(owner_file)) => break owner_file,
                        Ok(None) if Instant::now() < deadline => {
                            tokio::time::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS))
                                .await;
                        }
                        Ok(None) => {
                            return Err(format!(
                                "timed out acquiring A2A task ledger lock {} owner fence",
                                lock_path.display()
                            ));
                        }
                        Err(error) => {
                            let _ = tokio::fs::remove_dir_all(&lock_path).await;
                            return Err(error);
                        }
                    }
                };
                return Ok(A2ATaskLedgerFileLock {
                    path: lock_path,
                    token,
                    owner_file: Some(owner_file),
                    #[cfg(test)]
                    release_probe: None,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = match tokio::fs::symlink_metadata(&lock_path).await {
                    Ok(metadata) => metadata,
                    // The owner released the lock between our create_dir
                    // attempt and this probe, so the path is free again.
                    // Retry the atomic create instead of failing the write.
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        if Instant::now() >= deadline {
                            return Err(format!(
                                "timed out waiting for A2A task ledger lock {}",
                                lock_path.display()
                            ));
                        }
                        continue;
                    }
                    Err(metadata_error) => {
                        return Err(format!(
                            "failed to inspect A2A task ledger lock {}: {metadata_error}",
                            lock_path.display()
                        ));
                    }
                };
                if !metadata.is_dir() {
                    return Err(format!(
                        "A2A task ledger lock {} is not the shared directory protocol",
                        lock_path.display()
                    ));
                }
                let owner_file = try_acquire_a2a_task_ledger_owner_fence(&lock_path)?;
                if owner_file.is_none()
                    && tokio::fs::metadata(lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE))
                        .await
                        .is_ok()
                {
                    // A live writer owns the advisory inode fence, even if
                    // heartbeat metadata is old. Do not reclaim it between
                    // its final ownership check and atomic rename.
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "timed out waiting for A2A task ledger lock {}",
                            lock_path.display()
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS)).await;
                    continue;
                }
                if a2a_task_ledger_lock_is_stale(&lock_path).await? {
                    // Keep owner_file in scope while removing the stale
                    // directory; dropping it releases the inode fence only
                    // after the remove has completed.
                    match tokio::fs::remove_dir_all(&lock_path).await {
                        Ok(()) => continue,
                        Err(remove_error)
                            if remove_error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(remove_error) => {
                            return Err(format!(
                                "failed to remove stale A2A task ledger lock {}: {remove_error}",
                                lock_path.display()
                            ));
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out waiting for A2A task ledger lock {}",
                        lock_path.display()
                    ));
                }
                #[cfg(test)]
                let _ = LOCK_RETRY_PROBE.try_with(|probe| {
                    if let Some(waiting) = probe.borrow_mut().take() {
                        let _ = waiting.send(());
                    }
                });
                tokio::time::sleep(Duration::from_millis(A2A_LEDGER_LOCK_RETRY_MS)).await;
            }
            Err(error) => {
                return Err(format!(
                    "failed to acquire A2A task ledger lock {}: {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

fn try_acquire_a2a_task_ledger_owner_fence(lock_path: &Path) -> Result<Option<File>, String> {
    let owner_path = lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE);
    let file = match OpenOptions::new().read(true).write(true).open(&owner_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to open A2A task ledger lock fence {}: {error}",
                owner_path.display()
            ));
        }
    };
    #[cfg(unix)]
    {
        // The retry path must remain nonblocking; callers wait using the
        // shared bounded retry interval when a live owner holds the fence.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Some(file));
        }
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK)
        {
            return Ok(None);
        }
        Err(format!(
            "failed to acquire A2A task ledger lock fence {}: {error}",
            owner_path.display()
        ))
    }
    #[cfg(not(unix))]
    {
        // Token revalidation remains the fallback on platforms without flock.
        Ok(Some(file))
    }
}

async fn restrict_a2a_task_ledger_lock_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|error| {
                format!(
                    "failed to restrict A2A task ledger lock permissions {}: {error}",
                    path.display()
                )
            })?;
    }
    Ok(())
}

async fn write_a2a_task_ledger_lock_metadata(lock_path: &Path, token: &str) -> Result<(), String> {
    tokio::fs::write(
        lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE),
        format!("{token}\n"),
    )
    .await
    .map_err(|error| {
        format!(
            "failed to write A2A task ledger lock owner {}: {error}",
            lock_path.display()
        )
    })?;
    restrict_a2a_task_ledger_lock_metadata(&lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE)).await?;
    write_a2a_task_ledger_lock_heartbeat(lock_path).await
}

async fn write_a2a_task_ledger_lock_heartbeat(lock_path: &Path) -> Result<(), String> {
    tokio::fs::write(
        lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE),
        format!("{}\n", unix_millis_now()),
    )
    .await
    .map_err(|error| {
        format!(
            "failed to write A2A task ledger lock heartbeat {}: {error}",
            lock_path.display()
        )
    })?;
    restrict_a2a_task_ledger_lock_metadata(&lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE)).await
}

async fn restrict_a2a_task_ledger_lock_metadata(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|error| {
                format!(
                    "failed to restrict A2A task ledger lock metadata {}: {error}",
                    path.display()
                )
            })?;
    }
    Ok(())
}

fn a2a_task_ledger_lock_heartbeat_interval() -> Duration {
    Duration::from_millis(
        (A2A_LEDGER_LOCK_STALE_MS / 3)
            .max(A2A_LEDGER_LOCK_RETRY_MS)
            .max(1),
    )
}

pub(crate) fn spawn_a2a_task_ledger_lock_heartbeat(
    file_lock: &A2ATaskLedgerFileLock,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let lock_path = file_lock.path.clone();
    let token = file_lock.token.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if !a2a_task_ledger_lock_is_owned(&lock_path, &token).await {
                break;
            }
            let _ = write_a2a_task_ledger_lock_heartbeat(&lock_path).await;
        }
    })
}

pub(crate) async fn release_a2a_task_ledger_file_lock(file_lock: A2ATaskLedgerFileLock) {
    let owned = a2a_task_ledger_lock_is_owned(&file_lock.path, &file_lock.token).await;
    let A2ATaskLedgerFileLock {
        path,
        token: _,
        owner_file,
        #[cfg(test)]
        release_probe,
    } = file_lock;
    if owned {
        #[cfg(test)]
        if let Some(probe) = release_probe {
            let _ = probe.checked.send(());
            let _ = probe.resume.await;
        }
        let _ = tokio::fs::remove_dir_all(&path).await;
    }
    // Keep the owner inode fence held until remove_dir_all has completed.
    // Releasing it earlier permits an ABA waiter to recreate the same lock
    // path before this release finishes and then removes the new owner's lock.
    drop(owner_file);
}

async fn a2a_task_ledger_lock_is_owned(lock_path: &Path, token: &str) -> bool {
    tokio::fs::read_to_string(lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE))
        .await
        .map(|owner| owner.trim() == token)
        .unwrap_or(false)
}

async fn a2a_task_ledger_lock_is_stale(lock_path: &Path) -> Result<bool, String> {
    let Some(modified_at) = a2a_task_ledger_lock_modified_at(lock_path).await? else {
        return Ok(true);
    };
    SystemTime::now()
        .duration_since(modified_at)
        .map(|age| age > Duration::from_millis(A2A_LEDGER_LOCK_STALE_MS))
        .map_err(|error| {
            format!(
                "failed to inspect A2A task ledger lock age {}: {error}",
                lock_path.display()
            )
        })
}

async fn a2a_task_ledger_lock_modified_at(lock_path: &Path) -> Result<Option<SystemTime>, String> {
    for path in [
        lock_path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE),
        lock_path.join(A2A_LEDGER_LOCK_OWNER_FILE),
        lock_path.to_path_buf(),
    ] {
        match tokio::fs::metadata(&path).await {
            Ok(metadata) => {
                return metadata.modified().map(Some).map_err(|error| {
                    format!(
                        "failed to inspect A2A task ledger lock metadata {}: {error}",
                        path.display()
                    )
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "failed to inspect A2A task ledger lock metadata {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(None)
}

pub(crate) fn a2a_task_ledger_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}
fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn a2a_ledger_peer_is_runtime_gateway(peer: &str) -> bool {
    matches!(
        peer,
        A2A_RUNTIME_GATEWAY_LEDGER_PEER | A2A_LEGACY_CONTROL_PLANE_LEDGER_PEER
    )
}

fn a2a_ledger_entry_is_runtime_gateway(entry: &Value) -> bool {
    entry
        .get("peer")
        .and_then(Value::as_str)
        .is_some_and(a2a_ledger_peer_is_runtime_gateway)
        || (entry.get("peer").is_none()
            && entry.get("id").and_then(Value::as_str).is_some()
            && entry.get("status").and_then(Value::as_object).is_some())
}

fn a2a_ledger_entry_is_raw_a2a_task(entry: &Value) -> bool {
    entry.get("peer").is_none()
        && entry.get("taskId").is_none()
        && entry.get("id").and_then(Value::as_str).is_some()
        && entry.get("status").and_then(Value::as_object).is_some()
}

fn a2a_ledger_entry_from_task(task: &Value, existing: Option<&Value>) -> Value {
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown-task");
    let context_id = task.get("contextId").and_then(Value::as_str);
    let state = a2a_task_status_state(task).unwrap_or("TASK_STATE_UNKNOWN");
    let updated_at = a2a_task_status_timestamp(task)
        .map(str::to_string)
        .unwrap_or_else(now_rfc3339);
    let transcript = a2a_task_transcript(task, state, &updated_at);
    let text = transcript
        .iter()
        .find(|entry| entry.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|entry| entry.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            existing
                .and_then(|entry| entry.get("text").and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("A2A task {task_id}"));
    let response_text = a2a_task_response_text(task);
    let created_at = existing
        .and_then(|entry| entry.get("createdAt").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            transcript
                .first()
                .and_then(|entry| entry.get("at").and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| updated_at.clone());
    let metadata = a2a_clean_ledger_metadata(task.get("metadata"));
    let work_graph = task
        .get("metadata")
        .and_then(|metadata| metadata.get("workGraph"))
        .cloned();
    let ledger_task = a2a_task_for_ledger(task);
    let mut entry = serde_json::json!({
        "id": existing
            .and_then(|entry| entry.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| format!("maestro-runtime-gateway-{task_id}")),
        "kind": "delegation",
        "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
        "peerDisplayName": A2A_RUNTIME_GATEWAY_LEDGER_DISPLAY_NAME,
        "taskId": task_id,
        "text": text,
        "state": state,
        "transcript": transcript,
        "createdAt": created_at,
        "updatedAt": updated_at,
        "metadata": metadata,
        "a2aTask": ledger_task
    });
    if let Some(context_id) = context_id {
        entry["contextId"] = Value::String(context_id.to_string());
    }
    if let Some(message_id) = a2a_task_first_user_message_id(task) {
        entry["messageId"] = Value::String(message_id);
    }
    if let Some(response_text) = response_text {
        entry["responseText"] = Value::String(response_text);
    }
    if let Some(work_graph) = work_graph {
        entry["workGraph"] = work_graph;
    }
    if a2a_task_is_terminal(task) {
        entry["completedAt"] = entry["updatedAt"].clone();
    }
    if let Some(existing) = existing {
        a2a_preserve_ledger_extensions(existing, &mut entry);
    }
    entry
}

/// Keep fields introduced by a newer Gateway/TUI release when the current
/// Gateway regenerates its typed projection. Generated canonical fields win;
/// only keys absent from the new projection are copied from the old row.
fn a2a_preserve_ledger_extensions(existing: &Value, generated: &mut Value) {
    let (Some(existing_object), Some(generated_object)) =
        (existing.as_object(), generated.as_object_mut())
    else {
        return;
    };
    for (key, value) in existing_object {
        generated_object
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
    for key in ["a2aTask"] {
        if let (Some(existing_nested), Some(generated_nested)) = (
            existing_object.get(key).and_then(Value::as_object),
            generated_object.get_mut(key).and_then(Value::as_object_mut),
        ) {
            for (nested_key, value) in existing_nested {
                generated_nested
                    .entry(nested_key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }
    if let (Some(existing_transcript), Some(generated_transcript)) = (
        existing_object.get("transcript").and_then(Value::as_array),
        generated_object
            .get_mut("transcript")
            .and_then(Value::as_array_mut),
    ) {
        for (existing_item, generated_item) in existing_transcript.iter().zip(generated_transcript)
        {
            let (Some(existing_item), Some(generated_item)) =
                (existing_item.as_object(), generated_item.as_object_mut())
            else {
                continue;
            };
            for (key, value) in existing_item {
                generated_item
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }
}

fn a2a_task_for_ledger(task: &Value) -> Value {
    let task_is_terminal = a2a_task_is_terminal(task);
    let mut task = task.clone();
    if let Some(task_object) = task.as_object_mut() {
        if let Some(metadata) = task_object.get("metadata") {
            let mut ledger_metadata = a2a_embedded_task_metadata_for_ledger(metadata);
            if !task_is_terminal {
                // In-flight tasks need callback delivery config after restart; terminal rows do not.
                if let Some(push_configs) = metadata.get(A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY)
                {
                    ledger_metadata[A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY] =
                        push_configs.clone();
                }
            }
            task_object.insert("metadata".to_string(), ledger_metadata);
        }
    }
    task
}

fn a2a_embedded_task_metadata_for_ledger(metadata: &Value) -> Value {
    let mut metadata = metadata
        .as_object()
        .map(|_| metadata.clone())
        .unwrap_or_else(|| Value::Object(Map::new()));
    a2a_remove_secret_ledger_metadata(&mut metadata);
    metadata
}

fn a2a_remove_secret_ledger_metadata(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| !a2a_ledger_metadata_key_is_secret(key));
            for value in object.values_mut() {
                a2a_remove_secret_ledger_metadata(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                a2a_remove_secret_ledger_metadata(value);
            }
        }
        _ => {}
    }
}

fn a2a_task_transcript(task: &Value, state: &str, updated_at: &str) -> Vec<Value> {
    task.get("history")
        .and_then(Value::as_array)
        .map(|history| {
            history
                .iter()
                .filter_map(|message| a2a_transcript_entry_from_message(message, state, updated_at))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn a2a_transcript_entry_from_message(
    message: &Value,
    state: &str,
    updated_at: &str,
) -> Option<Value> {
    let text = a2a_message_value_text(message)?;
    let role = match message.get("role").and_then(Value::as_str) {
        Some(role)
            if role.eq_ignore_ascii_case("ROLE_AGENT") || role.eq_ignore_ascii_case("agent") =>
        {
            "agent"
        }
        _ => "user",
    };
    let mut entry = serde_json::json!({
        "at": updated_at,
        "role": role,
        "text": text
    });
    if role == "agent" {
        entry["state"] = Value::String(state.to_string());
    }
    if let Some(message_id) = message.get("messageId").and_then(Value::as_str) {
        entry["messageId"] = Value::String(message_id.to_string());
    }
    Some(entry)
}

fn a2a_task_response_text(task: &Value) -> Option<String> {
    task.get("status")
        .and_then(|status| status.get("message"))
        .and_then(a2a_message_value_text)
        .or_else(|| {
            task.get("artifacts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|artifact| {
                    artifact
                        .get("parts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .map(str::trim)
                .find(|text| !text.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            task.get("history")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .rev()
                .find(|message| {
                    message
                        .get("role")
                        .and_then(Value::as_str)
                        .is_some_and(|role| {
                            role.eq_ignore_ascii_case("ROLE_AGENT")
                                || role.eq_ignore_ascii_case("agent")
                        })
                })
                .and_then(a2a_message_value_text)
        })
}

fn a2a_message_value_text(message: &Value) -> Option<String> {
    let text = message
        .get("parts")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn a2a_task_first_user_message_id(task: &Value) -> Option<String> {
    task.get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|message| {
            message
                .get("role")
                .and_then(Value::as_str)
                .is_none_or(|role| {
                    role.eq_ignore_ascii_case("ROLE_USER") || role.eq_ignore_ascii_case("user")
                })
        })
        .and_then(|message| message.get("messageId").and_then(Value::as_str))
        .map(str::to_string)
}

fn a2a_clean_ledger_metadata(metadata: Option<&Value>) -> Value {
    let object = metadata
        .and_then(Value::as_object)
        .map(|metadata| {
            metadata
                .iter()
                .filter_map(|(key, value)| match value {
                    Value::String(_) | Value::Number(_) | Value::Bool(_)
                        if !a2a_ledger_metadata_key_is_secret(key) =>
                    {
                        Some((key.clone(), value.clone()))
                    }
                    _ => None,
                })
                .collect::<Map<_, _>>()
        })
        .unwrap_or_default();
    Value::Object(object)
}

fn a2a_ledger_metadata_key_is_secret(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_')
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if a2a_ledger_metadata_key_is_token_metric(&normalized) {
        return false;
    }
    // Explicit sensitive aliases redact regardless of position in the key.
    matches!(
        normalized.as_str(),
        "authorization"
            | "token"
            | "apitoken"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "authtoken"
            | "secret"
            | "clientsecret"
            | "sharedsecret"
            | "password"
            | "apikey"
            | "credentials"
            | "bearer"
    ) || a2a_ledger_metadata_key_has_secret_suffix(&normalized)
        || key == A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY
}

/// Catch compound credential field names such as `webhookSecret`,
/// `oauthToken`, or `apiPassword` that the exact-match list above would miss.
/// Explicitly exclude negated names (`nonSecret`, `nonCredentials`,
/// `notASecret`) so benign audit metadata is not stripped, and keep the
/// token-metric carve-out above authoritative for token-count fields.
fn a2a_ledger_metadata_key_has_secret_suffix(normalized: &str) -> bool {
    const SECRET_SUFFIXES: [&str; 5] = ["secret", "token", "password", "apikey", "credentials"];
    const NEGATION_PREFIXES: [&str; 3] = ["non", "not", "no"];
    let Some(stem) = SECRET_SUFFIXES
        .iter()
        .find_map(|suffix| normalized.strip_suffix(suffix))
    else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    !NEGATION_PREFIXES
        .iter()
        .any(|prefix| stem.starts_with(prefix))
}

fn a2a_ledger_metadata_key_is_token_metric(normalized: &str) -> bool {
    matches!(
        normalized,
        "totaltoken"
            | "totaltokens"
            | "inputtoken"
            | "inputtokens"
            | "outputtoken"
            | "outputtokens"
            | "cachetoken"
            | "cachetokens"
            | "cachereadtoken"
            | "cachereadtokens"
            | "cachewritetoken"
            | "cachewritetokens"
            | "prompttoken"
            | "prompttokens"
            | "completiontoken"
            | "completiontokens"
            | "maxtoken"
            | "maxtokens"
            | "tokencount"
            | "tokenscount"
    )
}

fn ledger_entry_updated_at(entry: &Value) -> &str {
    entry
        .get("updatedAt")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn ledger_entry_task_id(entry: &Value) -> &str {
    entry
        .get("taskId")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn a2a_task_store_waits_for_shared_ledger_file_lock() {
        // Bound a stuck fixture as a whole, without making disk speed part of
        // the lock-ordering contract. The production lock deadline is shorter.
        tokio::time::timeout(Duration::from_secs(60), async {
            let root = tempfile::tempdir().expect("test directory should be created");
            let mut state = crate::tests::test_app_state_with_sessions(HashMap::new());
            let mut config = (*state.config).clone();
            config.a2a_tasks_file_path = root.path().join("tasks.json");
            state.config = std::sync::Arc::new(config);
            let lock_path = a2a_task_ledger_lock_path(&state.config.a2a_tasks_file_path);
            tokio::fs::create_dir_all(&lock_path)
                .await
                .expect("test lock should be created");
            let task = a2a_task_value(
                "maestro-task-locked",
                "ctx-1",
                "TASK_STATE_COMPLETED",
                a2a_agent_message("ctx-1", "complete"),
                Vec::new(),
                Vec::new(),
                serde_json::json!({}),
            );
            let state_for_store = state.clone();
            let task_for_store = task.clone();
            let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
            let mut store = tokio::spawn(LOCK_RETRY_PROBE.scope(
                std::cell::RefCell::new(Some(waiting_tx)),
                async move {
                    super::super::tasks::store_a2a_task_unless_canceled(
                        &state_for_store,
                        "maestro-task-locked",
                        task_for_store,
                    )
                    .await
                },
            ));

            tokio::select! {
                result = &mut store => panic!("store finished before lock release: {result:?}"),
                waiting = waiting_rx => waiting.expect("store should reach shared lock contention"),
            }
            let database_path =
                maestro_a2a_ledger::database_path(&state.config.a2a_tasks_file_path);
            assert!(
                !tokio::fs::try_exists(&database_path)
                    .await
                    .expect("database existence should be checkable"),
                "persist should wait for the shared ledger lock before writing"
            );
            assert!(
                !store.is_finished(),
                "store should still be waiting for the lock"
            );
            tokio::fs::remove_dir_all(&lock_path)
                .await
                .expect("test lock should be released");
            assert_eq!(store.await.expect("store task should join"), task);

            let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
                .expect("database should be readable after lock release");
            let persisted = ledger["tasks"]
                .as_array()
                .expect("persisted tasks should be an array")
                .iter()
                .find(|entry| entry["taskId"] == "maestro-task-locked")
                .expect("task should be persisted after lock release");
            assert_eq!(persisted["a2aTask"], task);
            assert!(!lock_path.exists(), "store should release its ledger lock");
        })
        .await
        .expect("shared ledger lock fixture should complete");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn ledger_lock_preserves_existing_custom_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let custom_parent = directory.path().join("shared-state");
        tokio::fs::create_dir(&custom_parent).await.unwrap();
        tokio::fs::set_permissions(&custom_parent, std::fs::Permissions::from_mode(0o750))
            .await
            .unwrap();
        let tasks_path = custom_parent.join("tasks.json");

        let file_lock = acquire_a2a_task_ledger_file_lock(&tasks_path)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::metadata(&custom_parent)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750,
            "an existing custom parent belongs to the caller"
        );
        assert_eq!(
            tokio::fs::metadata(&file_lock.path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for metadata_file in [A2A_LEDGER_LOCK_OWNER_FILE, A2A_LEDGER_LOCK_HEARTBEAT_FILE] {
            assert_eq!(
                tokio::fs::metadata(file_lock.path.join(metadata_file))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        release_a2a_task_ledger_file_lock(file_lock).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_fence_blocks_aba_reclaim_before_new_owner() {
        let directory = tempfile::tempdir().unwrap();
        let tasks_path = directory.path().join("tasks.json");
        let mut file_lock = acquire_a2a_task_ledger_file_lock(&tasks_path)
            .await
            .unwrap();
        let lock_path = file_lock.path.clone();
        let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        file_lock.release_probe = Some(A2ATaskLedgerReleaseProbe {
            checked: checked_tx,
            resume: resume_rx,
        });
        let release = tokio::spawn(async move {
            release_a2a_task_ledger_file_lock(file_lock).await;
        });
        checked_rx.await.unwrap();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
        let contender_lock_path = lock_path.clone();
        let contender_tasks_path = tasks_path.clone();
        let contender = tokio::spawn(async move {
            let candidate = try_acquire_a2a_task_ledger_owner_fence(&contender_lock_path).unwrap();
            if let Some(owner_file) = candidate {
                tokio::fs::remove_dir_all(&contender_lock_path)
                    .await
                    .unwrap();
                drop(owner_file);
                let recreated = acquire_a2a_task_ledger_file_lock(&contender_tasks_path)
                    .await
                    .unwrap();
                let _ = ready_tx.send(true);
                let _ = continue_rx.await;
                let survived = tokio::fs::try_exists(&contender_lock_path).await.unwrap();
                release_a2a_task_ledger_file_lock(recreated).await;
                survived
            } else {
                let _ = ready_tx.send(false);
                let _ = continue_rx.await;
                let recreated = acquire_a2a_task_ledger_file_lock(&contender_tasks_path)
                    .await
                    .unwrap();
                let survived = tokio::fs::try_exists(&contender_lock_path).await.unwrap();
                release_a2a_task_ledger_file_lock(recreated).await;
                survived
            }
        });

        let reclaimed_old_inode = ready_rx.await.unwrap();
        resume_tx.send(()).unwrap();
        release.await.unwrap();
        continue_tx.send(()).unwrap();
        let recreated_lock_survived = contender.await.unwrap();
        assert!(!reclaimed_old_inode);
        assert!(recreated_lock_survived);
    }
}
