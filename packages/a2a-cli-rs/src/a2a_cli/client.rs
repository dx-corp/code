//! Minimal A2A HTTP client for Agent Card discovery, message send, and task subscription.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct A2AServiceConfig {
    pub base_url: String,
    pub token: Option<String>,
    pub organization_id: Option<String>,
    pub workspace_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
    pub actor_id: Option<String>,
    pub timeout_ms: u64,
    pub max_attempts: u64,
}

#[derive(Debug, Clone)]
pub struct SendMessageInput {
    pub text: String,
    pub message_id: String,
    pub context_id: Option<String>,
    pub task_id: Option<String>,
    pub metadata: Option<Value>,
    pub return_immediately: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATask {
    pub id: String,
    #[serde(default)]
    pub context_id: Option<String>,
    pub status: A2ATaskStatus,
    #[serde(default)]
    pub artifacts: Option<Vec<A2AArtifact>>,
    #[serde(default)]
    pub history: Option<Vec<A2AMessage>>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2ATaskStatus {
    pub state: String,
    #[serde(default)]
    pub message: Option<A2AMessage>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2AMessage {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub context_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub parts: Vec<A2APart>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2APart {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct A2AArtifact {
    #[serde(default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub parts: Vec<A2APart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResult {
    pub task: A2ATask,
}

pub async fn discover_agent_card(config: &A2AServiceConfig) -> Result<Value> {
    let url = format!(
        "{}/.well-known/agent-card.json",
        config.base_url.trim_end_matches('/')
    );
    request_json(config, reqwest::Method::GET, &url, None).await
}

pub async fn send_message(
    config: &A2AServiceConfig,
    input: SendMessageInput,
) -> Result<SendMessageResult> {
    let url = format!("{}/message:send", config.base_url.trim_end_matches('/'));
    let mut message = json!({
        "messageId": input.message_id,
        "role": "ROLE_USER",
        "parts": [{
            "text": input.text,
            "mediaType": "text/plain"
        }],
    });
    if let Some(context_id) = &input.context_id {
        message["contextId"] = json!(context_id);
    }
    if let Some(task_id) = &input.task_id {
        message["taskId"] = json!(task_id);
    }
    let mut metadata = serde_json::Map::new();
    if let Some(Value::Object(extra)) = input.metadata {
        metadata.extend(extra);
    } else if let Some(extra) = input.metadata {
        metadata.insert("extra".into(), extra);
    }
    if let Some(workspace_id) = &config.workspace_id {
        metadata.insert("workspaceId".into(), json!(workspace_id));
    }
    if let Some(agent_id) = &config.agent_id {
        metadata.insert("agentId".into(), json!(agent_id));
    }
    if let Some(session_id) = &config.session_id {
        metadata.insert("sessionId".into(), json!(session_id));
    }
    if let Some(actor_id) = &config.actor_id {
        metadata.insert("actorId".into(), json!(actor_id));
    }
    if !metadata.is_empty() {
        message["metadata"] = Value::Object(metadata);
    }
    let body = json!({
        "message": message,
        "configuration": {
            "returnImmediately": input.return_immediately
        }
    });
    let response = request_json(config, reqwest::Method::POST, &url, Some(body)).await?;
    parse_send_result(response)
}

pub async fn get_task(config: &A2AServiceConfig, task_id: &str) -> Result<A2ATask> {
    let trimmed = task_id.trim();
    if trimmed.is_empty() {
        bail!("A2A task id is required");
    }
    let url = format!(
        "{}/tasks/{}",
        config.base_url.trim_end_matches('/'),
        urlencoding::encode(trimmed)
    );
    let response = request_json(config, reqwest::Method::GET, &url, None).await?;
    parse_task(response)
}

pub async fn wait_for_task(
    config: &A2AServiceConfig,
    task_id: &str,
    max_wait_ms: u64,
    interval_ms: u64,
) -> Result<A2ATask> {
    let deadline = std::time::Instant::now() + Duration::from_millis(max_wait_ms);
    let mut last = get_task(config, task_id).await?;
    let mut reconnect_delay = Duration::from_millis(100);
    if is_terminal_state(&last.status.state) {
        return Ok(last);
    }

    loop {
        match subscribe_until_disconnect(config, task_id, &mut last, deadline).await? {
            SubscribeOutcome::Terminal => return Ok(last),
            SubscribeOutcome::Unsupported => {
                return poll_until_terminal(config, task_id, last, deadline, interval_ms).await;
            }
            SubscribeOutcome::Disconnected => {
                last = get_task_before_deadline(config, task_id, deadline).await?;
                if is_terminal_state(&last.status.state) {
                    return Ok(last);
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(
                    reconnect_delay
                        .min(deadline.saturating_duration_since(std::time::Instant::now())),
                )
                .await;
                reconnect_delay = reconnect_delay
                    .saturating_mul(2)
                    .min(Duration::from_secs(2));
            }
        }
    }
    timeout_error(task_id, &last)
}

enum SubscribeOutcome {
    Terminal,
    Unsupported,
    Disconnected,
}

const A2A_SSE_EVENT_LIMIT_BYTES: usize = 8 * 1024 * 1024;
const A2A_ERROR_BODY_LIMIT_BYTES: usize = 64 * 1024;

async fn subscribe_until_disconnect(
    config: &A2AServiceConfig,
    task_id: &str,
    last: &mut A2ATask,
    deadline: std::time::Instant,
) -> Result<SubscribeOutcome> {
    let url = format!(
        "{}/tasks/{}:subscribe",
        config.base_url.trim_end_matches('/'),
        urlencoding::encode(task_id.trim())
    );
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(config.timeout_ms.max(1_000)))
        .build()
        .context("build A2A subscription client")?;
    let mut headers = build_headers(config)?;
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    let response = match tokio::time::timeout(
        deadline.saturating_duration_since(std::time::Instant::now()),
        client.get(&url).headers(headers).send(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Timed out waiting for A2A task {task_id}; last state {}",
            last.status.state
        )
    })? {
        Ok(response) => response,
        Err(_) => return Ok(SubscribeOutcome::Disconnected),
    };
    if matches!(response.status().as_u16(), 404 | 405 | 501) {
        return Ok(SubscribeOutcome::Unsupported);
    }
    if response.status().is_server_error() || matches!(response.status().as_u16(), 408 | 429) {
        return Ok(SubscribeOutcome::Disconnected);
    }
    if !response.status().is_success() {
        let status = response.status();
        let detail = parse_error_detail(&read_error_body(response, deadline).await?);
        bail!(
            "Platform A2A subscription failed with {status}{}{}",
            if detail.is_empty() { "" } else { ": " },
            detail
        );
    }

    let mut parser = SseParser::default();
    let mut stream = response.bytes_stream();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return timeout_error(task_id, last);
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                for event in parser.push(&chunk)? {
                    if apply_stream_response(last, &event)? {
                        return Ok(SubscribeOutcome::Terminal);
                    }
                }
            }
            Ok(Some(Err(_))) => return Ok(SubscribeOutcome::Disconnected),
            Ok(None) if is_terminal_state(&last.status.state) => {
                // Some older peers end the stream cleanly after their terminal
                // status and never send a full-task envelope. We consumed every
                // event before EOF, including any artifact updates; preserve
                // that compatibility without treating a transport error as
                // authoritative completion.
                return Ok(SubscribeOutcome::Terminal);
            }
            Ok(None) => return Ok(SubscribeOutcome::Disconnected),
            Err(_) => return timeout_error(task_id, last),
        }
    }
}

async fn read_error_body(
    response: reqwest::Response,
    deadline: std::time::Instant,
) -> Result<String> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!("Timed out reading A2A subscription error response");
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let available = A2A_ERROR_BODY_LIMIT_BYTES.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(available)]);
                if body.len() == A2A_ERROR_BODY_LIMIT_BYTES {
                    break;
                }
            }
            Ok(Some(Err(error))) => {
                return Err(error).context("read A2A subscription error response");
            }
            Ok(None) => break,
            Err(_) => bail!("Timed out reading A2A subscription error response"),
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn poll_until_terminal(
    config: &A2AServiceConfig,
    task_id: &str,
    mut last: A2ATask,
    deadline: std::time::Instant,
    interval_ms: u64,
) -> Result<A2ATask> {
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(
            Duration::from_millis(interval_ms.max(100))
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        )
        .await;
        last = get_task_before_deadline(config, task_id, deadline).await?;
        if is_terminal_state(&last.status.state) {
            return Ok(last);
        }
    }
    timeout_error(task_id, &last)
}

async fn get_task_before_deadline(
    config: &A2AServiceConfig,
    task_id: &str,
    deadline: std::time::Instant,
) -> Result<A2ATask> {
    tokio::time::timeout(
        deadline.saturating_duration_since(std::time::Instant::now()),
        get_task(config, task_id),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out reconciling A2A task {task_id}"))?
}

fn timeout_error<T>(task_id: &str, last: &A2ATask) -> Result<T> {
    bail!(
        "Timed out waiting for A2A task {}; last state {}",
        task_id,
        last.status.state
    )
}

#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
    _id: Option<String>,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>> {
        if self.buffer.len().saturating_add(chunk.len()) > A2A_SSE_EVENT_LIMIT_BYTES {
            bail!("A2A SSE event exceeded {A2A_SSE_EVENT_LIMIT_BYTES} bytes");
        }
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line).context("A2A SSE line was not UTF-8")?;
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data).join("\n"));
                    self.data_bytes = 0;
                }
                self._id = None;
            } else if !line.starts_with(':') {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "data" => {
                        self.data_bytes = self
                            .data_bytes
                            .saturating_add(value.len())
                            .saturating_add(1);
                        if self.data_bytes > A2A_SSE_EVENT_LIMIT_BYTES {
                            bail!("A2A SSE event exceeded {A2A_SSE_EVENT_LIMIT_BYTES} bytes");
                        }
                        self.data.push(value.to_string());
                    }
                    "id" if !value.contains('\0') => self._id = Some(value.to_string()),
                    _ => {}
                }
            }
        }
        Ok(events)
    }
}

fn apply_stream_response(last: &mut A2ATask, data: &str) -> Result<bool> {
    let value: Value = serde_json::from_str(data).context("decode A2A SSE data")?;
    let value = value.get("result").unwrap_or(&value);
    if let Some(task) = value.get("task") {
        *last = parse_task(task.clone())?;
        return Ok(is_terminal_state(&last.status.state));
    } else if let Some(update) = value.get("statusUpdate") {
        if let Some(status) = update.get("status") {
            last.status =
                serde_json::from_value(status.clone()).context("parse A2A status update")?;
        }
    } else if let Some(update) = value.get("artifactUpdate") {
        if let Some(artifact) = update.get("artifact") {
            let artifact: A2AArtifact =
                serde_json::from_value(artifact.clone()).context("parse A2A artifact update")?;
            last.artifacts.get_or_insert_with(Vec::new).push(artifact);
        }
    }
    // A terminal status update can precede artifactUpdate and the final task.
    // Only the authoritative full-task event completes the subscription.
    Ok(false)
}

pub fn extract_task_text(task: &A2ATask) -> Option<String> {
    if let Some(message) = &task.status.message {
        if let Some(text) = first_message_text(message) {
            return Some(text);
        }
    }
    if let Some(artifacts) = &task.artifacts {
        for artifact in artifacts {
            for part in &artifact.parts {
                if let Some(text) = part
                    .text
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    return Some(text.to_string());
                }
            }
        }
    }
    if let Some(history) = &task.history {
        for message in history.iter().rev() {
            let role = message.role.as_deref().unwrap_or("").to_ascii_lowercase();
            if role.contains("agent") {
                if let Some(text) = first_message_text(message) {
                    return Some(text);
                }
            }
        }
    }
    None
}

pub fn is_terminal_state(state: &str) -> bool {
    is_final_state(state) || is_action_required_state(state)
}

pub fn is_final_state(state: &str) -> bool {
    is_completed_state(state) || is_failed_state(state)
}

pub fn is_completed_state(state: &str) -> bool {
    matches_state(state, &["COMPLETED", "SUCCEEDED", "SUCCESS"])
}

pub fn is_failed_state(state: &str) -> bool {
    matches_state(state, &["FAILED", "CANCELED", "CANCELLED", "REJECTED"])
}

pub fn is_action_required_state(state: &str) -> bool {
    matches_state(state, &["INPUT_REQUIRED", "AUTH_REQUIRED"])
}

fn matches_state(state: &str, candidates: &[&str]) -> bool {
    let normalized = normalize_state(state);
    // Accept the wire enum prefix, but never infer a state from arbitrary suffixes.
    let canonical = normalized
        .strip_prefix("TASK_STATE_")
        .unwrap_or(&normalized);
    candidates.contains(&canonical)
}

fn normalize_state(state: &str) -> String {
    state.trim().to_ascii_uppercase().replace([' ', '-'], "_")
}

fn first_message_text(message: &A2AMessage) -> Option<String> {
    message
        .parts
        .iter()
        .filter_map(|part| part.text.as_ref())
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_send_result(response: Value) -> Result<SendMessageResult> {
    if let Ok(result) = serde_json::from_value::<SendMessageResult>(response.clone()) {
        return Ok(result);
    }
    if response.get("id").is_some() && response.get("status").is_some() {
        return Ok(SendMessageResult {
            task: parse_task(response)?,
        });
    }
    if let Some(task) = response.get("result").or_else(|| response.get("task")) {
        return Ok(SendMessageResult {
            task: parse_task(task.clone())?,
        });
    }
    bail!("A2A send response did not include a task");
}

fn parse_task(value: Value) -> Result<A2ATask> {
    serde_json::from_value(value).context("parse A2A task")
}

async fn request_json(
    config: &A2AServiceConfig,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(config.timeout_ms.max(1_000)))
        .build()
        .context("build HTTP client")?;
    let headers = build_headers(config)?;
    let max_attempts = config.max_attempts.max(1);
    let mut last_error = None;
    for attempt in 1..=max_attempts {
        let mut request = client.request(method.clone(), url).headers(headers.clone());
        if let Some(body) = &body {
            request = request.json(body);
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                if status.is_success() {
                    if text.trim().is_empty() {
                        return Ok(json!({}));
                    }
                    return serde_json::from_str(&text)
                        .with_context(|| format!("decode A2A JSON response from {url}"));
                }
                let detail = parse_error_detail(&text);
                let error = anyhow::anyhow!(
                    "Platform A2A request failed with {status}{}{}",
                    if detail.is_empty() { "" } else { ": " },
                    detail
                );
                if status.is_server_error() && attempt < max_attempts {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
                    continue;
                }
                return Err(error);
            }
            Err(error) => {
                last_error = Some(error.into());
                if attempt < max_attempts {
                    tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
                    continue;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("A2A request failed")))
}

fn parse_error_detail(text: &str) -> String {
    if let Ok(payload) = serde_json::from_str::<Value>(text) {
        if let Some(message) = payload
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .or_else(|| payload.pointer("/error/code").and_then(|v| v.as_str()))
            .or_else(|| payload.get("message").and_then(|v| v.as_str()))
        {
            return message.to_string();
        }
    }
    text.chars().take(240).collect()
}

fn build_headers(config: &A2AServiceConfig) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(token) = &config.token {
        let value = format!("Bearer {token}");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&value).context("invalid Authorization header")?,
        );
    }
    insert_optional_header(
        &mut headers,
        "X-Organization-ID",
        config.organization_id.as_deref(),
    )?;
    insert_optional_header(
        &mut headers,
        "X-EvalOps-Workspace-Id",
        config.workspace_id.as_deref(),
    )?;
    insert_optional_header(
        &mut headers,
        "X-EvalOps-Agent-Id",
        config.agent_id.as_deref(),
    )?;
    insert_optional_header(
        &mut headers,
        "X-EvalOps-Session-Id",
        config.session_id.as_deref(),
    )?;
    insert_optional_header(
        &mut headers,
        "X-EvalOps-Actor-Id",
        config.actor_id.as_deref(),
    )?;
    Ok(headers)
}

fn insert_optional_header(headers: &mut HeaderMap, name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).context("header name")?,
            HeaderValue::from_str(value).with_context(|| format!("invalid {name} header"))?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
