use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};

use crate::a2a_skill_catalog::A2A_SUBAGENT_REQUEST_METADATA_PATH;
use crate::auth::{
    AuthContext, a2a_task_id_from_cancel_path, a2a_task_id_from_subscribe_path, nonempty_str,
};
use crate::http::{RequestHead, json_response, read_request_body, response};
use crate::{
    AppState, CODEX_SUBAGENT_WORK_GRAPH_SCHEMA, authorized_context, env_u64,
    json_string_from_object, now_millis, now_rfc3339, send_sse, sse_headers, validate_csrf,
};
use maestro_runtime::DelegationEvent;

use super::ledger::persist_a2a_tasks;
use super::push_notifications::{
    a2a_metadata_key_is_reserved, a2a_metadata_with_push_notification_config,
    a2a_push_notification_config_from_send_request, a2a_push_notification_config_path,
    a2a_redact_push_notification_metadata, dispatch_a2a_push_notifications,
    handle_a2a_push_notification_config_create, handle_a2a_push_notification_config_delete,
    handle_a2a_push_notification_config_get, handle_a2a_push_notification_config_list,
};
use super::{
    A2ASubagentExecutionPolicy, A2ATurnResult, CapsuleValidationError,
    ValidatedSubagentTaskCapsule, a2a_agent_card, a2a_extended_agent_card,
    build_a2a_subagent_execution_policy_for_state, decode_subagent_capsule_for_completion,
    run_a2a_native_turn, validate_subagent_capsule,
};

pub(crate) const A2A_PROTOCOL_VERSION: &str = "1.0";
pub(crate) const A2A_DEFAULT_TURN_TIMEOUT_MS: u64 = 180_000;
pub(crate) const A2A_TERMINAL_TASK_STORE_LIMIT: usize = 128;
pub(crate) const A2A_DEFAULT_LIST_PAGE_SIZE: usize = 50;
pub(crate) const A2A_MAX_LIST_PAGE_SIZE: usize = 100;
pub(crate) const A2A_DEFAULT_SUBSCRIBE_TIMEOUT_MS: u64 = 60_000;
pub(crate) const A2A_DEFAULT_SUBSCRIBE_HEARTBEAT_MS: u64 = 15_000;
const A2A_TASK_EVENT_REPLAY_LIMIT: usize = 256;
pub(crate) const A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY: &str = "pushNotificationConfigs";
pub(crate) const EVALOPS_A2A_EXTENSION_URI: &str =
    "https://evalops.com/a2a/extensions/operating-plane/v1";
pub(crate) const A2A_RUNTIME_GATEWAY_LEDGER_PEER: &str = "maestro-runtime-gateway";
pub(crate) const A2A_RUNTIME_GATEWAY_LEDGER_DISPLAY_NAME: &str = "Deixic Code Runtime Gateway";
pub(crate) const A2A_LEGACY_CONTROL_PLANE_LEDGER_PEER: &str = "maestro-control-plane";
static A2A_ID_FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) type A2ACancelSender = watch::Sender<bool>;
pub(crate) type A2ACancelReceiver = watch::Receiver<bool>;

#[derive(Clone, Debug)]
pub(crate) struct A2ATaskUpdateEvent {
    pub(crate) task_id: String,
    pub(crate) sequence: u64,
    pub(crate) task: Value,
}

#[derive(Debug, Default)]
pub(crate) struct A2ATaskEventHistory {
    pub(crate) next_sequence: u64,
    pub(crate) events: Vec<A2ATaskUpdateEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2APartBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) media_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2AMessageBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) parts: Vec<A2APartBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reference_task_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2ASendMessageRequest {
    pub(crate) message: A2AMessageBody,
    #[serde(default)]
    pub(crate) configuration: Option<Value>,
    #[serde(default)]
    pub(crate) metadata: Option<Value>,
}

#[derive(Debug)]
pub(crate) struct A2ASendTarget {
    pub(crate) task_id: String,
    pub(crate) context_id: String,
    pub(crate) history: Vec<Value>,
    pub(crate) previous_task: Option<Value>,
    pub(crate) metadata: Value,
    pub(crate) subagent_capsule: Option<ValidatedSubagentTaskCapsule>,
    pub(crate) subagent_execution_policy: Option<A2ASubagentExecutionPolicy>,
}

pub(crate) fn is_a2a_endpoint(head: &RequestHead) -> bool {
    if head.method == "OPTIONS" {
        return head.path == "/.well-known/agent-card.json"
            || head.path == "/message:send"
            || head.path == "/message:stream"
            || head.path == "/extendedAgentCard"
            || head.path == "/tasks"
            || head.path.starts_with("/tasks/");
    }
    matches!(
        (head.method.as_str(), head.path.as_str()),
        ("GET", "/.well-known/agent-card.json")
            | ("GET", "/extendedAgentCard")
            | ("POST", "/message:send")
            | ("GET", "/tasks")
    ) || (head.method == "GET" && a2a_task_id_from_get_path(&head.path).is_some())
        || (head.method == "POST" && a2a_task_id_from_cancel_path(&head.path).is_some())
        || ((head.method == "GET" || head.method == "POST" || head.method == "DELETE")
            && a2a_push_notification_config_path(&head.path).is_some())
}

pub(crate) fn is_a2a_streaming_endpoint(head: &RequestHead) -> bool {
    (head.method == "POST" && head.path == "/message:stream")
        || ((head.method == "GET" || head.method == "POST")
            && a2a_task_id_from_subscribe_path(&head.path).is_some())
}

fn a2a_task_id_from_get_path(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/tasks/")?;
    (!id.is_empty() && !id.contains('/') && !id.contains(':')).then_some(id)
}

fn validate_a2a_protocol_version(head: &RequestHead) -> Result<(), Vec<u8>> {
    let Some(version) = a2a_requested_protocol_version(head) else {
        return Ok(());
    };
    let version = version.trim();
    if version == A2A_PROTOCOL_VERSION {
        Ok(())
    } else {
        let message =
            format!("Unsupported A2A protocol version {version}; expected {A2A_PROTOCOL_VERSION}");
        Err(a2a_error_response(400, "UNSUPPORTED_VERSION", &message))
    }
}

fn a2a_requested_protocol_version(head: &RequestHead) -> Option<&str> {
    head.headers
        .get("a2a-version")
        .and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find(|part| !part.is_empty())
        })
        .or_else(|| head.query.get("a2a-version").map(String::as_str))
        .or_else(|| head.query.get("A2A-Version").map(String::as_str))
        .or_else(|| head.query.get("a2aVersion").map(String::as_str))
}

fn validate_a2a_requested_extensions(
    head: &RequestHead,
    message_extensions: Option<&[String]>,
) -> Result<Vec<String>, Vec<u8>> {
    let requested = requested_a2a_extensions(head, message_extensions);
    let unsupported = requested
        .iter()
        .find(|extension| !a2a_supported_extension(extension));
    if let Some(extension) = unsupported {
        return Err(a2a_error_response(
            400,
            "EXTENSION_NOT_SUPPORTED",
            &format!("A2A extension is not supported by this Deixic Code agent: {extension}"),
        ));
    }
    Ok(requested)
}

fn requested_a2a_extensions(
    head: &RequestHead,
    message_extensions: Option<&[String]>,
) -> Vec<String> {
    let mut requested = Vec::new();
    if let Some(header) = head.headers.get("a2a-extensions") {
        for extension in header.split(',') {
            push_unique_a2a_extension(&mut requested, extension);
        }
    }
    if let Some(query) = head.query.get("a2a-extensions") {
        for extension in query.split(',') {
            push_unique_a2a_extension(&mut requested, extension);
        }
    }
    if let Some(query) = head.query.get("A2A-Extensions") {
        for extension in query.split(',') {
            push_unique_a2a_extension(&mut requested, extension);
        }
    }
    if let Some(extensions) = message_extensions {
        for extension in extensions {
            push_unique_a2a_extension(&mut requested, extension);
        }
    }
    requested
}

fn push_unique_a2a_extension(requested: &mut Vec<String>, extension: &str) {
    let extension = extension.trim();
    if extension.is_empty() || requested.iter().any(|existing| existing == extension) {
        return;
    }
    requested.push(extension.to_string());
}

fn a2a_supported_extension(extension: &str) -> bool {
    extension == EVALOPS_A2A_EXTENSION_URI
}

pub(crate) async fn handle_a2a_endpoint(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: RequestHead,
    state: &AppState,
) -> Vec<u8> {
    if head.method == "OPTIONS" {
        return response(204, "text/plain; charset=utf-8", &[]);
    }

    if let Err(response) = validate_a2a_protocol_version(&head) {
        return response;
    }

    if let Err(response) = validate_csrf(&head, &state.config) {
        return response;
    }

    if head.method == "GET" && head.path == "/.well-known/agent-card.json" {
        return json_response(200, &a2a_agent_card(&head, &state.config));
    }

    let auth = match authorized_context(&head, &state.config) {
        Ok(auth) => auth,
        Err(response) => return response,
    };

    if head.method == "GET" && head.path == "/extendedAgentCard" {
        return json_response(200, &a2a_extended_agent_card(&head, &state.config));
    }

    if head.method == "GET" && head.path == "/tasks" {
        return match a2a_list_tasks_response(&head, state, &auth).await {
            Ok(value) => json_response(200, &value),
            Err(response) => response,
        };
    }

    if let Some((task_id, config_id)) = a2a_push_notification_config_path(&head.path) {
        return match (head.method.as_str(), config_id.as_deref()) {
            ("GET", None) => handle_a2a_push_notification_config_list(state, &task_id, &auth).await,
            ("GET", Some(config_id)) => {
                handle_a2a_push_notification_config_get(state, &task_id, config_id, &auth).await
            }
            ("POST", None) => {
                handle_a2a_push_notification_config_create(
                    stream, initial, &head, state, &task_id, &auth,
                )
                .await
            }
            ("DELETE", Some(config_id)) => {
                handle_a2a_push_notification_config_delete(state, &task_id, config_id, &auth).await
            }
            _ => a2a_error_response(404, "NOT_FOUND", "A2A endpoint not found"),
        };
    }

    if head.method == "GET" {
        if let Some(task_id) = a2a_task_id_from_get_path(&head.path) {
            let tasks = state.a2a_tasks.lock().await;
            return tasks.get(task_id).map_or_else(
                || a2a_error_response(404, "TASK_NOT_FOUND", "A2A task not found"),
                |task| {
                    if a2a_task_visible_to_auth(task, &auth) {
                        json_response(200, &a2a_task_for_query(task, true, None))
                    } else {
                        a2a_error_response(404, "TASK_NOT_FOUND", "A2A task not found")
                    }
                },
            );
        }
    }

    if head.method == "POST" {
        if let Some(task_id) = a2a_task_id_from_cancel_path(&head.path) {
            return match cancel_a2a_task(state, task_id, &auth).await {
                Ok(task) => json_response(200, &a2a_public_task(&task)),
                Err(response) => response,
            };
        }
    }

    if head.method == "POST" && head.path == "/message:send" {
        return handle_a2a_message_send(stream, initial, &head, state, &auth).await;
    }

    a2a_error_response(404, "NOT_FOUND", "A2A endpoint not found")
}

pub(crate) async fn handle_a2a_streaming_endpoint(
    mut stream: TcpStream,
    mut initial: Vec<u8>,
    head: RequestHead,
    state: AppState,
) -> Result<(), String> {
    if let Err(response) = validate_a2a_protocol_version(&head) {
        return write_response_and_close(&mut stream, response).await;
    }
    if let Err(response) = validate_csrf(&head, &state.config) {
        return write_response_and_close(&mut stream, response).await;
    }
    let auth = match authorized_context(&head, &state.config) {
        Ok(auth) => auth,
        Err(response) => return write_response_and_close(&mut stream, response).await,
    };

    if head.method == "POST" && head.path == "/message:stream" {
        return handle_a2a_message_stream(&mut stream, &mut initial, &head, &state, &auth).await;
    }
    if (head.method == "GET" || head.method == "POST")
        && a2a_task_id_from_subscribe_path(&head.path).is_some()
    {
        return handle_a2a_task_subscribe(&mut stream, &head, &state, &auth).await;
    }
    write_response_and_close(
        &mut stream,
        a2a_error_response(404, "NOT_FOUND", "A2A streaming endpoint not found"),
    )
    .await
}

async fn write_response_and_close(stream: &mut TcpStream, response: Vec<u8>) -> Result<(), String> {
    stream
        .write_all(&response)
        .await
        .map_err(|error| error.to_string())?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn handle_a2a_message_stream(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
) -> Result<(), String> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => {
            return write_response_and_close(
                stream,
                a2a_error_response(400, "INVALID_REQUEST", &error),
            )
            .await;
        }
    };
    let request: A2ASendMessageRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return write_response_and_close(
                stream,
                a2a_error_response(
                    400,
                    "INVALID_REQUEST",
                    &format!("invalid A2A message request: {error}"),
                ),
            )
            .await;
        }
    };
    let requested_extensions =
        match validate_a2a_requested_extensions(head, request.message.extensions.as_deref()) {
            Ok(extensions) => extensions,
            Err(response) => return write_response_and_close(stream, response).await,
        };
    let Some(prompt) = a2a_message_text(&request.message) else {
        return write_response_and_close(
            stream,
            a2a_error_response(
                400,
                "INVALID_REQUEST",
                "A2A message must contain at least one text part",
            ),
        )
        .await;
    };
    if let Err(error) = a2a_return_immediately(&request) {
        return write_response_and_close(stream, a2a_error_response(400, "INVALID_REQUEST", error))
            .await;
    }

    let metadata = a2a_task_metadata(head, &request, auth, &requested_extensions);
    let target = match claim_a2a_send_task(state, &request, head, auth, metadata).await {
        Ok(target) => target,
        Err(response) => return write_response_and_close(stream, response).await,
    };
    let task_id = target.task_id;
    let context_id = target.context_id;
    let history = target.history;
    let mut previous_task = target.previous_task;
    let metadata = target.metadata;
    let subagent_capsule = target.subagent_capsule;
    let subagent_execution_policy = target.subagent_execution_policy;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    if let Err(response) = register_a2a_cancel_sender(state, &task_id, cancel_tx).await {
        rollback_a2a_send_claim(state, &task_id, previous_task.take()).await;
        return write_response_and_close(stream, response).await;
    }

    if let Err(error) = stream.write_all(sse_headers().as_bytes()).await {
        state.a2a_cancel_senders.lock().await.remove(&task_id);
        rollback_a2a_send_claim(state, &task_id, previous_task.take()).await;
        return Err(error.to_string());
    }
    if let Some(task) = state.a2a_tasks.lock().await.get(&task_id).cloned() {
        if let Err(error) = send_a2a_stream_response(
            stream,
            &serde_json::json!({ "task": a2a_public_task(&task) }),
        )
        .await
        {
            state.a2a_cancel_senders.lock().await.remove(&task_id);
            rollback_a2a_send_claim(state, &task_id, previous_task.take()).await;
            return Err(error);
        }
    }
    let task = complete_a2a_task_with_capsule(
        state,
        A2ACompletionRequest {
            prompt,
            task_id,
            context_id,
            history,
            metadata,
            capsule: subagent_capsule,
            execution_policy: subagent_execution_policy,
        },
        cancel_rx,
    )
    .await;
    send_a2a_stream_response(
        stream,
        &serde_json::json!({ "statusUpdate": a2a_status_update_event(&task) }),
    )
    .await?;
    for artifact in task
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        send_a2a_stream_response(
            stream,
            &serde_json::json!({
                "artifactUpdate": a2a_artifact_update_event(&task, artifact)
            }),
        )
        .await?;
    }
    send_a2a_stream_response(
        stream,
        &serde_json::json!({ "task": a2a_public_task(&task) }),
    )
    .await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn handle_a2a_task_subscribe(
    stream: &mut TcpStream,
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
) -> Result<(), String> {
    let task_id = a2a_task_id_from_subscribe_path(&head.path)
        .expect("subscribe path should have been recognized");
    let mut receiver = state.a2a_task_events.subscribe();
    let current = {
        let tasks = state.a2a_tasks.lock().await;
        let Some(task) = tasks.get(task_id) else {
            return write_response_and_close(
                stream,
                a2a_error_response(404, "TASK_NOT_FOUND", "A2A task not found"),
            )
            .await;
        };
        if !a2a_task_visible_to_auth(task, auth) {
            return write_response_and_close(
                stream,
                a2a_error_response(404, "TASK_NOT_FOUND", "A2A task not found"),
            )
            .await;
        }
        task.clone()
    };

    if a2a_task_is_terminal(&current) {
        return write_response_and_close(
            stream,
            a2a_error_response(
                400,
                "UNSUPPORTED_OPERATION",
                "A2A terminal tasks cannot be subscribed to",
            ),
        )
        .await;
    }
    stream
        .write_all(sse_headers().as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    send_a2a_stream_response(
        stream,
        &serde_json::json!({ "task": a2a_public_task(&current) }),
    )
    .await?;
    send_a2a_stream_response(
        stream,
        &serde_json::json!({ "statusUpdate": a2a_status_update_event(&current) }),
    )
    .await?;
    let mut next_replay_sequence = a2a_task_event_next_sequence(state, task_id).await;

    let subscribe_timeout = Duration::from_millis(
        env_u64(
            "MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS",
            A2A_DEFAULT_SUBSCRIBE_TIMEOUT_MS,
        )
        .max(1),
    );
    let heartbeat_interval = Duration::from_millis(
        env_u64(
            "MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS",
            A2A_DEFAULT_SUBSCRIBE_HEARTBEAT_MS,
        )
        .max(1),
    );
    let deadline = Instant::now() + subscribe_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let wait_timeout = remaining.min(heartbeat_interval);
        let event = match tokio::time::timeout(wait_timeout, receiver.recv()).await {
            Ok(Ok(event)) => {
                if event.task_id == task_id {
                    next_replay_sequence = event.sequence.saturating_add(1);
                    Some(event.task)
                } else {
                    None
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                let replay =
                    a2a_task_events_since(state, task_id, next_replay_sequence, auth).await;
                if replay.is_empty() {
                    next_replay_sequence = a2a_task_event_next_sequence(state, task_id).await;
                    current_a2a_subscribe_task(state, task_id, auth).await
                } else {
                    for event in replay {
                        next_replay_sequence = event.sequence.saturating_add(1);
                        if send_a2a_subscribe_task_update(stream, &event.task, auth).await? {
                            let _ = stream.shutdown().await;
                            return Ok(());
                        }
                    }
                    continue;
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => {
                if Instant::now() >= deadline {
                    break;
                }
                stream
                    .write_all(b": keep-alive\n\n")
                    .await
                    .map_err(|error| error.to_string())?;
                continue;
            }
        };
        let Some(event) = event else {
            continue;
        };
        if event.get("id").and_then(Value::as_str) != Some(task_id) {
            continue;
        }
        if send_a2a_subscribe_task_update(stream, &event, auth).await? {
            break;
        }
    }
    let _ = stream.shutdown().await;
    Ok(())
}

async fn current_a2a_subscribe_task(
    state: &AppState,
    task_id: &str,
    auth: &AuthContext,
) -> Option<Value> {
    let tasks = state.a2a_tasks.lock().await;
    let task = tasks.get(task_id)?;
    a2a_task_visible_to_auth(task, auth).then(|| task.clone())
}

async fn a2a_task_event_next_sequence(state: &AppState, task_id: &str) -> u64 {
    state
        .a2a_task_event_history
        .lock()
        .await
        .get(task_id)
        .map(|history| history.next_sequence)
        .unwrap_or(0)
}

async fn a2a_task_events_since(
    state: &AppState,
    task_id: &str,
    sequence: u64,
    auth: &AuthContext,
) -> Vec<A2ATaskUpdateEvent> {
    state
        .a2a_task_event_history
        .lock()
        .await
        .get(task_id)
        .map(|history| {
            history
                .events
                .iter()
                .filter(|event| {
                    event.sequence >= sequence && a2a_task_visible_to_auth(&event.task, auth)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

async fn send_a2a_subscribe_task_update(
    stream: &mut TcpStream,
    task: &Value,
    auth: &AuthContext,
) -> Result<bool, String> {
    if !a2a_task_visible_to_auth(task, auth) {
        return Ok(false);
    }
    send_a2a_stream_response(
        stream,
        &serde_json::json!({ "statusUpdate": a2a_status_update_event(task) }),
    )
    .await?;
    if a2a_task_is_terminal(task) {
        for artifact in task
            .get("artifacts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            send_a2a_stream_response(
                stream,
                &serde_json::json!({
                    "artifactUpdate": a2a_artifact_update_event(task, artifact)
                }),
            )
            .await?;
        }
        send_a2a_stream_response(
            stream,
            &serde_json::json!({ "task": a2a_public_task(task) }),
        )
        .await?;
        return Ok(true);
    }
    Ok(false)
}

async fn send_a2a_stream_response(stream: &mut TcpStream, value: &Value) -> Result<(), String> {
    let Some(event_name) = a2a_stream_event_name(value) else {
        return send_sse(stream, value).await;
    };
    let event_id = a2a_stream_event_id(value)
        .map(|id| format!("id: {id}\n"))
        .unwrap_or_default();
    let body = serde_json::to_string(value).map_err(|error| error.to_string())?;
    stream
        .write_all(format!("{event_id}event: {event_name}\ndata: {body}\n\n").as_bytes())
        .await
        .map_err(|error| error.to_string())
}

fn a2a_stream_event_name(value: &Value) -> Option<&'static str> {
    if value.get("task").is_some() {
        Some("task")
    } else if value.get("statusUpdate").is_some() {
        Some("statusUpdate")
    } else if value.get("artifactUpdate").is_some() {
        Some("artifactUpdate")
    } else {
        None
    }
}

fn a2a_stream_event_id(value: &Value) -> Option<String> {
    if let Some(task) = value.get("task") {
        let context_id = a2a_stream_id_value(task.get("contextId"), "unknown-context");
        let task_id = a2a_stream_id_value(task.get("id"), "unknown-task");
        let state = a2a_stream_id_value(
            task.get("status").and_then(|status| status.get("state")),
            "snapshot",
        );
        let timestamp = a2a_stream_id_value(
            task.get("status")
                .and_then(|status| status.get("timestamp")),
            "latest",
        );
        return Some(format!(
            "a2a:{context_id}:{task_id}:task:{state}:{timestamp}"
        ));
    }

    if let Some(update) = value.get("statusUpdate") {
        let context_id = a2a_stream_id_value(update.get("contextId"), "unknown-context");
        let task_id = a2a_stream_id_value(update.get("taskId"), "unknown-task");
        let state = a2a_stream_id_value(
            update.get("status").and_then(|status| status.get("state")),
            "snapshot",
        );
        let timestamp = a2a_stream_id_value(
            update
                .get("status")
                .and_then(|status| status.get("timestamp")),
            "latest",
        );
        return Some(format!(
            "a2a:{context_id}:{task_id}:status:{state}:{timestamp}"
        ));
    }

    if let Some(update) = value.get("artifactUpdate") {
        let context_id = a2a_stream_id_value(update.get("contextId"), "unknown-context");
        let task_id = a2a_stream_id_value(update.get("taskId"), "unknown-task");
        let artifact_id = a2a_stream_id_value(
            update
                .get("artifact")
                .and_then(|artifact| artifact.get("artifactId").or_else(|| artifact.get("id"))),
            "latest",
        );
        return Some(format!("a2a:{context_id}:{task_id}:artifact:{artifact_id}"));
    }

    None
}

fn a2a_stream_id_value(value: Option<&Value>, fallback: &str) -> String {
    let segment = value
        .and_then(Value::as_str)
        .map(a2a_stream_id_segment)
        .filter(|segment| !segment.is_empty());
    segment.unwrap_or_else(|| fallback.to_string())
}

fn a2a_stream_id_segment(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn a2a_status_update_event(task: &Value) -> Value {
    let task = a2a_public_task(task);
    let mut event = serde_json::json!({
        "taskId": task.get("id").cloned().unwrap_or(Value::Null),
        "contextId": task.get("contextId").cloned().unwrap_or(Value::Null),
        "status": task.get("status").cloned().unwrap_or(Value::Null),
        "metadata": task.get("metadata").cloned().unwrap_or_else(|| serde_json::json!({}))
    });
    if let Some(delegation) = task.get("delegation") {
        event["delegation"] = delegation.clone();
    }
    event
}

pub(super) fn a2a_artifact_update_event(task: &Value, artifact: &Value) -> Value {
    let task = a2a_public_task(task);
    let mut event = serde_json::json!({
        "taskId": task.get("id").cloned().unwrap_or(Value::Null),
        "contextId": task.get("contextId").cloned().unwrap_or(Value::Null),
        "artifact": artifact,
        "append": false,
        "lastChunk": true,
        "metadata": task.get("metadata").cloned().unwrap_or_else(|| serde_json::json!({}))
    });
    if let Some(delegation) = task.get("delegation") {
        event["delegation"] = delegation.clone();
    }
    event
}

fn a2a_public_task(task: &Value) -> Value {
    let mut task = task.clone();
    a2a_redact_push_notification_metadata(&mut task);
    if let Some(delegation) =
        DelegationEvent::from_a2a_task(&task).and_then(|event| serde_json::to_value(event).ok())
    {
        if let Some(task_object) = task.as_object_mut() {
            task_object.insert("delegation".to_string(), delegation);
        }
    }
    task
}

pub(crate) async fn cancel_a2a_task(
    state: &AppState,
    task_id: &str,
    auth: &AuthContext,
) -> Result<Value, Vec<u8>> {
    let mut tasks = state.a2a_tasks.lock().await;
    let Some(task) = tasks.get_mut(task_id) else {
        return Err(a2a_error_response(
            404,
            "TASK_NOT_FOUND",
            "A2A task not found",
        ));
    };
    if !a2a_task_visible_to_auth(task, auth) {
        return Err(a2a_error_response(
            404,
            "TASK_NOT_FOUND",
            "A2A task not found",
        ));
    }
    if a2a_task_is_terminal(task) {
        return Err(a2a_error_response(
            400,
            "TASK_NOT_CANCELABLE",
            "A2A task cannot be canceled from its current state",
        ));
    }
    let context_id = task
        .get("contextId")
        .and_then(Value::as_str)
        .unwrap_or("a2a")
        .to_string();
    task["status"] = serde_json::json!({
        "state": "TASK_STATE_CANCELED",
        "message": a2a_agent_message(&context_id, "Task canceled"),
        "timestamp": now_rfc3339()
    });
    task["artifacts"] = Value::Array(Vec::new());
    if let Some(metadata) = task.get_mut("metadata") {
        let capsule = completion_subagent_capsule_from_metadata(metadata)
            .ok()
            .flatten();
        attach_server_owned_subagent_completion(
            metadata,
            capsule.as_ref(),
            task_id,
            &context_id,
            "canceled",
            &[],
        );
    }
    let task = task.clone();
    prune_a2a_terminal_tasks(&mut tasks);
    drop(tasks);

    if let Some(sender) = state.a2a_cancel_senders.lock().await.remove(task_id) {
        let _ = sender.send(true);
    }
    publish_a2a_task_update(state, &task).await;
    persist_a2a_tasks(state).await;

    Ok(task)
}

pub(crate) fn a2a_task_status_state(task: &Value) -> Option<&str> {
    task.get("status")
        .and_then(|status| status.get("state"))
        .and_then(Value::as_str)
}

pub(crate) fn normalize_a2a_task_state(state: &str) -> String {
    let mut normalized = String::new();
    let mut previous_was_separator = false;
    for ch in state.trim().chars() {
        if ch == '-' || ch.is_ascii_whitespace() {
            if !previous_was_separator && !normalized.is_empty() {
                normalized.push('_');
            }
            previous_was_separator = true;
        } else {
            normalized.push(ch.to_ascii_uppercase());
            previous_was_separator = false;
        }
    }
    normalized
}

pub(crate) fn canonical_a2a_task_state(state: &str) -> String {
    let normalized = normalize_a2a_task_state(state);
    match normalized.as_str() {
        "TASK_STATE_COMPLETED"
        | "STATE_COMPLETED"
        | "COMPLETED"
        | "TASK_STATE_SUCCEEDED"
        | "STATE_SUCCEEDED"
        | "SUCCEEDED"
        | "TASK_STATE_SUCCESS"
        | "STATE_SUCCESS"
        | "SUCCESS" => "TASK_STATE_COMPLETED".to_string(),
        "TASK_STATE_FAILED" | "STATE_FAILED" | "FAILED" => "TASK_STATE_FAILED".to_string(),
        "TASK_STATE_CANCELED"
        | "STATE_CANCELED"
        | "CANCELED"
        | "TASK_STATE_CANCELLED"
        | "STATE_CANCELLED"
        | "CANCELLED" => "TASK_STATE_CANCELED".to_string(),
        "TASK_STATE_REJECTED" | "STATE_REJECTED" | "REJECTED" => "TASK_STATE_REJECTED".to_string(),
        "TASK_STATE_INPUT_REQUIRED" | "STATE_INPUT_REQUIRED" | "INPUT_REQUIRED" => {
            "TASK_STATE_INPUT_REQUIRED".to_string()
        }
        _ if normalized.starts_with("TASK_STATE_") => normalized,
        _ => format!("TASK_STATE_{normalized}"),
    }
}

pub(crate) fn a2a_state_is_completed(state: &str) -> bool {
    matches!(
        normalize_a2a_task_state(state).as_str(),
        "TASK_STATE_COMPLETED"
            | "STATE_COMPLETED"
            | "COMPLETED"
            | "TASK_STATE_SUCCEEDED"
            | "STATE_SUCCEEDED"
            | "SUCCEEDED"
            | "TASK_STATE_SUCCESS"
            | "STATE_SUCCESS"
            | "SUCCESS"
    )
}

pub(crate) fn a2a_state_is_failed(state: &str) -> bool {
    matches!(
        normalize_a2a_task_state(state).as_str(),
        "TASK_STATE_FAILED"
            | "STATE_FAILED"
            | "FAILED"
            | "TASK_STATE_CANCELED"
            | "STATE_CANCELED"
            | "CANCELED"
            | "TASK_STATE_CANCELLED"
            | "STATE_CANCELLED"
            | "CANCELLED"
            | "TASK_STATE_REJECTED"
            | "STATE_REJECTED"
            | "REJECTED"
    )
}

fn a2a_state_is_input_required(state: &str) -> bool {
    matches!(
        normalize_a2a_task_state(state).as_str(),
        "TASK_STATE_INPUT_REQUIRED" | "STATE_INPUT_REQUIRED" | "INPUT_REQUIRED"
    )
}

pub(super) fn a2a_task_status_timestamp(task: &Value) -> Option<&str> {
    task.get("status")
        .and_then(|status| status.get("timestamp"))
        .and_then(Value::as_str)
}

pub(crate) fn a2a_task_is_terminal(task: &Value) -> bool {
    a2a_task_status_state(task)
        .is_some_and(|state| a2a_state_is_completed(state) || a2a_state_is_failed(state))
}

/// True when the task's status state canonicalizes to TASK_STATE_CANCELED.
/// Accepts aliases such as `CANCELLED` or `STATE_CANCELED` that normalize to
/// the canonical canceled state, so a canceled row is not overwritten by a
/// later store or completion update.
pub(crate) fn a2a_task_is_canceled(task: &Value) -> bool {
    a2a_task_status_state(task)
        .is_some_and(|state| canonical_a2a_task_state(state) == "TASK_STATE_CANCELED")
}

fn a2a_task_accepts_message(task: &Value) -> bool {
    a2a_task_status_state(task).is_some_and(a2a_state_is_input_required)
}

fn a2a_task_owner_subject(task: &Value) -> Option<&str> {
    task.get("metadata")
        .and_then(|metadata| metadata.get("ownerSubject"))
        .and_then(Value::as_str)
}

pub(crate) fn a2a_task_visible_to_auth(task: &Value, auth: &AuthContext) -> bool {
    if auth.unrestricted {
        return true;
    }
    let Some(subject) = auth.subject.as_deref() else {
        return false;
    };
    if a2a_task_owner_subject(task) != Some(subject) {
        return false;
    }
    let (organization_id, workspace_id) = a2a_task_tenant(task);
    match (organization_id.as_deref(), workspace_id.as_deref()) {
        (Some(task_org), Some(task_workspace)) => {
            auth.organization_id.as_deref() == Some(task_org)
                && auth.workspace_id.as_deref() == Some(task_workspace)
        }
        // Pre-tenant tasks are deliberately isolated from tenant-bound
        // principals until they are re-created with durable tenant metadata.
        (None, None) => auth.organization_id.is_none() && auth.workspace_id.is_none(),
        _ => false,
    }
}

pub(super) fn a2a_task_tenant(task: &Value) -> (Option<String>, Option<String>) {
    let Some(metadata) = task.get("metadata").and_then(Value::as_object) else {
        return (None, None);
    };
    let organization_id = ["organizationId", "organization_id"]
        .iter()
        .find_map(|key| metadata.get(*key).and_then(Value::as_str))
        .and_then(nonempty_str)
        .map(str::to_owned);
    let workspace_id = ["workspaceId", "workspace_id"]
        .iter()
        .find_map(|key| metadata.get(*key).and_then(Value::as_str))
        .and_then(nonempty_str)
        .map(str::to_owned);
    (organization_id, workspace_id)
}

pub(crate) async fn publish_a2a_task_update(state: &AppState, task: &Value) {
    let tasks = state.a2a_tasks.lock().await;
    publish_a2a_task_update_locked(state, task, &tasks).await;
}

/// Callers already serializing task updates must keep that same lock through
/// publication so concurrent callbacks cannot reorder their replay snapshots.
pub(super) async fn publish_a2a_task_update_locked(
    state: &AppState,
    task: &Value,
    tasks: &HashMap<String, Value>,
) {
    let Some(task_id) = task
        .get("id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.is_empty())
    else {
        return;
    };
    let event = {
        // Keep the task lock until history is updated: a delayed publication
        // must not recreate replay payloads after another writer evicts a task.
        let mut histories = state.a2a_task_event_history.lock().await;
        histories.retain(|id, _| tasks.contains_key(id));
        let mut transient_history = A2ATaskEventHistory::default();
        let history = if tasks.contains_key(task_id) {
            histories.entry(task_id.to_string()).or_default()
        } else {
            // Still notify existing subscribers, without retaining the payload.
            &mut transient_history
        };
        let event = A2ATaskUpdateEvent {
            task_id: task_id.to_string(),
            sequence: history.next_sequence,
            task: task.clone(),
        };
        history.next_sequence = history.next_sequence.saturating_add(1);
        history.events.push(event.clone());
        let overflow = history
            .events
            .len()
            .saturating_sub(A2A_TASK_EVENT_REPLAY_LIMIT);
        if overflow > 0 {
            history.events.drain(..overflow);
        }
        event
    };
    let _ = state.a2a_task_events.send(event);
    dispatch_a2a_push_notifications(task);
}

async fn a2a_list_tasks_response(
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
) -> Result<Value, Vec<u8>> {
    let page_size = match a2a_usize_query(head, &["pageSize", "page_size", "limit"]) {
        Ok(Some(value)) => value.clamp(1, A2A_MAX_LIST_PAGE_SIZE),
        Ok(None) => A2A_DEFAULT_LIST_PAGE_SIZE,
        Err(message) => return Err(a2a_error_response(400, "INVALID_REQUEST", &message)),
    };
    let page_start = match a2a_task_page_start(head) {
        Ok(value) => value,
        Err(message) => return Err(a2a_error_response(400, "INVALID_REQUEST", &message)),
    };
    let context_id = a2a_string_query(head, &["contextId", "context_id"]);
    let status =
        a2a_string_query(head, &["status", "state"]).map(|value| a2a_normalize_state(&value));
    let status_timestamp_after = a2a_string_query(
        head,
        &[
            "statusTimestampAfter",
            "status_timestamp_after",
            "lastUpdatedAfter",
            "last_updated_after",
        ],
    );
    let include_artifacts = match a2a_bool_query(head, &["includeArtifacts", "include_artifacts"]) {
        Ok(Some(value)) => value,
        Ok(None) => false,
        Err(message) => return Err(a2a_error_response(400, "INVALID_REQUEST", &message)),
    };
    let history_length = match a2a_usize_query(head, &["historyLength", "history_length"]) {
        Ok(value) => value,
        Err(message) => return Err(a2a_error_response(400, "INVALID_REQUEST", &message)),
    };

    let mut tasks = state
        .a2a_tasks
        .lock()
        .await
        .values()
        .filter(|task| a2a_task_visible_to_auth(task, auth))
        .filter(|task| {
            context_id.as_deref().is_none_or(|context_id| {
                task.get("contextId")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value == context_id)
            })
        })
        .filter(|task| {
            status
                .as_deref()
                .is_none_or(|status| a2a_task_status_state(task) == Some(status))
        })
        .filter(|task| {
            status_timestamp_after.as_deref().is_none_or(|after| {
                a2a_task_status_timestamp(task)
                    .is_some_and(|timestamp| a2a_timestamp_at_or_after(timestamp, after))
            })
        })
        .map(|task| a2a_task_for_query(task, include_artifacts, history_length))
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        compare_a2a_task_status_timestamps_desc(left, right)
            .then_with(|| a2a_task_id_for_sort(left).cmp(a2a_task_id_for_sort(right)))
    });
    let total_size = tasks.len();
    let page_start_index = a2a_task_page_start_index(&tasks, &page_start);
    let page = tasks
        .into_iter()
        .skip(page_start_index)
        .take(page_size)
        .collect::<Vec<_>>();
    let next_offset = page_start_index.saturating_add(page.len());
    let next_page_token = (next_offset < total_size)
        .then(|| page.last().and_then(a2a_task_page_token))
        .flatten();
    Ok(serde_json::json!({
        "tasks": page,
        "nextPageToken": next_page_token.unwrap_or_default(),
        "pageSize": page_size,
        "totalSize": total_size
    }))
}

fn a2a_timestamp_at_or_after(timestamp: &str, after: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(timestamp),
        chrono::DateTime::parse_from_rfc3339(after),
    ) {
        (Ok(timestamp), Ok(after)) => timestamp >= after,
        _ => timestamp >= after,
    }
}

fn compare_a2a_task_status_timestamps_desc(left: &Value, right: &Value) -> std::cmp::Ordering {
    compare_a2a_status_timestamps_desc(
        a2a_task_status_timestamp(left),
        a2a_task_status_timestamp(right),
    )
}

fn compare_a2a_status_timestamps_desc(
    left_timestamp: Option<&str>,
    right_timestamp: Option<&str>,
) -> std::cmp::Ordering {
    match (
        left_timestamp.and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok()),
        right_timestamp.and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok()),
    ) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right_timestamp
            .unwrap_or_default()
            .cmp(left_timestamp.unwrap_or_default()),
    }
}

#[derive(Debug)]
enum A2ATaskPageStart {
    Beginning,
    Offset(usize),
    Cursor(A2ATaskPageCursor),
}

#[derive(Debug)]
struct A2ATaskPageCursor {
    status_timestamp: String,
    id: String,
}

fn a2a_task_page_start(head: &RequestHead) -> Result<A2ATaskPageStart, String> {
    if let Some(token) = a2a_string_query(head, &["pageToken", "page_token"]) {
        if let Ok(offset) = token.parse::<usize>() {
            return Ok(A2ATaskPageStart::Offset(offset));
        }
        return parse_a2a_task_page_token(&token).map(A2ATaskPageStart::Cursor);
    }
    match a2a_usize_query(head, &["offset"])? {
        Some(offset) => Ok(A2ATaskPageStart::Offset(offset)),
        None => Ok(A2ATaskPageStart::Beginning),
    }
}

fn parse_a2a_task_page_token(token: &str) -> Result<A2ATaskPageCursor, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| "A2A query parameter pageToken must be a valid task page token".to_string())?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "A2A query parameter pageToken must be a valid task page token".to_string())?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            "A2A query parameter pageToken must be a valid task page token".to_string()
        })?;
    Ok(A2ATaskPageCursor {
        status_timestamp: value
            .get("statusTimestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        id: id.to_string(),
    })
}

fn a2a_task_page_start_index(tasks: &[Value], page_start: &A2ATaskPageStart) -> usize {
    match page_start {
        A2ATaskPageStart::Beginning => 0,
        A2ATaskPageStart::Offset(offset) => *offset,
        A2ATaskPageStart::Cursor(cursor) => tasks
            .iter()
            .position(|task| a2a_task_matches_page_cursor(task, cursor))
            .map(|index| index.saturating_add(1))
            .unwrap_or_else(|| {
                tasks
                    .iter()
                    .position(|task| a2a_task_sorts_after_page_cursor(task, cursor))
                    .unwrap_or(tasks.len())
            }),
    }
}

fn a2a_task_matches_page_cursor(task: &Value, cursor: &A2ATaskPageCursor) -> bool {
    a2a_task_id_for_sort(task) == cursor.id
        && compare_a2a_status_timestamps_desc(
            a2a_task_status_timestamp(task),
            Some(cursor.status_timestamp.as_str()),
        )
        .is_eq()
}

fn a2a_task_sorts_after_page_cursor(task: &Value, cursor: &A2ATaskPageCursor) -> bool {
    match compare_a2a_status_timestamps_desc(
        a2a_task_status_timestamp(task),
        Some(cursor.status_timestamp.as_str()),
    ) {
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => a2a_task_id_for_sort(task) > cursor.id.as_str(),
        std::cmp::Ordering::Greater => true,
    }
}

fn a2a_task_page_token(task: &Value) -> Option<String> {
    let id = a2a_task_id_for_sort(task);
    if id.is_empty() {
        return None;
    }
    let value = serde_json::json!({
        "statusTimestamp": a2a_task_status_timestamp(task).unwrap_or_default(),
        "id": id,
    });
    serde_json::to_vec(&value)
        .ok()
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
}

fn a2a_task_id_for_sort(task: &Value) -> &str {
    task.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn a2a_task_for_query(
    task: &Value,
    include_artifacts: bool,
    history_length: Option<usize>,
) -> Value {
    let mut task = a2a_public_task(task);
    if !include_artifacts {
        if let Some(task) = task.as_object_mut() {
            task.remove("artifacts");
        }
    }
    if let Some(history_length) = history_length {
        if let Some(history) = task.get_mut("history").and_then(Value::as_array_mut) {
            if history_length == 0 {
                history.clear();
            } else if history.len() > history_length {
                let start = history.len() - history_length;
                history.drain(..start);
            }
        }
    }
    task
}

fn a2a_string_query(head: &RequestHead, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        head.query
            .get(*name)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn a2a_usize_query(head: &RequestHead, names: &[&str]) -> Result<Option<usize>, String> {
    let Some(value) = a2a_string_query(head, names) else {
        return Ok(None);
    };
    value
        .parse::<usize>()
        .map(Some)
        .map_err(|_| format!("A2A query parameter {} must be an integer", names[0]))
}

fn a2a_bool_query(head: &RequestHead, names: &[&str]) -> Result<Option<bool>, String> {
    let Some(value) = a2a_string_query(head, names) else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => Err(format!(
            "A2A query parameter {} must be a boolean",
            names[0]
        )),
    }
}

fn a2a_normalize_state(value: &str) -> String {
    canonical_a2a_task_state(value)
}

fn validated_subagent_capsule_from_metadata(
    metadata: &Value,
) -> Result<Option<ValidatedSubagentTaskCapsule>, CapsuleValidationError> {
    let Some(request) = metadata.get(A2A_SUBAGENT_REQUEST_METADATA_PATH) else {
        return Ok(None);
    };
    let skill_id = request
        .get("skillId")
        .and_then(Value::as_str)
        .ok_or_else(|| CapsuleValidationError::Malformed {
            message: "skillId must be a string".to_string(),
        })?;
    validate_subagent_capsule(request, skill_id).map(Some)
}

pub(crate) fn completion_subagent_capsule_from_metadata(
    metadata: &Value,
) -> Result<Option<ValidatedSubagentTaskCapsule>, CapsuleValidationError> {
    let Some(request) = metadata.get(A2A_SUBAGENT_REQUEST_METADATA_PATH) else {
        return Ok(None);
    };
    let skill_id = request
        .get("skillId")
        .and_then(Value::as_str)
        .ok_or_else(|| CapsuleValidationError::Malformed {
            message: "skillId must be a string".to_string(),
        })?;
    decode_subagent_capsule_for_completion(request, skill_id).map(Some)
}

pub(crate) async fn claim_a2a_send_task(
    state: &AppState,
    request: &A2ASendMessageRequest,
    head: &RequestHead,
    auth: &AuthContext,
    metadata: Value,
) -> Result<A2ASendTarget, Vec<u8>> {
    let requested_task_id = request
        .message
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|task_id| !task_id.is_empty())
        .map(str::to_string);
    let task_id = requested_task_id
        .clone()
        .unwrap_or_else(|| generate_a2a_id("maestro-task"));
    let explicit_context_id = request
        .message
        .context_id
        .as_deref()
        .map(str::trim)
        .filter(|context_id| !context_id.is_empty())
        .map(str::to_string);
    let push_config = a2a_push_notification_config_from_send_request(request, &task_id).await?;

    let mut tasks = state.a2a_tasks.lock().await;
    let (task_id, context_id, mut history, previous_task, mut task_metadata) =
        if requested_task_id.is_some() {
            let Some(task) = tasks.get(&task_id) else {
                return Err(a2a_error_response(
                    404,
                    "TASK_NOT_FOUND",
                    "A2A task not found",
                ));
            };
            if !a2a_task_visible_to_auth(task, auth) {
                return Err(a2a_error_response(
                    404,
                    "TASK_NOT_FOUND",
                    "A2A task not found",
                ));
            }
            if a2a_task_is_terminal(task) {
                return Err(a2a_error_response(
                    400,
                    "UNSUPPORTED_OPERATION",
                    "A2A terminal tasks cannot accept more messages",
                ));
            }
            if !a2a_task_accepts_message(task) {
                return Err(a2a_error_response(
                    409,
                    "UNSUPPORTED_OPERATION",
                    "A2A task is not ready to accept another message",
                ));
            }

            let task_context_id = task
                .get("contextId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|context_id| !context_id.is_empty())
                .map(str::to_string);
            if let (Some(message_context_id), Some(task_context_id)) =
                (explicit_context_id.as_deref(), task_context_id.as_deref())
            {
                if message_context_id != task_context_id {
                    return Err(a2a_error_response(
                        400,
                        "INVALID_REQUEST",
                        "A2A message contextId must match the referenced task",
                    ));
                }
            }
            let context_id = explicit_context_id
                .or(task_context_id)
                .unwrap_or_else(|| a2a_context_id(request, head));
            let history = task
                .get("history")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            (
                task_id,
                context_id,
                history,
                Some(task.clone()),
                a2a_merge_task_metadata(task, metadata),
            )
        } else {
            (
                task_id,
                explicit_context_id.unwrap_or_else(|| a2a_context_id(request, head)),
                Vec::new(),
                None,
                metadata,
            )
        };
    if let Some(config) = push_config {
        task_metadata = a2a_metadata_with_push_notification_config(task_metadata, config)
            .map_err(|message| a2a_error_response(400, "INVALID_REQUEST", &message))?;
    }
    let subagent_capsule = validated_subagent_capsule_from_metadata(&task_metadata)
        .map_err(|error| a2a_error_response(400, "INVALID_REQUEST", &error.to_string()))?;
    let subagent_execution_policy = subagent_capsule
        .as_ref()
        .map(|capsule| build_a2a_subagent_execution_policy_for_state(state, capsule))
        .transpose()
        .map_err(|error| a2a_error_response(400, "INVALID_REQUEST", &error))?;
    if subagent_capsule.is_some() {
        if let Some(metadata) = task_metadata.as_object_mut() {
            metadata.remove("workGraph");
            metadata.remove("evalops.subagentCompletion");
        }
    }
    history.push(a2a_user_message_value(&request.message, &context_id));
    let working_message = a2a_agent_message(&context_id, "Deixic Code is working on the A2A task.");
    let task = a2a_task_value(
        &task_id,
        &context_id,
        "TASK_STATE_WORKING",
        working_message,
        history.clone(),
        Vec::new(),
        task_metadata.clone(),
    );
    tasks.insert(task_id.clone(), task.clone());
    prune_a2a_terminal_tasks(&mut tasks);
    drop(tasks);
    publish_a2a_task_update(state, &task).await;
    persist_a2a_tasks(state).await;
    Ok(A2ASendTarget {
        task_id,
        context_id,
        history,
        previous_task,
        metadata: task_metadata,
        subagent_capsule,
        subagent_execution_policy,
    })
}

fn a2a_merge_task_metadata(existing_task: &Value, metadata: Value) -> Value {
    let mut merged = existing_task
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Value::Object(metadata) = metadata {
        for (key, value) in metadata {
            merged.insert(key, value);
        }
    }
    Value::Object(merged)
}

async fn rollback_a2a_send_claim(state: &AppState, task_id: &str, previous_task: Option<Value>) {
    let mut tasks = state.a2a_tasks.lock().await;
    let Some(task) = tasks.get(task_id) else {
        return;
    };
    if a2a_task_status_state(task) != Some("TASK_STATE_WORKING") {
        return;
    }
    if let Some(previous_task) = previous_task {
        tasks.insert(task_id.to_string(), previous_task.clone());
        drop(tasks);
        publish_a2a_task_update(state, &previous_task).await;
    } else {
        tasks.remove(task_id);
        state.a2a_task_event_history.lock().await.remove(task_id);
        drop(tasks);
    }
    persist_a2a_tasks(state).await;
}

async fn a2a_canceled_task(state: &AppState, task_id: &str) -> Option<Value> {
    state
        .a2a_tasks
        .lock()
        .await
        .get(task_id)
        .and_then(|task| a2a_task_is_canceled(task).then(|| task.clone()))
}

pub(crate) async fn store_a2a_task_unless_canceled(
    state: &AppState,
    task_id: &str,
    task: Value,
) -> Value {
    let mut tasks = state.a2a_tasks.lock().await;
    if let Some(existing) = tasks.get(task_id) {
        if a2a_task_is_canceled(existing) {
            return existing.clone();
        }
    }
    tasks.insert(task_id.to_string(), task.clone());
    prune_a2a_terminal_tasks(&mut tasks);
    drop(tasks);
    publish_a2a_task_update(state, &task).await;
    persist_a2a_tasks(state).await;
    task
}

pub(crate) fn prune_a2a_terminal_tasks(tasks: &mut HashMap<String, Value>) {
    let mut terminal_tasks = tasks
        .iter()
        .filter(|(_, task)| a2a_task_is_terminal(task))
        .map(|(task_id, task)| {
            (
                task_id.clone(),
                a2a_task_status_timestamp(task)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    if terminal_tasks.len() <= A2A_TERMINAL_TASK_STORE_LIMIT {
        return;
    }
    terminal_tasks.sort_by(|(left_id, left_timestamp), (right_id, right_timestamp)| {
        left_timestamp
            .cmp(right_timestamp)
            .then_with(|| left_id.cmp(right_id))
    });
    let overflow = terminal_tasks.len() - A2A_TERMINAL_TASK_STORE_LIMIT;
    for (task_id, _) in terminal_tasks.into_iter().take(overflow) {
        tasks.remove(&task_id);
    }
}

pub(crate) async fn register_a2a_cancel_sender(
    state: &AppState,
    task_id: &str,
    cancel_tx: A2ACancelSender,
) -> Result<(), Vec<u8>> {
    let mut senders = state.a2a_cancel_senders.lock().await;
    if senders.contains_key(task_id) {
        return Err(a2a_error_response(
            409,
            "UNSUPPORTED_OPERATION",
            "A2A task is already running",
        ));
    }
    senders.insert(task_id.to_string(), cancel_tx);
    Ok(())
}

pub(crate) async fn handle_a2a_message_send(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    state: &AppState,
    auth: &AuthContext,
) -> Vec<u8> {
    let body = match read_request_body(stream, initial, head).await {
        Ok(body) => body,
        Err(error) => return a2a_error_response(400, "INVALID_REQUEST", &error),
    };
    let request: A2ASendMessageRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return a2a_error_response(
                400,
                "INVALID_REQUEST",
                &format!("invalid A2A message request: {error}"),
            );
        }
    };
    let requested_extensions =
        match validate_a2a_requested_extensions(head, request.message.extensions.as_deref()) {
            Ok(extensions) => extensions,
            Err(response) => return response,
        };

    let Some(prompt) = a2a_message_text(&request.message) else {
        return a2a_error_response(
            400,
            "INVALID_REQUEST",
            "A2A message must contain at least one text part",
        );
    };
    let return_immediately = match a2a_return_immediately(&request) {
        Ok(value) => value,
        Err(error) => return a2a_error_response(400, "INVALID_REQUEST", error),
    };

    let metadata = a2a_task_metadata(head, &request, auth, &requested_extensions);
    let target = match claim_a2a_send_task(state, &request, head, auth, metadata).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let task_id = target.task_id;
    let context_id = target.context_id;
    let history = target.history;
    let previous_task = target.previous_task;
    let metadata = target.metadata;
    let subagent_capsule = target.subagent_capsule;
    let subagent_execution_policy = target.subagent_execution_policy;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    if let Err(response) = register_a2a_cancel_sender(state, &task_id, cancel_tx).await {
        rollback_a2a_send_claim(state, &task_id, previous_task).await;
        return response;
    }
    if let Some(task) = a2a_canceled_task(state, &task_id).await {
        state.a2a_cancel_senders.lock().await.remove(&task_id);
        return json_response(200, &serde_json::json!({ "task": a2a_public_task(&task) }));
    }
    if return_immediately {
        let accepted_message = a2a_agent_message(&context_id, "Deixic Code accepted the A2A task.");
        let mut accepted_history = history.clone();
        accepted_history.push(accepted_message.clone());
        let task = a2a_task_value(
            &task_id,
            &context_id,
            "TASK_STATE_WORKING",
            accepted_message.clone(),
            accepted_history.clone(),
            Vec::new(),
            metadata.clone(),
        );
        let task = store_a2a_task_unless_canceled(state, &task_id, task).await;
        let state = state.clone();
        tokio::spawn(async move {
            let _ = complete_a2a_task_with_capsule(
                &state,
                A2ACompletionRequest {
                    prompt,
                    task_id,
                    context_id,
                    history: accepted_history,
                    metadata,
                    capsule: subagent_capsule,
                    execution_policy: subagent_execution_policy,
                },
                cancel_rx,
            )
            .await;
        });
        return json_response(200, &serde_json::json!({ "task": a2a_public_task(&task) }));
    }

    let task = complete_a2a_task_with_capsule(
        state,
        A2ACompletionRequest {
            prompt,
            task_id,
            context_id,
            history,
            metadata,
            capsule: subagent_capsule,
            execution_policy: subagent_execution_policy,
        },
        cancel_rx,
    )
    .await;
    json_response(200, &serde_json::json!({ "task": a2a_public_task(&task) }))
}

#[allow(dead_code)]
pub(crate) async fn complete_a2a_task(
    state: &AppState,
    prompt: String,
    task_id: String,
    context_id: String,
    history: Vec<Value>,
    metadata: Value,
    cancel_rx: A2ACancelReceiver,
) -> Value {
    let capsule = completion_subagent_capsule_from_metadata(&metadata)
        .ok()
        .flatten();
    let execution_policy = capsule
        .as_ref()
        .and_then(|capsule| build_a2a_subagent_execution_policy_for_state(state, capsule).ok());
    complete_a2a_task_with_capsule(
        state,
        A2ACompletionRequest {
            prompt,
            task_id,
            context_id,
            history,
            metadata,
            capsule,
            execution_policy,
        },
        cancel_rx,
    )
    .await
}

struct A2ACompletionRequest {
    prompt: String,
    task_id: String,
    context_id: String,
    history: Vec<Value>,
    metadata: Value,
    capsule: Option<ValidatedSubagentTaskCapsule>,
    execution_policy: Option<A2ASubagentExecutionPolicy>,
}

async fn complete_a2a_task_with_capsule(
    state: &AppState,
    request: A2ACompletionRequest,
    cancel_rx: A2ACancelReceiver,
) -> Value {
    let A2ACompletionRequest {
        prompt,
        task_id,
        context_id,
        mut history,
        mut metadata,
        capsule,
        execution_policy,
    } = request;
    let native_session_id = a2a_native_session_id(&context_id, &metadata);
    let turn = match run_a2a_native_turn(
        state,
        prompt,
        &native_session_id,
        cancel_rx,
        capsule.as_ref(),
        execution_policy.as_ref(),
    )
    .await
    {
        Ok(A2ATurnResult::Completed(turn)) => turn,
        Ok(A2ATurnResult::Canceled) => {
            let message = a2a_agent_message(&context_id, "Task canceled");
            history.push(message.clone());
            attach_server_owned_subagent_completion(
                &mut metadata,
                capsule.as_ref(),
                &task_id,
                &context_id,
                "canceled",
                &[],
            );
            let task = a2a_task_value(
                &task_id,
                &context_id,
                "TASK_STATE_CANCELED",
                message,
                history,
                Vec::new(),
                metadata,
            );
            let task = store_a2a_task_unless_canceled(state, &task_id, task).await;
            state.a2a_cancel_senders.lock().await.remove(&task_id);
            return task;
        }
        Err(error) => {
            warn_a2a_native_turn_failure(&task_id, &context_id, &metadata, &error);
            let message = a2a_agent_message(&context_id, &error);
            history.push(message.clone());
            attach_server_owned_subagent_completion(
                &mut metadata,
                capsule.as_ref(),
                &task_id,
                &context_id,
                "failed",
                &[],
            );
            let task = a2a_task_value(
                &task_id,
                &context_id,
                "TASK_STATE_FAILED",
                message.clone(),
                history,
                Vec::new(),
                metadata,
            );
            let task = store_a2a_task_unless_canceled(state, &task_id, task).await;
            state.a2a_cancel_senders.lock().await.remove(&task_id);
            return task;
        }
    };

    let assistant_text = if turn.assistant_text.trim().is_empty() {
        "Deixic Code completed the A2A task without a text response.".to_string()
    } else {
        turn.assistant_text
    };
    let agent_message = a2a_agent_message(&context_id, &assistant_text);
    if !turn.thinking_text.trim().is_empty() {
        metadata["thinking"] = Value::String(turn.thinking_text);
    }
    if !turn.tools.is_empty() {
        metadata["tools"] = Value::Array(turn.tools);
    }
    if let Some(usage) = turn.usage {
        metadata["usage"] = serde_json::json!({
            "input": usage.input_tokens,
            "output": usage.output_tokens,
            "cacheRead": usage.cache_read_tokens,
            "cacheWrite": usage.cache_write_tokens,
            "cost": usage.cost.unwrap_or(0.0)
        });
    }
    let mut artifacts = vec![serde_json::json!({
        "artifactId": format!("{task_id}-assistant-response"),
        "name": "assistant-response",
        "artifactKind": "assistant.response",
        "parts": [{ "text": assistant_text.clone(), "mediaType": "text/plain" }]
    })];
    if capsule
        .as_ref()
        .is_some_and(|capsule| capsule.lane_id == "code-review")
    {
        artifacts.push(serde_json::json!({
            "artifactId": format!("{task_id}-review-summary"),
            "name": "review-summary",
            "artifactKind": "review.summary",
            "parts": [{ "text": assistant_text, "mediaType": "text/plain" }],
            "metadata": { "serverGenerated": true }
        }));
    }
    artifacts.extend(a2a_acceptance_report_artifacts(
        &task_id,
        &turn.acceptance_reports,
    ));
    attach_server_owned_subagent_completion(
        &mut metadata,
        capsule.as_ref(),
        &task_id,
        &context_id,
        "completed",
        &artifacts,
    );
    let task = a2a_task_value(
        &task_id,
        &context_id,
        "TASK_STATE_COMPLETED",
        agent_message.clone(),
        {
            history.push(agent_message);
            history
        },
        artifacts,
        metadata,
    );
    let task = store_a2a_task_unless_canceled(state, &task_id, task).await;
    state.a2a_cancel_senders.lock().await.remove(&task_id);
    task
}

pub(crate) fn a2a_acceptance_report_artifacts(task_id: &str, reports: &[Value]) -> Vec<Value> {
    reports
        .iter()
        .enumerate()
        .map(|(index, report)| {
            serde_json::json!({
                "artifactId": format!("{task_id}-test-report-{index}"),
                "name": format!("acceptance-check-{index}"),
                "artifactKind": "test.report",
                "parts": [{
                    "data": report,
                    "mediaType": "application/json"
                }]
            })
        })
        .collect()
}

fn attach_server_owned_subagent_completion(
    metadata: &mut Value,
    capsule: Option<&ValidatedSubagentTaskCapsule>,
    task_id: &str,
    context_id: &str,
    observed_outcome: &str,
    artifacts: &[Value],
) {
    let Some(capsule) = capsule else {
        maybe_attach_a2a_subagent_work_graph(metadata, task_id, context_id);
        stamp_a2a_work_graph_outcome(metadata, observed_outcome);
        return;
    };
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.remove("workGraph");
        metadata.remove("evalops.subagentCompletion");
    }
    maybe_attach_a2a_subagent_work_graph(metadata, task_id, context_id);

    let mut observed_artifact_kinds = Vec::new();
    for artifact in artifacts {
        let artifact_kind = artifact
            .get("artifactKind")
            .or_else(|| artifact.get("kind"))
            .or_else(|| {
                artifact
                    .get("metadata")
                    .and_then(|metadata| metadata.get("artifactKind"))
            })
            .and_then(Value::as_str);
        if let Some(artifact_kind) = artifact_kind {
            if !observed_artifact_kinds
                .iter()
                .any(|observed| observed == artifact_kind)
            {
                observed_artifact_kinds.push(artifact_kind.to_string());
            }
        }
    }
    let missing_artifact_kinds = capsule
        .expected_artifact_kinds
        .iter()
        .filter(|expected| !observed_artifact_kinds.contains(expected))
        .cloned()
        .collect::<Vec<_>>();
    let verification_disposition = match observed_outcome {
        "completed" if capsule.material => "pending_independent_review",
        "completed" if missing_artifact_kinds.is_empty() => "completed",
        "completed" => "completed_with_missing_artifacts",
        "canceled" => "canceled",
        _ => "runtime_failed",
    };
    let work_graph_status = if observed_outcome == "completed" && capsule.material {
        "awaiting_review"
    } else {
        observed_outcome
    };

    let Some(metadata_object) = metadata.as_object_mut() else {
        return;
    };
    metadata_object.insert(
        "evalops.subagentCompletion".to_string(),
        serde_json::json!({
            "serverGenerated": true,
            "capsuleVersion": super::SUBAGENT_TASK_CAPSULE_VERSION,
            "taskId": capsule.task_id,
            "parentTaskId": capsule.parent_task_id,
            "skillId": capsule.skill_id,
            "laneId": capsule.lane_id,
            "taskClass": capsule.task_class,
            "observedOutcome": observed_outcome,
            "observedArtifactKinds": observed_artifact_kinds,
            "missingArtifactKinds": missing_artifact_kinds,
            "verificationDisposition": verification_disposition
        }),
    );
    if let Some(work_graph) = metadata_object
        .get_mut("workGraph")
        .and_then(Value::as_object_mut)
    {
        work_graph.insert(
            "state".to_string(),
            Value::String(work_graph_status.to_string()),
        );
        work_graph.insert(
            "status".to_string(),
            Value::String(work_graph_status.to_string()),
        );
        if work_graph_status == "awaiting_review" {
            work_graph.insert("waitingItemCount".to_string(), Value::from(1));
            work_graph.insert(
                "stateCounts".to_string(),
                serde_json::json!({ "awaiting_review": 1 }),
            );
            if let Some(edges) = work_graph
                .get_mut("codexSubagents")
                .and_then(|subagents| subagents.get_mut("edges"))
                .and_then(Value::as_array_mut)
            {
                for edge in edges {
                    edge["status"] = Value::String("awaiting_review".to_string());
                    edge["workItemState"] = Value::String("awaiting_review".to_string());
                    edge["completionGate"] = Value::String("independent-code-review".to_string());
                }
            }
        }
    }
}

fn a2a_native_session_id(context_id: &str, metadata: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"maestro-a2a-native-session-v1\0");
    for label in ["ownerSubject", "organizationId", "workspaceId"] {
        hasher.update(label.as_bytes());
        hasher.update([0]);
        if let Some(value) = metadata.get(label).and_then(Value::as_str) {
            hasher.update(value.as_bytes());
        }
        hasher.update([0]);
    }
    hasher.update(b"contextId\0");
    hasher.update(context_id.as_bytes());
    format!(
        "a2a-session-v1-{}",
        URL_SAFE_NO_PAD.encode(hasher.finalize())
    )
}

fn stamp_a2a_work_graph_outcome(metadata: &mut Value, observed_outcome: &str) {
    let Some(work_graph) = metadata
        .as_object_mut()
        .and_then(|metadata| metadata.get_mut("workGraph"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    work_graph.insert(
        "state".to_string(),
        Value::String(observed_outcome.to_string()),
    );
    work_graph.insert(
        "status".to_string(),
        Value::String(observed_outcome.to_string()),
    );
}

pub(crate) fn a2a_vfs_hydration_gap(context_file_count: i64, hydrated_file_count: i64) -> bool {
    context_file_count > 0 && hydrated_file_count == 0
}

fn a2a_metadata_i64(metadata: &Value, keys: &[&str]) -> i64 {
    let Some(object) = metadata.as_object() else {
        return 0;
    };
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_i64))
        .unwrap_or(0)
}

fn warn_a2a_native_turn_failure(task_id: &str, context_id: &str, metadata: &Value, error: &str) {
    let object = metadata.as_object();
    let agent_run_id = object
        .and_then(|object| {
            json_string_from_object(object, &["agentRunId", "agent_run_id", "platformRunId"])
        })
        .unwrap_or_default();
    let platform_run_id = object
        .and_then(|object| json_string_from_object(object, &["platformRunId", "platform_run_id"]))
        .unwrap_or_default();
    let vfs_context_file_count =
        a2a_metadata_i64(metadata, &["vfs_context_file_count", "vfsContextFileCount"]);
    let vfs_hydrated_file_count = a2a_metadata_i64(
        metadata,
        &["vfs_hydrated_file_count", "vfsHydratedFileCount"],
    );
    tracing::warn!(
        target: "maestro.a2a",
        event = "a2a_turn_failed",
        task_id = %task_id,
        context_id = %context_id,
        agent_run_id = %agent_run_id,
        platform_run_id = %platform_run_id,
        vfs_context_file_count,
        vfs_hydrated_file_count,
        vfs_hydration_gap = a2a_vfs_hydration_gap(vfs_context_file_count, vfs_hydrated_file_count),
        error = %error,
        "A2A native turn failed"
    );
}

fn a2a_subagent_request_object(metadata: &Map<String, Value>) -> Option<&Map<String, Value>> {
    [
        A2A_SUBAGENT_REQUEST_METADATA_PATH,
        "subagent_request",
        "subagentRequest",
    ]
    .into_iter()
    .find_map(|key| metadata.get(key).and_then(Value::as_object))
}

fn maybe_attach_a2a_subagent_work_graph(metadata: &mut Value, task_id: &str, context_id: &str) {
    let Some(metadata_object) = metadata.as_object_mut() else {
        return;
    };
    if metadata_object.get("workGraph").is_some() {
        return;
    }
    let Some(subagent_request) = a2a_subagent_request_object(metadata_object) else {
        return;
    };
    let capsule = subagent_request.get("capsule").and_then(Value::as_object);
    let skill_id = json_string_from_object(subagent_request, &["skillId", "skill_id"]);
    let role = json_string_from_object(subagent_request, &["role"]).or_else(|| {
        capsule.and_then(|capsule| json_string_from_object(capsule, &["laneId", "lane_id"]))
    });
    let swarm_id = json_string_from_object(subagent_request, &["swarmId", "swarm_id"]);
    let work_item_id = json_string_from_object(subagent_request, &["taskId", "task_id"])
        .or_else(|| {
            capsule.and_then(|capsule| json_string_from_object(capsule, &["taskId", "task_id"]))
        })
        .unwrap_or_else(|| task_id.to_string());
    let child_run_id = format!("a2a-task:{task_id}");
    let tool_call_id = format!("a2a-subagent-dispatch:{task_id}");
    let platform_run_id =
        json_string_from_object(metadata_object, &["platformRunId", "platform_run_id"]);
    let mut correlation_path = if let Some(swarm_id) = swarm_id.as_deref() {
        format!("maestro-swarm/{swarm_id}/{work_item_id}/a2a/{task_id}")
    } else {
        format!("a2a/{context_id}/{task_id}")
    };
    if let Some(platform_run_id) = platform_run_id.as_deref() {
        correlation_path.push_str(&format!(";platform_agent_run_id={platform_run_id}"));
    }

    metadata_object.insert(
        "workGraph".to_string(),
        serde_json::json!({
            "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
            "state": "completed",
            "itemCount": 1,
            "activeItemCount": 0,
            "blockedItemCount": 0,
            "waitingItemCount": 0,
            "childRunCount": 1,
            "childRunIds": [child_run_id],
            "toolCallCount": 1,
            "pendingToolCallCount": 0,
            "toolExecutionIds": [tool_call_id],
            "waitItemCount": 0,
            "waitIds": [],
            "stateCounts": { "completed": 1 },
            "correlationPath": correlation_path,
            "rawPayloadWithheld": true,
            "codexSubagents": {
                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                "edgeCount": 1,
                "toolCallIds": [tool_call_id],
                "childRunIds": [child_run_id],
                "threadIds": [],
                "edges": [{
                    "spawnToolCallId": tool_call_id,
                    "childRunId": child_run_id,
                    "operation": "a2a.subagent.dispatch",
                    "status": "completed",
                    "role": role,
                    "workItemState": "completed",
                    "completionGate": "terminal-task",
                    "workItemId": work_item_id,
                    "skillId": skill_id
                }]
            }
        }),
    );
}

pub(crate) fn a2a_message_text(message: &A2AMessageBody) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

pub(crate) fn a2a_context_id(request: &A2ASendMessageRequest, head: &RequestHead) -> String {
    let normalized = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    normalized(request.message.context_id.as_deref())
        .or_else(|| {
            normalized(
                request
                    .message
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("sessionId").and_then(Value::as_str)),
            )
        })
        .or_else(|| normalized(head.headers.get("x-evalops-session-id").map(String::as_str)))
        .or_else(|| normalized(head.headers.get("x-maestro-session-id").map(String::as_str)))
        .unwrap_or_else(|| generate_a2a_id("maestro-context"))
}

pub(crate) fn a2a_user_message_value(message: &A2AMessageBody, context_id: &str) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| serde_json::json!({}));
    if let Value::Object(object) = &mut value {
        object
            .entry("messageId")
            .or_insert_with(|| Value::String(generate_a2a_id("maestro-message")));
        object.insert(
            "contextId".to_string(),
            Value::String(context_id.to_string()),
        );
        object
            .entry("role")
            .or_insert_with(|| Value::String("ROLE_USER".to_string()));
    }
    value
}

pub(crate) fn a2a_agent_message(context_id: &str, text: &str) -> Value {
    serde_json::json!({
        "messageId": generate_a2a_id("maestro-message"),
        "contextId": context_id,
        "role": "ROLE_AGENT",
        "parts": [{ "text": text, "mediaType": "text/plain" }],
        "metadata": {
            "runtime": "maestro-rust-control-plane",
            "surface": "rust-tui"
        }
    })
}

pub(crate) fn a2a_task_value(
    task_id: &str,
    context_id: &str,
    state: &str,
    status_message: Value,
    history: Vec<Value>,
    artifacts: Vec<Value>,
    metadata: Value,
) -> Value {
    serde_json::json!({
        "id": task_id,
        "contextId": context_id,
        "status": {
            "state": state,
            "message": status_message,
            "timestamp": now_rfc3339()
        },
        "history": history,
        "artifacts": artifacts,
        "metadata": metadata
    })
}

pub(crate) fn a2a_task_metadata(
    head: &RequestHead,
    request: &A2ASendMessageRequest,
    auth: &AuthContext,
    requested_extensions: &[String],
) -> Value {
    let mut metadata = Map::new();
    metadata.insert(
        "runtime".to_string(),
        Value::String("maestro-rust-control-plane".to_string()),
    );
    metadata.insert("surface".to_string(), Value::String("rust-tui".to_string()));
    metadata.insert(
        "a2aProtocolVersion".to_string(),
        Value::String(A2A_PROTOCOL_VERSION.to_string()),
    );
    if let Some(subject) = auth.subject.as_deref() {
        metadata.insert(
            "ownerSubject".to_string(),
            Value::String(subject.to_string()),
        );
    }
    for (field, header) in [
        ("workspaceId", "x-evalops-workspace-id"),
        ("agentId", "x-evalops-agent-id"),
        ("sessionId", "x-evalops-session-id"),
        ("actorId", "x-evalops-actor-id"),
        ("traceparent", "traceparent"),
        ("tracestate", "tracestate"),
    ] {
        if let Some(value) = head.headers.get(header).map(String::as_str) {
            if !value.trim().is_empty() {
                metadata.insert(field.to_string(), Value::String(value.trim().to_string()));
            }
        }
    }
    if let Some(Value::Object(request_metadata)) = request.metadata.as_ref() {
        for (key, value) in request_metadata {
            if a2a_metadata_key_is_reserved(key) {
                continue;
            }
            metadata.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    if let Some(configuration) = request
        .configuration
        .as_ref()
        .and_then(a2a_configuration_metadata)
    {
        metadata
            .entry("configuration".to_string())
            .or_insert(configuration);
    }
    if let Some(Value::Object(message_metadata)) = request.message.metadata.as_ref() {
        for (key, value) in message_metadata {
            if a2a_metadata_key_is_reserved(key) {
                continue;
            }
            metadata.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    if !requested_extensions.is_empty() {
        metadata.insert(
            "a2aExtensions".to_string(),
            Value::Array(
                requested_extensions
                    .iter()
                    .map(|extension| Value::String(extension.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(organization_id) = auth.organization_id.as_deref() {
        metadata.insert(
            "organizationId".to_string(),
            Value::String(organization_id.to_string()),
        );
    }
    if let Some(workspace_id) = auth.workspace_id.as_deref() {
        metadata.insert(
            "workspaceId".to_string(),
            Value::String(workspace_id.to_string()),
        );
    }
    Value::Object(metadata)
}

fn a2a_configuration_metadata(configuration: &Value) -> Option<Value> {
    let mut object = configuration.as_object()?.clone();
    object.remove("taskPushNotificationConfig");
    object.remove("task_push_notification_config");
    object.remove("pushNotificationConfig");
    (!object.is_empty()).then_some(Value::Object(object))
}

pub(crate) fn a2a_return_immediately(
    request: &A2ASendMessageRequest,
) -> Result<bool, &'static str> {
    let Some(configuration) = request.configuration.as_ref() else {
        return Ok(false);
    };
    let Some(configuration) = configuration.as_object() else {
        return Err("A2A configuration must be an object");
    };
    let Some(return_immediately) = configuration.get("returnImmediately") else {
        return Ok(false);
    };
    return_immediately
        .as_bool()
        .ok_or("A2A configuration returnImmediately must be a boolean")
}

pub(crate) fn generate_a2a_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return format!("{prefix}-{}", URL_SAFE_NO_PAD.encode(bytes));
    }
    let counter = A2A_ID_FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{}-{counter}", now_millis(), process::id())
}

pub(crate) fn a2a_error_response(status: u16, code: &str, message: &str) -> Vec<u8> {
    json_response(
        status,
        &serde_json::json!({ "error": { "code": code, "message": message } }),
    )
}

#[cfg(test)]
mod native_session_tests {
    use super::*;

    #[test]
    fn native_session_identity_is_stable_and_tenant_scoped() {
        let first = serde_json::json!({
            "ownerSubject": "peer",
            "organizationId": "org-1",
            "workspaceId": "workspace-1"
        });
        let second_tenant = serde_json::json!({
            "ownerSubject": "peer",
            "organizationId": "org-2",
            "workspaceId": "workspace-1"
        });

        assert_eq!(
            a2a_native_session_id("chief", &first),
            a2a_native_session_id("chief", &first)
        );
        assert_ne!(
            a2a_native_session_id("chief", &first),
            a2a_native_session_id("chief", &second_tenant)
        );
        assert_ne!(
            a2a_native_session_id("chief", &first),
            a2a_native_session_id("reviewer", &first)
        );
    }
}
