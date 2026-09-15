//! Anthropic API Client
//!
//! Implements streaming communication with Anthropic's Claude API using Server-Sent Events (SSE).
//! Supports Claude Opus 4.5, Sonnet 4.5, and other Claude models.
//!
//! # API Overview
//!
//! The Anthropic API uses HTTP POST requests to `/v1/messages` with streaming responses.
//! Responses are delivered as Server-Sent Events (SSE), a text-based streaming protocol
//! where events are delimited by double newlines (`\n\n`).
//!
//! # Rust Concepts
//!
//! ## Async HTTP with Reqwest
//!
//! The `reqwest` crate provides async HTTP functionality:
//!
//! - `Client::builder()`: Configure HTTP client with timeouts
//! - `.post(url).headers().json()`: Build POST request
//! - `.send().await`: Execute request asynchronously
//! - `.bytes_stream()`: Get streaming response body
//!
//! ## Streaming with Futures
//!
//! The module uses the `futures` crate for async stream processing:
//!
//! - `StreamExt::next()`: Await next item from stream
//! - `while let Some(chunk) = stream.next().await`: Process chunks as they arrive
//!
//! ## Channels for Communication
//!
//! Uses `tokio::sync::mpsc` for passing events between tasks:
//!
//! - `unbounded_channel()`: Create sender/receiver pair with no backpressure
//! - `tx.send(event)`: Send event to receiver (returns Err if receiver dropped)
//! - Background task: Spawned with `tokio::spawn`, processes SSE stream independently
//!
//! ## Server-Sent Events (SSE) Parsing
//!
//! SSE format:
//!
//! ```text
//! event: message_start
//! data: {"type":"message_start","message":{"id":"msg_123"}}
//!
//! event: content_block_delta
//! data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}
//! ```
//!
//! The `parse_sse_event` function:
//! 1. Splits SSE text into lines
//! 2. Extracts `event:` and `data:` fields
//! 3. Deserializes JSON data using serde
//! 4. Converts to unified `StreamEvent` enum
//!
//! ## Serde JSON Deserialization
//!
//! Uses advanced serde features:
//!
//! - `#[serde(tag = "type")]`: Discriminate enum variants by `type` field
//! - `#[serde(rename_all = "snake_case")]`: Convert variant names to `snake_case`
//! - `#[serde(default)]`: Use default value if field is missing
//! - `Option<T>`: Optional fields that may be absent from JSON
//!
//! # Example SSE Flow
//!
//! ```text
//! message_start -> content_block_start -> text_delta -> text_delta -> ...
//!                                      -> content_block_stop -> message_delta -> message_stop
//! ```
//!
//! # API Features
//!
//! - Prompt caching (via `anthropic-beta` header)
//! - Extended thinking mode (interleaved-thinking-2025-05-14)
//! - Tool use (function calling)
//! - Streaming text, thinking, and tool call deltas

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use tokio::sync::mpsc;

use super::client::{AiClient, AiProvider, provider_model_name};
use super::model_capabilities::{AnthropicThinkingMode, anthropic_request_capabilities};
use super::op_secret;
use super::transform::{OutboundTarget, transform_messages_for_target};
use super::types::{ContentBlock, Message, RequestConfig, StopReason, StreamEvent};

/// Parser state for accumulating data across SSE events within a single stream.
/// This replaces thread-local storage to avoid race conditions in async contexts.
#[derive(Debug, Default)]
struct SseParserState {
    /// Stop reason from `message_delta`, consumed by `message_stop`
    pending_stop_reason: Option<StopReason>,
    /// Thinking signature from `signature_delta`, associated with thinking blocks
    pending_thinking_signature: Option<String>,
}

const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_MESSAGES_PATH: &str = "messages";
const ANTHROPIC_COUNT_TOKENS_PATH: &str = "messages/count_tokens";
const ANTHROPIC_VERSION: &str = "2023-06-01";

fn anthropic_model_accepts_temperature(model: &str) -> bool {
    anthropic_request_capabilities(Some("anthropic"), model).temperature
}

/// Anthropic API client for Claude models
///
/// Maintains an HTTP client and API key for making requests to Anthropic's API.
/// The client is designed to be reused across multiple requests for connection pooling.
///
/// # Thread Safety
///
/// This struct implements `Send + Sync` (via the `AiClient` trait requirement),
/// allowing it to be safely shared across threads. The underlying `reqwest::Client`
/// is internally synchronized.
#[derive(Clone)]
pub struct AnthropicClient {
    /// Reusable HTTP client with connection pooling
    client: reqwest::Client,
    /// API key for authentication (via x-api-key header)
    api_key: String,
    /// Base URL for the Anthropic Messages API, without the `/messages` path.
    base_url: String,
}

impl AnthropicClient {
    /// Create a new Anthropic client
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_base_url(api_key, ANTHROPIC_DEFAULT_BASE_URL)
    }

    /// Create a client using an explicit Anthropic API base URL.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_mins(5))
            .build()
            .context("Failed to create HTTP client")?;
        let api_key = api_key.into().trim().to_string();
        let base_url = base_url.into().trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            anyhow::bail!("Anthropic base URL cannot be empty");
        }
        reqwest::Url::parse(&format!("{base_url}/{ANTHROPIC_MESSAGES_PATH}"))
            .context("Invalid Anthropic base URL")?;

        Ok(Self {
            client,
            api_key,
            base_url,
        })
    }

    /// Create a new client from environment variable
    pub fn from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["ANTHROPIC_API_KEY"])?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string());
        Self::with_base_url(api_key, base_url)
    }

    fn messages_endpoint(&self) -> String {
        format!("{}/{ANTHROPIC_MESSAGES_PATH}", self.base_url)
    }

    fn count_tokens_endpoint(&self) -> String {
        format!("{}/{ANTHROPIC_COUNT_TOKENS_PATH}", self.base_url)
    }

    /// Ask Anthropic to count the exact provider input without creating a
    /// message. Callers use this sparingly near the compaction threshold.
    pub(crate) async fn count_input_tokens(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<u64> {
        crate::cache_topology::validate_prepared(messages, config)?;
        let body = self.build_count_tokens_body(messages, config)?;
        let response = self
            .client
            .post(self.count_tokens_endpoint())
            .headers(self.headers())
            .json(&body)
            .send()
            .await
            .context("Failed to send Anthropic token-count request")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("Failed to read Anthropic token-count response")?;
        if !status.is_success() {
            anyhow::bail!(
                "Anthropic token-count API error {status}: {}",
                super::summarize_error_body(&String::from_utf8_lossy(&bytes))
            );
        }
        #[derive(Deserialize)]
        struct CountTokensResponse {
            input_tokens: u64,
        }
        Ok(serde_json::from_slice::<CountTokensResponse>(&bytes)
            .context("Invalid Anthropic token-count response")?
            .input_tokens)
    }

    /// Build request headers
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&self.api_key).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        // Enable extended features
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("prompt-caching-2024-07-31,interleaved-thinking-2025-05-14"),
        );
        headers
    }

    /// Stream a request to the API (internal implementation)
    ///
    /// This function:
    /// 1. Creates an mpsc channel for streaming events
    /// 2. Sends HTTP POST request to Anthropic API
    /// 3. Spawns background task to process SSE stream
    /// 4. Returns receiver for consuming events
    ///
    /// # SSE Parsing Strategy
    ///
    /// The background task accumulates bytes into a buffer and scans for
    /// double-newline delimiters (`\n\n`) that mark SSE event boundaries.
    /// Each complete event is parsed and sent through the channel.
    ///
    /// # Error Handling
    ///
    /// - HTTP errors: Sent as `StreamEvent::Error` through the channel
    /// - Stream errors: Sent as `StreamEvent::Error`, task exits
    /// - Receiver dropped: Task exits early (no error, consumer stopped listening)
    async fn stream_impl(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        // ─────────────────────────────────────────────────────────────
        // Channel Setup
        // ─────────────────────────────────────────────────────────────
        // Create unbounded channel: sender (tx) sends events, receiver (rx) consumes them.
        // Unbounded means no backpressure - if consumer is slow, events queue in memory.
        let (tx, rx) = mpsc::unbounded_channel();

        // ─────────────────────────────────────────────────────────────
        // Build Request
        // ─────────────────────────────────────────────────────────────
        crate::cache_topology::validate_prepared(messages, config)?;
        let body = self.build_request_body(messages, config)?;

        // ─────────────────────────────────────────────────────────────
        // Execute HTTP Request
        // ─────────────────────────────────────────────────────────────
        let response = self
            .client
            .post(self.messages_endpoint())
            .headers(self.headers())
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        // ─────────────────────────────────────────────────────────────
        // Check for HTTP Errors
        // ─────────────────────────────────────────────────────────────
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            let _ = tx.send(StreamEvent::Error {
                message: format!(
                    "API error {status}: {}",
                    super::summarize_error_body(&error_text)
                ),
            });
            return Ok(rx);
        }

        // ─────────────────────────────────────────────────────────────
        // Spawn Background Task for SSE Processing
        // ─────────────────────────────────────────────────────────────
        // Get streaming response body as a stream of byte chunks
        let mut stream = response.bytes_stream();

        // Spawn a background task to process the SSE stream.
        // This task runs independently and sends events through the channel.
        tokio::spawn(async move {
            // Buffer for accumulating incomplete SSE events
            let mut buffer = String::new();
            // Parser state - local to this stream, avoiding thread-local race conditions
            let mut parser_state = SseParserState::default();

            // Process chunks as they arrive from the HTTP stream
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        // Convert bytes to UTF-8 and append to buffer
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        // Process all complete SSE events in the buffer
                        // SSE events are delimited by double newlines: "\n\n"
                        while let Some(pos) = buffer.find("\n\n") {
                            let event_data = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();

                            // Parse SSE event and send to receiver
                            if let Some(event) = parse_sse_event(&event_data, &mut parser_state) {
                                if tx.send(event).is_err() {
                                    return; // Receiver dropped - consumer stopped listening
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Stream error - notify receiver and exit
                        let _ = tx.send(StreamEvent::Error {
                            message: format!("Stream error: {e}"),
                        });
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    /// Build the request body
    ///
    /// # Prompt Caching
    ///
    /// When `cache_system_prompt` is enabled, the system prompt is formatted
    /// as a content block array with a `cache_control` marker. This enables
    /// Anthropic's prompt caching feature which can significantly reduce
    /// latency and cost for repeated requests with the same system prompt.
    ///
    /// The cached system prompt format is:
    /// ```json
    /// "system": [
    ///   {
    ///     "type": "text",
    ///     "text": "Your system prompt here...",
    ///     "cache_control": { "type": "ephemeral" }
    ///   }
    /// ]
    /// ```
    pub(crate) fn build_request_body(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<serde_json::Value> {
        let model = provider_model_name(&config.model);
        let capabilities = anthropic_request_capabilities(Some("anthropic"), &model);
        let previous_checkpoint = config
            .cache_topology
            .as_ref()
            .and_then(|prepared| prepared.previous_checkpoint())
            .filter(|index| *index < messages.len())
            .and_then(|index| {
                transform_messages_for_target(&messages[..=index], OutboundTarget::Anthropic)
                    .len()
                    .checked_sub(1)
            });
        let messages = transform_messages_for_target(messages, OutboundTarget::Anthropic);
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": config.max_tokens,
            "messages": messages,
            "stream": true,
        });

        // Add system prompt with optional caching
        if let Some(system) = &config.system {
            if config.cache_system_prompt {
                // Use cache_control format for prompt caching
                body["system"] = serde_json::json!([{
                    "type": "text",
                    "text": system,
                    "cache_control": { "type": "ephemeral" }
                }]);
            } else {
                // Simple string format (no caching)
                body["system"] = serde_json::json!(system);
            }
        }

        if let Some(temp) = config
            .temperature
            .filter(|_| anthropic_model_accepts_temperature(&model))
        {
            body["temperature"] = serde_json::json!(temp);
        }

        // Add tools with cache_control on the last tool for tool definition caching
        if !config.tools.is_empty() {
            if config.cache_system_prompt {
                // Add cache_control to the last tool for tool definition caching
                let mut tools_json: Vec<serde_json::Value> =
                    config.tools.iter().map(|t| serde_json::json!(t)).collect();

                // Add cache_control to the last tool
                if let Some(last_tool) = tools_json.last_mut() {
                    if let Some(obj) = last_tool.as_object_mut() {
                        obj.insert(
                            "cache_control".to_string(),
                            serde_json::json!({ "type": "ephemeral" }),
                        );
                    }
                }

                body["tools"] = serde_json::json!(tools_json);
            } else {
                body["tools"] = serde_json::json!(config.tools.as_ref());
            }
        }

        if let Some(thinking) = &config.thinking {
            match capabilities.thinking {
                AnthropicThinkingMode::Extended => {
                    body["thinking"] = serde_json::json!(thinking);
                }
                AnthropicThinkingMode::Adaptive => {
                    if thinking.thinking_type.eq_ignore_ascii_case("disabled") {
                        body["thinking"] = serde_json::json!({ "type": "disabled" });
                    } else {
                        body["thinking"] = serde_json::json!({ "type": "adaptive" });
                        if let Some(effort) = capabilities.effort_for_budget(thinking.budget_tokens)
                        {
                            body["output_config"] = serde_json::json!({ "effort": effort });
                        }
                    }
                }
                AnthropicThinkingMode::AlwaysOn => {
                    // Always-on models reject both legacy `enabled` and
                    // `disabled`; effort is the supported depth control.
                    if !thinking.thinking_type.eq_ignore_ascii_case("disabled") {
                        if let Some(effort) = capabilities.effort_for_budget(thinking.budget_tokens)
                        {
                            body["output_config"] = serde_json::json!({ "effort": effort });
                        }
                    }
                }
            }
        } else if capabilities.thinking == AnthropicThinkingMode::Adaptive {
            // Native `None` is the UI's explicit Off setting. Adaptive models
            // such as Opus 5 default to thinking on, so omission would change
            // that user choice at the provider boundary. All supported
            // adaptive families accept disabled at the default high effort.
            body["thinking"] = serde_json::json!({ "type": "disabled" });
        }

        if let Some(prepared) = &config.cache_topology {
            if config.cache_system_prompt {
                crate::cache_topology::mark_stable_history(&mut body, "5m", previous_checkpoint);
            }
            prepared.append_volatile_tail(&mut body);
        }
        Ok(body)
    }

    fn build_count_tokens_body(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<serde_json::Value> {
        let mut body = self.build_request_body(messages, config)?;
        let Some(object) = body.as_object_mut() else {
            anyhow::bail!("Anthropic request body must be an object");
        };
        object.remove("stream");
        object.remove("max_tokens");
        object.remove("temperature");
        Ok(body)
    }
}

impl AiClient for AnthropicClient {
    fn provider(&self) -> AiProvider {
        AiProvider::Anthropic
    }

    async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        self.stream_impl(messages, config).await
    }
}

/// Parse an SSE event into a `StreamEvent`
///
/// # SSE Format
///
/// Each SSE event consists of multiple lines:
///
/// ```text
/// event: content_block_delta
/// data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}
/// ```
///
/// # Parsing Strategy
///
/// 1. Extract `event:` and `data:` lines from the SSE block
/// 2. Match on event type to determine which variant to parse
/// 3. Deserialize JSON data into appropriate Rust struct using serde
/// 4. Convert API-specific struct to unified `StreamEvent` enum
///
/// # State Management
///
/// The `state` parameter maintains context across SSE events within a single stream:
/// - `pending_stop_reason`: Captured from `message_delta`, consumed by `message_stop`
/// - `pending_thinking_signature`: Captured from `signature_delta` for thinking blocks
///
/// This approach replaces thread-local storage to avoid race conditions when
/// multiple streams run concurrently in async contexts.
///
/// # Serde Tagged Enums
///
/// Many API types use `#[serde(tag = "type")]` for JSON discrimination:
///
/// ```rust,ignore
/// #[serde(tag = "type", rename_all = "snake_case")]
/// enum DeltaType {
///     TextDelta { text: String },
///     ThinkingDelta { thinking: String },
///     InputJsonDelta { partial_json: String },
/// }
/// ```
///
/// This means JSON like `{"type": "text_delta", "text": "Hi"}` automatically
/// deserializes to the correct variant based on the `type` field.
///
/// # Returns
///
/// - `Some(StreamEvent)` if parsing succeeds
/// - `None` if parsing fails or event should be ignored (e.g., ping events)
fn parse_sse_event(data: &str, state: &mut SseParserState) -> Option<StreamEvent> {
    let mut event_type = None;
    let mut event_data = None;

    // ─────────────────────────────────────────────────────────────
    // Extract event type and data from SSE lines
    // ─────────────────────────────────────────────────────────────
    for line in data.lines() {
        if let Some(t) = line.strip_prefix("event: ") {
            event_type = Some(t.trim());
        } else if let Some(d) = line.strip_prefix("data: ") {
            event_data = Some(d.trim());
        }
    }

    let event_type = event_type?;
    let event_data = event_data?;

    match event_type {
        "message_start" => {
            let parsed: MessageStartEvent = serde_json::from_str(event_data).ok()?;
            Some(StreamEvent::MessageStart {
                id: parsed.message.id,
                model: parsed.message.model,
            })
        }
        "content_block_start" => {
            let parsed: ContentBlockStartEvent = serde_json::from_str(event_data).ok()?;
            let block = match parsed.content_block {
                RawContentBlock::Text { text } => ContentBlock::Text { text },
                RawContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
                    id,
                    name,
                    input: input.unwrap_or(serde_json::Value::Object(Default::default())),
                    gemini_context: None,
                },
                RawContentBlock::Thinking { thinking } => ContentBlock::Thinking {
                    thinking,
                    signature: None, // Will be populated from signature_delta
                },
            };
            Some(StreamEvent::ContentBlockStart {
                index: parsed.index,
                block,
            })
        }
        "content_block_delta" => {
            let parsed: ContentBlockDeltaEvent = serde_json::from_str(event_data).ok()?;
            match parsed.delta {
                DeltaType::TextDelta { text } => Some(StreamEvent::TextDelta {
                    index: parsed.index,
                    text,
                }),
                DeltaType::ThinkingDelta { thinking } => Some(StreamEvent::ThinkingDelta {
                    index: parsed.index,
                    thinking,
                }),
                DeltaType::InputJsonDelta { partial_json } => Some(StreamEvent::InputJsonDelta {
                    index: parsed.index,
                    partial_json,
                }),
                DeltaType::SignatureDelta { signature } => {
                    // Store signature in state for association with thinking block
                    state.pending_thinking_signature = Some(signature.clone());
                    Some(StreamEvent::ThinkingSignature {
                        index: parsed.index,
                        signature,
                    })
                }
            }
        }
        "content_block_stop" => {
            let parsed: ContentBlockStopEvent = serde_json::from_str(event_data).ok()?;
            // Take pending signature - it will be associated with the stopped block
            let signature = state.pending_thinking_signature.take();
            Some(StreamEvent::ContentBlockStop {
                index: parsed.index,
                thinking_signature: signature,
            })
        }
        "message_delta" => {
            let parsed: MessageDeltaEvent = serde_json::from_str(event_data).ok()?;
            // Store stop_reason in parser state for message_stop to pick up
            // This replaces thread-local storage to avoid race conditions
            if let Some(delta) = &parsed.delta {
                if delta.stop_reason.is_some() {
                    state.pending_stop_reason = delta.stop_reason;
                }
            }
            parsed.usage.map(|usage| StreamEvent::Usage {
                input_tokens: usage.input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
                cache_read_tokens: usage.cache_read_input_tokens,
                cache_creation_tokens: usage.cache_creation_input_tokens,
            })
        }
        "message_stop" => {
            // Consume the pending stop_reason from parser state
            let stop_reason = state.pending_stop_reason.take();
            Some(StreamEvent::MessageStop { stop_reason })
        }
        "error" => {
            let parsed: ErrorEvent = serde_json::from_str(event_data).ok()?;
            Some(StreamEvent::Error {
                message: parsed.error.message,
            })
        }
        "ping" => None, // Ignore pings
        _ => None,
    }
}

// ============================================================================
// SSE Event Parsing Types
// ============================================================================
//
// These types mirror Anthropic's API response structure and are used for
// deserializing SSE event JSON data. They are internal implementation details
// and are converted to the unified `StreamEvent` enum for public consumption.

/// Message start event - first event in a streaming response
///
/// Contains metadata about the message being generated (ID, model used).
#[derive(Debug, Deserialize)]
struct MessageStartEvent {
    message: MessageInfo,
}

/// Message metadata (ID and model name)
#[derive(Debug, Deserialize)]
struct MessageInfo {
    id: String,
    model: String,
}

/// Content block start event - signals start of a new content block
///
/// The `index` field identifies which content block this is (0-based).
/// Multiple blocks can exist in a single message (e.g., text + `tool_use`).
#[derive(Debug, Deserialize)]
struct ContentBlockStartEvent {
    index: usize,
    content_block: RawContentBlock,
}

/// Raw content block from API (before conversion to unified type)
///
/// # Serde Tagged Enum
///
/// The `#[serde(tag = "type")]` attribute tells serde to discriminate
/// variants based on a `type` field in the JSON. For example:
///
/// ```json
/// {"type": "text", "text": "Hello"}          -> Text variant
/// {"type": "tool_use", "id": "...", ...}     -> ToolUse variant
/// {"type": "thinking", "thinking": "..."}    -> Thinking variant
/// ```
///
/// The `rename_all = "snake_case"` converts Rust's `PascalCase` variant names
/// to `snake_case` for JSON (Text -> "text", `ToolUse` -> "`tool_use`").
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawContentBlock {
    /// Text content block
    Text {
        /// Text content (may be empty at start, filled by deltas)
        #[serde(default)]
        text: String,
    },
    /// Tool use block (function call)
    ToolUse {
        /// Unique ID for this tool call
        id: String,
        /// Name of the tool being called
        name: String,
        /// Tool arguments (streamed incrementally, may be None initially)
        #[serde(default)]
        input: Option<serde_json::Value>,
    },
    /// Thinking block (extended thinking mode)
    Thinking {
        /// Thinking content (streamed incrementally)
        #[serde(default)]
        thinking: String,
    },
}

/// Content block delta event - incremental updates to a content block
///
/// Sent repeatedly to stream content as it's generated. The `index` field
/// identifies which content block is being updated.
#[derive(Debug, Deserialize)]
struct ContentBlockDeltaEvent {
    index: usize,
    delta: DeltaType,
}

/// Type of delta update
///
/// # Serde Tagged Enum Example
///
/// This demonstrates serde's tagged enum deserialization:
///
/// ```json
/// {"type": "text_delta", "text": "Hi"}
/// ```
///
/// Serde:
/// 1. Reads `"type": "text_delta"`
/// 2. Converts "`text_delta`" to `TextDelta` variant (via `rename_all`)
/// 3. Deserializes remaining fields (`text`) into that variant
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // API-defined names
enum DeltaType {
    /// Text content delta
    TextDelta { text: String },
    /// Thinking content delta (extended thinking mode)
    ThinkingDelta { thinking: String },
    /// Partial JSON for tool arguments (streamed incrementally)
    InputJsonDelta { partial_json: String },
    /// Signature for thinking block (required for replaying thinking to API)
    SignatureDelta { signature: String },
}

#[derive(Debug, Deserialize)]
struct ContentBlockStopEvent {
    index: usize,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaEvent {
    #[serde(default)]
    usage: Option<UsageInfo>,
    #[serde(default)]
    delta: Option<MessageDelta>,
}

#[derive(Debug, Deserialize)]
struct MessageDelta {
    #[serde(default)]
    stop_reason: Option<super::types::StopReason>,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ErrorEvent {
    error: ErrorInfo,
}

#[derive(Debug, Deserialize)]
struct ErrorInfo {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageContent, Role, ThinkingConfig, Tool};

    /// Helper to create a fresh parser state for tests
    fn new_state() -> SseParserState {
        SseParserState::default()
    }

    #[test]
    fn parse_message_start() {
        let data = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-3-opus-20240229","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(matches!(event, StreamEvent::MessageStart { id, .. } if id == "msg_123"));
    }

    #[test]
    fn parse_text_delta() {
        let data = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(matches!(event, StreamEvent::TextDelta { text, .. } if text == "Hello"));
    }

    #[test]
    fn test_build_request_body_basic() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "anthropic/claude-3-opus-20240229".to_string(),
            max_tokens: 4096,
            system: Some("You are a helpful assistant.".to_string()),
            cache_system_prompt: false,
            cache_topology: None,
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        assert_eq!(body["model"], "claude-3-opus-20240229");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], true);
        // Without caching, system is a simple string
        assert_eq!(body["system"], "You are a helpful assistant.");
    }

    #[test]
    fn count_tokens_body_reuses_the_exact_input_without_generation_fields() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "anthropic/claude-sonnet-4-5".to_owned(),
            max_tokens: 8_192,
            temperature: Some(0.4),
            system: Some("system contract".to_owned()),
            tools: std::sync::Arc::new(vec![Tool::new("read", "Read a file")]),
            ..Default::default()
        };
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::text("hello"),
        }];

        let body = client.build_count_tokens_body(&messages, &config).unwrap();

        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["system"], "system contract");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert!(body.get("stream").is_none());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn test_build_request_body_uses_model_thinking_capabilities() {
        let client = AnthropicClient::new("test-key").unwrap();

        let modern_config = RequestConfig {
            model: "anthropic/claude-opus-4-7".to_string(),
            thinking: Some(ThinkingConfig::enabled(16_000)),
            ..Default::default()
        };
        let modern_body = client.build_request_body(&[], &modern_config).unwrap();
        assert_eq!(modern_body["thinking"]["type"], "adaptive");
        assert_eq!(modern_body["output_config"]["effort"], "high");
        assert!(modern_body["thinking"].get("budget_tokens").is_none());

        let always_on_config = RequestConfig {
            model: "claude-fable-5-1".to_string(),
            thinking: Some(ThinkingConfig::enabled(16_000)),
            ..Default::default()
        };
        let always_on_body = client.build_request_body(&[], &always_on_config).unwrap();
        assert!(always_on_body.get("thinking").is_none());
        assert_eq!(always_on_body["output_config"]["effort"], "high");

        let legacy_config = RequestConfig {
            model: "claude-opus-4-5".to_string(),
            thinking: Some(ThinkingConfig::enabled(16_000)),
            ..Default::default()
        };
        let legacy_body = client.build_request_body(&[], &legacy_config).unwrap();
        assert_eq!(legacy_body["thinking"]["type"], "enabled");
        assert_eq!(legacy_body["thinking"]["budget_tokens"], 16_000);
        assert!(legacy_body.get("output_config").is_none());
    }

    #[test]
    fn test_build_request_body_preserves_explicit_off_for_adaptive_models() {
        let client = AnthropicClient::new("test-key").unwrap();

        let adaptive_body = client
            .build_request_body(
                &[],
                &RequestConfig {
                    model: "claude-opus-5".to_string(),
                    thinking: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(adaptive_body["thinking"]["type"], "disabled");
        assert!(adaptive_body.get("output_config").is_none());

        let always_on_body = client
            .build_request_body(
                &[],
                &RequestConfig {
                    model: "claude-fable-5-1".to_string(),
                    thinking: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(always_on_body.get("thinking").is_none());

        let legacy_body = client
            .build_request_body(
                &[],
                &RequestConfig {
                    model: "claude-opus-4-5".to_string(),
                    thinking: None,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(legacy_body.get("thinking").is_none());
    }

    #[test]
    fn test_build_request_body_adapts_only_unsigned_thinking_history() {
        let client = AnthropicClient::new("test-key").unwrap();
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Thinking {
                        thinking: "cross-provider reasoning".to_string(),
                        signature: None,
                    },
                    ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: Some("claude-signature".to_string()),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_123".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "README.md"}),
                        gemini_context: None,
                    },
                ]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_123".to_string(),
                    content: "contents".to_string(),
                    is_error: Some(false),
                }]),
            },
        ];
        let config = RequestConfig {
            model: "claude-opus-4-7".to_string(),
            ..Default::default()
        };

        let body = client.build_request_body(&messages, &config).unwrap();
        let assistant_blocks = body["messages"][0]["content"]
            .as_array()
            .expect("assistant content blocks");
        assert_eq!(assistant_blocks[0]["type"], "text");
        assert_eq!(assistant_blocks[0]["text"], "cross-provider reasoning");
        assert_eq!(assistant_blocks[1]["type"], "thinking");
        assert_eq!(assistant_blocks[1]["thinking"], "");
        assert_eq!(assistant_blocks[1]["signature"], "claude-signature");
        assert_eq!(assistant_blocks[2]["id"], "toolu_123");
        assert_eq!(
            body["messages"][1]["content"][0]["tool_use_id"],
            "toolu_123"
        );

        let MessageContent::Blocks(original_blocks) = &messages[0].content else {
            panic!("expected block history");
        };
        assert!(matches!(
            &original_blocks[0],
            ContentBlock::Thinking {
                signature: None,
                thinking
            } if thinking == "cross-provider reasoning"
        ));
    }

    #[test]
    fn modern_claude_requests_omit_unsupported_sampling_when_thinking_is_off() {
        let client = AnthropicClient::new("test-key").unwrap();
        for model in [
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-mythos-5",
            "claude-mythos-5-1",
            "claude-mythos-preview",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-opus-4-8",
        ] {
            let body = client
                .build_request_body(
                    &[],
                    &RequestConfig {
                        model: model.to_owned(),
                        temperature: Some(0.7),
                        thinking: None,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(body.get("temperature").is_none(), "{model}: {body}");
        }
    }

    #[test]
    fn trims_base_url_before_joining_messages_endpoint() {
        let client =
            AnthropicClient::with_base_url("test-key", "http://127.0.0.1:1234/v1/").unwrap();
        assert_eq!(
            client.messages_endpoint(),
            "http://127.0.0.1:1234/v1/messages"
        );
    }

    #[test]
    fn test_build_request_body_keeps_temperature_for_models_that_accept_it() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "claude-sonnet-4-5-20250514".to_string(),
            temperature: Some(0.7),
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        let temperature = body["temperature"].as_f64().unwrap();
        assert!((temperature - 0.7).abs() < 0.000_001);
    }

    #[test]
    fn test_build_request_body_omits_temperature_for_opus_47() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "anthropic/claude-opus-4-7".to_string(),
            temperature: Some(0.7),
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn test_build_request_body_omits_temperature_for_opus_47_snapshot() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "claude-opus-4-7-20260520".to_string(),
            temperature: Some(0.7),
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn test_build_request_body_omits_temperature_for_opus_4_family() {
        let client = AnthropicClient::new("test-key").unwrap();

        for model in [
            "claude-opus-4",
            "claude-opus-4-1-20250805",
            "claude-opus-4-6",
            "claude-opus-4.7",
            "anthropic/claude-opus-4.7",
            "claude-opus-latest",
        ] {
            let config = RequestConfig {
                model: model.to_string(),
                temperature: Some(0.7),
                ..Default::default()
            };

            let body = client.build_request_body(&[], &config).unwrap();

            assert!(
                body.get("temperature").is_none(),
                "{model} should omit temperature"
            );
        }
    }

    #[test]
    fn test_build_request_body_keeps_temperature_for_older_opus() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "claude-3-opus-20240229".to_string(),
            temperature: Some(0.7),
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        let temperature = body["temperature"].as_f64().unwrap();
        assert!((temperature - 0.7).abs() < 0.000_001);
    }

    #[test]
    fn trims_api_key_before_building_headers() {
        let client = AnthropicClient::new("  test-key\n").unwrap();
        let headers = client.headers();

        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
    }

    #[test]
    fn test_build_request_body_with_caching() {
        let client = AnthropicClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "claude-3-opus-20240229".to_string(),
            max_tokens: 4096,
            system: Some("You are a helpful assistant.".to_string()),
            cache_system_prompt: true,
            cache_topology: None,
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        // With caching, system is an array with cache_control
        let system = body["system"].as_array().expect("system should be array");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are a helpful assistant.");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_build_request_body_tools_with_caching() {
        let client = AnthropicClient::new("test-key").unwrap();
        let tools = vec![
            Tool::new("read", "Read a file"),
            Tool::new("write", "Write a file"),
        ];
        let config = RequestConfig {
            model: "claude-3-opus-20240229".to_string(),
            max_tokens: 4096,
            tools: tools.into(),
            cache_system_prompt: true,
            cache_topology: None,
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        let tools = body["tools"].as_array().expect("tools should be array");
        assert_eq!(tools.len(), 2);

        // First tool should NOT have cache_control
        assert!(tools[0].get("cache_control").is_none());

        // Last tool should have cache_control
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_build_request_body_tools_without_caching() {
        let client = AnthropicClient::new("test-key").unwrap();
        let tools = vec![Tool::new("read", "Read a file")];
        let config = RequestConfig {
            model: "claude-3-opus-20240229".to_string(),
            max_tokens: 4096,
            tools: tools.into(),
            cache_system_prompt: false,
            cache_topology: None,
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config).unwrap();

        let tools = body["tools"].as_array().expect("tools should be array");
        // Without caching, no cache_control should be added
        assert!(tools[0].get("cache_control").is_none());
    }

    #[test]
    fn parse_thinking_delta() {
        let data = r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(
            matches!(event, StreamEvent::ThinkingDelta { thinking, index } if thinking == "Let me think..." && index == 1)
        );
    }

    #[test]
    fn parse_input_json_delta() {
        let data = r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(
            matches!(event, StreamEvent::InputJsonDelta { partial_json, index } if partial_json == "{\"path\":" && index == 2)
        );
    }

    #[test]
    fn parse_content_block_start_text() {
        let data = r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlock::Text { .. }
            }
        ));
    }

    #[test]
    fn parse_content_block_start_tool_use() {
        let data = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_123","name":"read_file"}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        match event {
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlock::ToolUse { id, name, .. },
            } => {
                assert_eq!(id, "toolu_123");
                assert_eq!(name, "read_file");
            }
            _ => panic!("Expected ContentBlockStart with ToolUse"),
        }
    }

    #[test]
    fn parse_content_block_stop() {
        let data = r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockStop {
                index: 0,
                thinking_signature: None
            }
        ));
    }

    #[test]
    fn parse_message_delta_with_usage() {
        let data = r#"event: message_delta
data: {"type":"message_delta","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":80,"cache_creation_input_tokens":20}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        match event {
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(output_tokens, 50);
                assert_eq!(cache_read_tokens, Some(80));
                assert_eq!(cache_creation_tokens, Some(20));
            }
            _ => panic!("Expected Usage event"),
        }
    }

    #[test]
    fn parse_error_event() {
        let data = r#"event: error
data: {"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded"}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        match event {
            StreamEvent::Error { message } => {
                assert_eq!(message, "Rate limit exceeded");
            }
            _ => panic!("Expected Error event"),
        }
    }

    #[test]
    fn parse_ping_returns_none() {
        let data = r"event: ping
data: {}";

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state);
        assert!(event.is_none());
    }

    #[test]
    fn parse_unknown_event_returns_none() {
        let data = r#"event: some_unknown_event
data: {"some": "data"}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state);
        assert!(event.is_none());
    }

    #[test]
    fn parse_signature_delta() {
        let data = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc123"}}"#;

        let mut state = new_state();
        let event = parse_sse_event(data, &mut state).unwrap();
        assert!(
            matches!(event, StreamEvent::ThinkingSignature { signature, index } if signature == "sig_abc123" && index == 0)
        );
        // Signature should be stored in state
        assert_eq!(
            state.pending_thinking_signature,
            Some("sig_abc123".to_string())
        );
    }

    #[test]
    fn parse_content_block_stop_with_signature() {
        let mut state = new_state();
        // First, process a signature_delta
        let sig_data = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"thinking_sig_xyz"}}"#;
        let _ = parse_sse_event(sig_data, &mut state);

        // Then process content_block_stop
        let stop_data = r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#;
        let event = parse_sse_event(stop_data, &mut state).unwrap();

        match event {
            StreamEvent::ContentBlockStop {
                index: 0,
                thinking_signature,
            } => {
                assert_eq!(thinking_signature, Some("thinking_sig_xyz".to_string()));
            }
            _ => panic!("Expected ContentBlockStop with signature"),
        }
        // Signature should be consumed
        assert!(state.pending_thinking_signature.is_none());
    }

    #[test]
    fn parse_stop_reason_via_state() {
        let mut state = new_state();

        // First, process message_delta with stop_reason
        let delta_data = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        let _ = parse_sse_event(delta_data, &mut state);

        // Verify stop_reason is stored in state
        assert!(matches!(
            state.pending_stop_reason,
            Some(StopReason::EndTurn)
        ));

        // Then process message_stop
        let stop_data = r#"event: message_stop
data: {"type":"message_stop"}"#;
        let event = parse_sse_event(stop_data, &mut state).unwrap();

        match event {
            StreamEvent::MessageStop { stop_reason } => {
                assert!(matches!(stop_reason, Some(StopReason::EndTurn)));
            }
            _ => panic!("Expected MessageStop"),
        }
        // Stop reason should be consumed
        assert!(state.pending_stop_reason.is_none());
    }
}
