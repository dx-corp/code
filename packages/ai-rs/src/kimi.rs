//! Moonshot Kimi K3 adapter.
//!
//! Kimi K3 uses the OpenAI-compatible Chat Completions protocol, but its
//! always-on reasoning trace travels in `reasoning_content`. That trace must be
//! replayed on the assistant message that contains any tool calls so a
//! subsequent tool-result turn remains valid.

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::client::{AiClient, AiProvider, provider_model_name};
use super::types::{
    ContentBlock, ImageSource, Message, MessageContent, ProviderStreamErrorKind, RequestConfig,
    Role, StopReason, StreamEvent, Tool,
};

const KIMI_K3_MISSING_TERMINAL_EVENT: &str =
    "kimi_k3_protocol_error: kind=transient reason=missing_done_event";

#[derive(Clone)]
pub(crate) struct KimiK3Client {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl KimiK3Client {
    pub(crate) fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_mins(5))
                .build()
                .context("failed to create Kimi K3 HTTP client")?,
            api_key: api_key.into().trim().to_string(),
            base_url: base_url.into().trim().trim_end_matches('/').to_string(),
        })
    }

    pub(crate) fn build_request_body(&self, messages: &[Message], config: &RequestConfig) -> Value {
        let mut converted = convert_messages(messages);
        if let Some(system) = config
            .system
            .as_deref()
            .map(str::trim)
            .filter(|system| !system.is_empty())
        {
            converted.insert(0, json!({"role": "system", "content": system}));
        }

        let mut body = json!({
            "model": provider_model_name(&config.model),
            "messages": converted,
            "max_tokens": config.max_tokens,
            "stream": true,
            "stream_options": {"include_usage": true},
        });

        if let Some(temperature) = config.temperature {
            body["temperature"] = json!(temperature);
        }

        if let Some(thinking) = &config.thinking {
            body["reasoning_effort"] = json!(reasoning_effort(thinking.budget_tokens));
        }

        if !config.tools.is_empty() {
            body["tools"] = Value::Array(config.tools.iter().map(tool_definition).collect());
            body["tool_choice"] = json!("auto");
        }

        body
    }

    async fn stream_inner(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let request_model = provider_model_name(&config.model);
        let endpoint = format!("{}/chat/completions", self.base_url);
        let mut request = self
            .client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/json")
            .json(&self.build_request_body(messages, config));
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request
            .send()
            .await
            .context("failed to send request to Kimi K3")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Kimi K3 API returned {status}: {}",
                compact_error_body(&body)
            );
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let mut events = response
            .bytes_stream()
            .map(|result| result.map_err(std::io::Error::other))
            .eventsource();

        tokio::spawn(async move {
            let mut message_started = false;
            let mut text_started = false;
            let mut text_closed = false;
            let mut tool_calls = Vec::<ToolCallAccumulator>::new();
            let mut tools_flushed = false;
            let mut stop_reason = None;

            while let Some(event_result) = events.next().await {
                let event = match event_result {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = tx.send(StreamEvent::ProviderError {
                            kind: ProviderStreamErrorKind::TransientProtocol,
                            message: format!("Kimi K3 SSE stream error: {error}"),
                        });
                        return;
                    }
                };

                if event.data.trim().is_empty() {
                    continue;
                }

                if event.data.trim() == "[DONE]" {
                    if text_started && !text_closed {
                        let _ = tx.send(StreamEvent::ContentBlockStop {
                            index: 0,
                            thinking_signature: None,
                        });
                    }
                    if !tools_flushed {
                        flush_tool_calls(&tx, &tool_calls);
                    }
                    if !message_started {
                        let _ = tx.send(StreamEvent::MessageStart {
                            id: "kimi-k3".to_string(),
                            model: request_model.clone(),
                        });
                    }
                    let _ = tx.send(StreamEvent::MessageStop { stop_reason });
                    return;
                }

                let chunk = match serde_json::from_str::<KimiChunk>(&event.data) {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let _ = tx.send(StreamEvent::ProviderError {
                            kind: ProviderStreamErrorKind::TransientProtocol,
                            message: format!("invalid Kimi K3 SSE payload: {error}"),
                        });
                        return;
                    }
                };

                if !message_started {
                    message_started = true;
                    let id = if chunk.id.is_empty() {
                        "kimi-k3".to_string()
                    } else {
                        chunk.id.clone()
                    };
                    let model = chunk.model.clone().unwrap_or_else(|| request_model.clone());
                    let _ = tx.send(StreamEvent::MessageStart { id, model });
                }

                for choice in chunk.choices {
                    if let Some(delta) = choice.delta {
                        if let Some(reasoning) = delta.reasoning_content {
                            if !reasoning.is_empty() {
                                let _ = tx.send(StreamEvent::ThinkingDelta {
                                    index: 0,
                                    thinking: reasoning,
                                });
                            }
                        }

                        if let Some(content) = delta.content {
                            if !content.is_empty() {
                                if !text_started {
                                    text_started = true;
                                    let _ = tx.send(StreamEvent::ContentBlockStart {
                                        index: 0,
                                        block: ContentBlock::Text {
                                            text: String::new(),
                                        },
                                    });
                                }
                                let _ = tx.send(StreamEvent::TextDelta {
                                    index: 0,
                                    text: content,
                                });
                            }
                        }

                        for tool_call in delta.tool_calls.unwrap_or_default() {
                            let index = tool_call.index;
                            while tool_calls.len() <= index {
                                tool_calls.push(ToolCallAccumulator::default());
                            }
                            if let Some(id) = tool_call.id {
                                tool_calls[index].id = id;
                            }
                            if let Some(function) = tool_call.function {
                                if let Some(name) = function.name {
                                    tool_calls[index].name = name;
                                }
                                if let Some(arguments) = function.arguments {
                                    tool_calls[index].arguments.push_str(&arguments);
                                }
                            }
                        }
                    }

                    if let Some(reason) = choice.finish_reason.as_deref() {
                        stop_reason = map_stop_reason(reason);
                        if text_started && !text_closed {
                            text_closed = true;
                            let _ = tx.send(StreamEvent::ContentBlockStop {
                                index: 0,
                                thinking_signature: None,
                            });
                        }
                        if reason == "tool_calls" && !tools_flushed {
                            tools_flushed = true;
                            flush_tool_calls(&tx, &tool_calls);
                        }
                    }
                }

                if let Some(usage) = chunk.usage {
                    let _ = tx.send(StreamEvent::Usage {
                        input_tokens: usage.prompt_tokens.unwrap_or(0),
                        output_tokens: usage.completion_tokens.unwrap_or(0),
                        cache_read_tokens: usage
                            .prompt_tokens_details
                            .and_then(|details| details.cached_tokens),
                        cache_creation_tokens: None,
                    });
                }
            }

            let _ = tx.send(StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message: KIMI_K3_MISSING_TERMINAL_EVENT.to_string(),
            });
        });

        Ok(rx)
    }
}

impl AiClient for KimiK3Client {
    async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        self.stream_inner(messages, config).await
    }

    fn provider(&self) -> AiProvider {
        AiProvider::Moonshot
    }
}

fn reasoning_effort(budget_tokens: u32) -> &'static str {
    match budget_tokens {
        0..=3_000 => "low",
        3_001..=10_000 => "high",
        _ => "max",
    }
}

fn tool_definition(tool: &Tool) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name.clone(),
            "description": tool.description.clone(),
            "parameters": tool.input_schema.clone(),
        }
    })
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut converted = Vec::new();

    for message in &super::transform::repair_tool_sequence(messages.to_vec()) {
        if let MessageContent::Blocks(blocks) = &message.content {
            let tool_results = blocks.iter().filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some((tool_use_id, content)),
                _ => None,
            });
            for (tool_use_id, content) in tool_results {
                converted.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
        }

        match &message.content {
            MessageContent::Text(text) => converted.push(json!({
                "role": role_name(message.role),
                "content": text,
            })),
            MessageContent::Blocks(blocks) if message.role == Role::Assistant => {
                if let Some(value) = assistant_message(blocks) {
                    converted.push(value);
                }
            }
            MessageContent::Blocks(blocks) if message.role == Role::System => {
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if !text.is_empty() {
                    converted.push(json!({"role": "system", "content": text}));
                }
            }
            MessageContent::Blocks(blocks) => {
                let parts = user_content_parts(blocks);
                if parts.is_empty() {
                    continue;
                }
                let content = if parts.len() == 1
                    && parts[0].get("type").and_then(Value::as_str) == Some("text")
                {
                    parts[0]["text"].clone()
                } else {
                    Value::Array(parts)
                };
                converted.push(json!({"role": "user", "content": content}));
            }
        }
    }

    converted
}

fn assistant_message(blocks: &[ContentBlock]) -> Option<Value> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let reasoning = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect::<String>();
    let tool_calls = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some(json!({
                "id": id.clone(),
                "type": "function",
                "function": {
                    "name": name.clone(),
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
                }
            })),
            _ => None,
        })
        .collect::<Vec<_>>();

    if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
        return None;
    }

    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert(
        "content".to_string(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Some(Value::Object(message))
}

fn user_content_parts(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            ContentBlock::Image { source } => {
                let url = match source {
                    ImageSource::Url { url } => url.clone(),
                    ImageSource::Base64 { media_type, data } => {
                        format!("data:{media_type};base64,{data}")
                    }
                };
                Some(json!({"type": "image_url", "image_url": {"url": url}}))
            }
            ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. } => None,
        })
        .collect()
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

fn map_stop_reason(reason: &str) -> Option<StopReason> {
    match reason {
        "stop" => Some(StopReason::EndTurn),
        "length" => Some(StopReason::MaxTokens),
        "tool_calls" => Some(StopReason::ToolUse),
        _ => None,
    }
}

fn compact_error_body(body: &str) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let prefix = chars.by_ref().take(1_000).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else if prefix.is_empty() {
        "empty response body".to_string()
    } else {
        prefix
    }
}

fn flush_tool_calls(tx: &mpsc::UnboundedSender<StreamEvent>, calls: &[ToolCallAccumulator]) {
    for (index, call) in calls.iter().enumerate() {
        if call.name.is_empty() {
            continue;
        }
        let block_index = index + 1;
        let id = if call.id.is_empty() {
            format!("kimi_tool_call_{index}")
        } else {
            call.id.clone()
        };
        let raw_arguments = if call.arguments.trim().is_empty() {
            "{}".to_string()
        } else {
            call.arguments.clone()
        };
        let input = serde_json::from_str(&raw_arguments).unwrap_or_else(|_| json!({}));
        let _ = tx.send(StreamEvent::ContentBlockStart {
            index: block_index,
            block: ContentBlock::ToolUse {
                id,
                name: call.name.clone(),
                input,
                gemini_context: None,
            },
        });
        let _ = tx.send(StreamEvent::InputJsonDelta {
            index: block_index,
            partial_json: raw_arguments,
        });
        let _ = tx.send(StreamEvent::ContentBlockStop {
            index: block_index,
            thinking_signature: None,
        });
    }
}

#[derive(Debug, Deserialize)]
struct KimiChunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<KimiChoice>,
    #[serde(default)]
    usage: Option<KimiUsage>,
}

#[derive(Debug, Deserialize)]
struct KimiChoice {
    #[serde(default)]
    delta: Option<KimiDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct KimiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<KimiToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct KimiToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<KimiFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct KimiFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KimiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<KimiPromptTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct KimiPromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn request_replays_reasoning_content_with_tool_calls() {
        let client = KimiK3Client::new("test-key", "https://api.moonshot.ai/v1").unwrap();
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Thinking {
                        thinking: "I should inspect the file first.".to_string(),
                        signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "call_read".to_string(),
                        name: "read".to_string(),
                        input: json!({"path": "Cargo.toml"}),
                        gemini_context: None,
                    },
                ]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_read".to_string(),
                    content: "[package]".to_string(),
                    is_error: None,
                }]),
            },
        ];

        let body = client.build_request_body(
            &messages,
            &RequestConfig {
                model: "moonshot/kimi-k3".to_string(),
                ..Default::default()
            },
        );
        let request_messages = body["messages"].as_array().unwrap();
        assert_eq!(request_messages[0]["role"], "assistant");
        assert_eq!(
            request_messages[0]["reasoning_content"],
            "I should inspect the file first."
        );
        assert!(request_messages[0]["content"].is_null());
        assert_eq!(request_messages[0]["tool_calls"][0]["id"], "call_read");
        assert_eq!(request_messages[1]["role"], "tool");
        assert_eq!(request_messages[1]["tool_call_id"], "call_read");
        assert_eq!(body["model"], "kimi-k3");
    }

    #[test]
    fn unmatched_results_in_a_mixed_batch_reach_the_model_as_text() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "call_a".to_string(),
                    name: "read".to_string(),
                    input: json!({}),
                    gemini_context: None,
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_a".to_string(),
                        content: "matched".to_string(),
                        is_error: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_stale".to_string(),
                        content: "retained evidence".to_string(),
                        is_error: None,
                    },
                ]),
            },
        ];

        let converted = convert_messages(&messages);
        assert_eq!(converted[1]["role"], "tool");
        assert_eq!(converted[1]["tool_call_id"], "call_a");
        assert_eq!(converted[2]["role"], "user");
        let text = converted[2]["content"].as_str().unwrap();
        assert!(text.contains("retained evidence"), "{text}");
        assert_eq!(converted.len(), 3);
    }

    #[test]
    fn thinking_budget_maps_to_kimi_supported_effort_levels() {
        let client = KimiK3Client::new("test-key", "https://api.moonshot.ai/v1").unwrap();
        for (budget, expected) in [(1_000, "low"), (5_000, "high"), (12_000, "max")] {
            let body = client.build_request_body(
                &[],
                &RequestConfig {
                    model: "kimi-k3".to_string(),
                    thinking: Some(super::super::types::ThinkingConfig::enabled(budget)),
                    ..Default::default()
                },
            );
            assert_eq!(body["reasoning_effort"], expected);
        }
    }

    #[tokio::test]
    async fn stream_emits_reasoning_tools_usage_and_terminal_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).unwrap();
            let body = [
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": "chatcmpl-k3",
                        "model": "kimi-k3",
                        "choices": [{
                            "delta": {"reasoning_content": "Need a tool."},
                            "finish_reason": null
                        }]
                    })
                ),
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": "chatcmpl-k3",
                        "model": "kimi-k3",
                        "choices": [{
                            "delta": {"tool_calls": [{
                                "index": 0,
                                "id": "call_read",
                                "function": {"name": "read", "arguments": "{\"path\":"}
                            }]},
                            "finish_reason": null
                        }]
                    })
                ),
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": "chatcmpl-k3",
                        "model": "kimi-k3",
                        "choices": [{
                            "delta": {"tool_calls": [{
                                "index": 0,
                                "function": {"arguments": "\"Cargo.toml\"}"}
                            }]},
                            "finish_reason": "tool_calls"
                        }]
                    })
                ),
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": "chatcmpl-k3",
                        "model": "kimi-k3",
                        "choices": [],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 20,
                            "prompt_tokens_details": {"cached_tokens": 4}
                        }
                    })
                ),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let client = KimiK3Client::new("test-key", format!("http://{address}/v1")).unwrap();
        let mut rx = client
            .stream(
                &[],
                &RequestConfig {
                    model: "kimi-k3".to_string(),
                    tools: Arc::new(vec![Tool::new("read", "Read a file")]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let terminal = matches!(event, StreamEvent::MessageStop { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        server.join().unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ThinkingDelta { thinking, .. } if thinking == "Need a tool."
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockStart {
                block: ContentBlock::ToolUse { id, name, input , .. },
                ..
            } if id == "call_read" && name == "read" && input == &json!({"path": "Cargo.toml"})
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: Some(4),
                ..
            }
        )));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageStop {
                stop_reason: Some(StopReason::ToolUse)
            })
        ));
    }
}
