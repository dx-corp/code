//! `OpenAI` API Client
//!
//! Implements streaming communication with `OpenAI`'s APIs, supporting two different endpoints:
//!
//! # API Endpoints
//!
//! ## Chat Completions API (`/v1/chat/completions`)
//!
//! Used by standard models like GPT-4o, GPT-4 Turbo, o1-preview, o1-mini:
//!
//! - Request format: Array of messages with role/content
//! - Streaming: Line-based SSE with `data:` prefix
//! - Tool calls: Accumulated in delta events, sent in completion
//! - Special marker: `data: [DONE]` signals end of stream
//!
//! ## Responses API (`/v1/responses`)
//!
//! Used by advanced models like gpt-5.1-codex-max, gpt-5.1-codex-lite, o3:
//!
//! - Request format: Array of `ResponseItems` (messages, function calls, outputs)
//! - Streaming: Structured SSE with distinct event types
//! - Tool calls: Sent as separate `function_call` items
//! - Reasoning: Native reasoning/thinking support with encrypted content
//! - Schema restrictions: No oneOf/anyOf/allOf at top level
//!
//! Note: Responses API models may require `ChatGPT` Plus authentication.
//!
//! # Rust Concepts
//!
//! ## Two SSE Parsing Strategies
//!
//! This module demonstrates two approaches to parsing Server-Sent Events:
//!
//! ### 1. Manual Line-Based Parsing (Chat Completions)
//!
//! ```rust,ignore
//! let mut buffer = String::new();
//! while let Some(chunk) = stream.next().await {
//!     buffer.push_str(&String::from_utf8_lossy(&chunk?));
//!     while let Some(pos) = buffer.find('\n') {
//!         let line = buffer[..pos].trim();
//!         // Process line...
//!     }
//! }
//! ```
//!
//! Advantages: Simple, full control, minimal dependencies.
//!
//! ### 2. eventsource-stream Crate (Responses API)
//!
//! ```rust,ignore
//! let mut sse_stream = stream
//!     .map(|result| result.map_err(std::io::Error::other))
//!     .eventsource();
//! while let Some(event_result) = sse_stream.next().await {
//!     // event_result contains parsed SSE event
//! }
//! ```
//!
//! Advantages: Handles reconnection, proper SSE spec compliance.
//!
//! ## Pattern Matching for API Selection
//!
//! Uses provider-aware model detection for API selection:
//!
//! ```rust,ignore
//! fn uses_responses_api(provider: Option<&str>, model: &str) -> bool {
//!     // OpenRouter model ids are opaque and stay on Chat Completions unless
//!     // the provider/model catalog explicitly maps them to Responses.
//!     provider == Some("openrouter") && model.starts_with("gpt-5.6-")
//! }
//! ```
//!
//! ## Function Composition
//!
//! The module uses helper functions to organize complex logic:
//!
//! - `classify_error()`: Maps API errors to retry strategies
//! - `extract_function_call()`: Parses function call items
//! - `filter_responses_api_tools()`: Removes incompatible tool schemas
//! - `parse_retry_after()`: Extracts retry duration from error messages
//!
//! ## Error Classification for Retries
//!
//! The `ApiError` enum classifies errors into categories:
//!
//! - `ContextWindowExceeded`: Fatal, need to reduce input
//! - `QuotaExceeded`: Fatal, billing issue
//! - `RateLimited`: Retryable with delay
//! - `Retryable`: Temporary failures (server overload)
//! - `Fatal`: Authentication, invalid requests
//!
//! # Example: Model-Specific Behavior
//!
//! ```rust,ignore
//! let body = if uses_responses_api(Some("openai"), &config.model) {
//!     self.build_responses_request_body(messages, config)
//! } else {
//!     self.build_chat_request_body(messages, config)
//! };
//! ```
//!
//! This demonstrates runtime polymorphism without trait objects - the decision
//! is made at runtime but with zero-cost static dispatch.

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::client::{AiClient, AiProvider, CancellableStream, provider_model_name};
use super::op_secret;
use super::transform::{OutboundTarget, transform_messages_for_target};
use super::types::{
    ContentBlock, ImageSource, ManagedGatewayReceipt, Message, MessageContent,
    ProviderStreamErrorKind, RequestConfig, Role, StreamEvent, Tool, ToolSchemaEnforcement,
};

// The managed gateway owns a 45-second default provider attempt, plus
// admission and terminal persistence. Let it return its authoritative result
// before the client retry owner starts another request. This bounds headers
// only; streaming still has its separate idle/cancellation controls.
pub(crate) const MANAGED_GATEWAY_RESPONSE_OPEN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);
const MANAGED_GATEWAY_RECEIPT_ID_MAX_LEN: usize = 256;
const MANAGED_GATEWAY_RECEIPT_LINEAGE_MAX_LEN: usize = 256;
const MANAGED_GATEWAY_RECEIPT_STATUS_MAX_LEN: usize = 64;

/// SSE event structure for Responses API (matches `OpenAI`'s format)
///
/// # Serde Field Attributes
///
/// - `#[serde(rename = "type")]`: Map Rust field `kind` to JSON field `type`
///   (avoiding Rust keyword)
/// - `#[serde(default)]`: Use default value (None) if field is missing
/// - `#[allow(dead_code)]`: Suppress warnings for fields reserved for future use
///
/// # Event Types
///
/// The Responses API emits many event types:
///
/// - `response.created`: Initial response metadata
/// - `response.output_item.added`: New output item started
/// - `response.output_text.delta`: Incremental text content
/// - `response.function_call_arguments.delta`: Incremental tool arguments
/// - `response.reasoning_text.delta`: Thinking/reasoning content
/// - `response.completed`: Response finished successfully
/// - `response.failed`: Error occurred
#[derive(Debug, Deserialize)]
struct ResponsesSseEvent {
    /// Event type (e.g., "response.created", "`response.output_text.delta`")
    #[serde(rename = "type")]
    kind: String,
    /// Response metadata (present in some events)
    #[serde(default)]
    response: Option<serde_json::Value>,
    /// Output item data (present in item-related events)
    #[serde(default)]
    item: Option<serde_json::Value>,
    /// Content part data (present in content-part events)
    #[serde(default)]
    part: Option<serde_json::Value>,
    /// Delta content (text, reasoning, or arguments)
    #[serde(default)]
    delta: Option<String>,
    /// Index of the content part within the output item (for multi-part content)
    #[serde(default)]
    #[allow(dead_code)] // Reserved for future multi-part content support
    content_index: Option<i64>,
    /// Index for reasoning summaries (reserved for future use)
    #[serde(default)]
    #[allow(dead_code)] // Reserved for future reasoning summary support
    summary_index: Option<i64>,
    /// Index of the output item in the response
    #[serde(default)]
    output_index: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
struct IncompleteResponseError {
    kind: ProviderStreamErrorKind,
    reason: String,
}

impl std::fmt::Display for IncompleteResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self.kind {
            ProviderStreamErrorKind::OutputTokenExhaustion => "openai_response_exhausted",
            ProviderStreamErrorKind::TransientProtocol
            | ProviderStreamErrorKind::IncompleteResponse => "openai_response_incomplete",
            ProviderStreamErrorKind::ProviderDeclaredFailure => "openai_response_failed",
        };
        write!(formatter, "{code}: reason={}", self.reason)
    }
}

impl std::error::Error for IncompleteResponseError {}

fn incomplete_response_error(response: Option<&serde_json::Value>) -> IncompleteResponseError {
    let reason = response
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let kind = if reason == "max_output_tokens" {
        ProviderStreamErrorKind::OutputTokenExhaustion
    } else {
        ProviderStreamErrorKind::IncompleteResponse
    };
    IncompleteResponseError { kind, reason }
}

const RESPONSES_MISSING_TERMINAL_EVENT_ERROR: &str =
    "openai_response_protocol_error: kind=transient reason=missing_terminal_event";

async fn send_with_response_open_timeout(
    request: reqwest::RequestBuilder,
    timeout: Option<std::time::Duration>,
) -> Result<reqwest::Response> {
    let send = request.send();
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, send)
            .await
            .map_err(|_| anyhow::anyhow!("managed gateway response headers timed out"))?,
        None => send.await,
    }
    .context("Failed to send request to OpenAI API")
}

fn required_managed_receipt_header(
    headers: &HeaderMap,
    name: &'static str,
    max_len: usize,
) -> Result<String> {
    let value = headers
        .get(name)
        .with_context(|| format!("managed Gateway response is missing required {name} header"))?
        .to_str()
        .with_context(|| format!("managed Gateway response has invalid {name} header"))?;
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("managed Gateway response has empty {name} header");
    }
    if value.len() > max_len {
        anyhow::bail!("managed Gateway response {name} header exceeds the {max_len}-byte limit");
    }
    Ok(value.to_string())
}

fn managed_provider_tools_evidence(headers: &reqwest::header::HeaderMap) -> Option<(String, u32)> {
    let digest = headers
        .get("x-evalops-provider-tools-sha256")?
        .to_str()
        .ok()?;
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let count = headers
        .get("x-evalops-provider-tool-count")?
        .to_str()
        .ok()?;
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digest.to_owned(), count.parse().ok()?))
}

fn managed_gateway_receipt(
    headers: &HeaderMap,
    expected_lineage: Option<&str>,
) -> Result<ManagedGatewayReceipt> {
    let lineage_id = required_managed_receipt_header(
        headers,
        "x-evalops-lineage-id",
        MANAGED_GATEWAY_RECEIPT_LINEAGE_MAX_LEN,
    )?;
    if let Some(expected_lineage) = expected_lineage {
        if lineage_id != expected_lineage {
            anyhow::bail!("managed Gateway response receipt lineage mismatch");
        }
    }
    Ok(ManagedGatewayReceipt {
        provider_prompt_sha256: None,
        provider_tools_sha256: None,
        provider_tool_count: None,
        request_id: required_managed_receipt_header(
            headers,
            "x-request-id",
            MANAGED_GATEWAY_RECEIPT_ID_MAX_LEN,
        )?,
        record_id: required_managed_receipt_header(
            headers,
            "x-evalops-record-id",
            MANAGED_GATEWAY_RECEIPT_ID_MAX_LEN,
        )?,
        lineage_id,
        record_status: required_managed_receipt_header(
            headers,
            "x-evalops-record-status",
            MANAGED_GATEWAY_RECEIPT_STATUS_MAX_LEN,
        )?,
    })
}

fn parse_managed_inference_authorization(encoded: &str) -> Result<(serde_json::Value, String)> {
    let authorization: serde_json::Value = serde_json::from_str(encoded)
        .context("managed inference authorization must be valid JSON")?;
    let authorization_object = authorization
        .as_object()
        .context("managed inference authorization must be a JSON object")?;
    let claims = match authorization_object.get("claims") {
        Some(claims) => claims
            .as_object()
            .context("managed inference authorization claims must be a JSON object")?,
        None => authorization_object,
    };
    let lineage_id = claims
        .get("lineage_id")
        .or_else(|| claims.get("lineageId"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("managed inference authorization is missing lineage_id")?
        .to_string();
    Ok((authorization, lineage_id))
}

fn managed_inference_context(authorization: &serde_json::Value) -> Result<serde_json::Value> {
    let object = authorization
        .as_object()
        .context("managed inference authorization must be a JSON object")?;
    let claims = object
        .get("claims")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(object);
    let claim = |snake: &str, camel: &str| {
        claims
            .get(snake)
            .or_else(|| claims.get(camel))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .with_context(|| format!("managed inference authorization is missing {snake}"))
    };
    Ok(serde_json::json!({
        "session_id": claim("session_id", "sessionId")?,
        "thread_id": claim("thread_id", "threadId")?,
        "run_id": claim("run_id", "runId")?,
        "turn_id": claim("turn_id", "turnId")?
    }))
}

fn project_managed_provider_candidates(
    provider_candidates: &serde_json::Value,
) -> Result<serde_json::Value> {
    let candidates = provider_candidates
        .as_array()
        .context("managed inference authorization provider_candidates must be an array")?;
    if candidates.is_empty() {
        anyhow::bail!(
            "managed inference authorization provider_candidates must be a non-empty array"
        );
    }

    let mut projected = Vec::with_capacity(candidates.len());
    for (index, candidate) in candidates.iter().enumerate() {
        let candidate = candidate.as_object().with_context(|| {
            format!("managed inference authorization provider candidate {index} must be an object")
        })?;
        let field = |names: &[&str], label: &str| {
            names
				.iter()
				.find_map(|name| candidate.get(*name))
				.and_then(serde_json::Value::as_str)
				.map(str::to_owned)
				.with_context(|| {
					format!(
						"managed inference authorization provider candidate {index} is missing {label}"
					)
				})
        };
        let provider = field(&["provider"], "provider")?;
        let environment = field(&["environment"], "environment")?;
        let credential_name = field(&["credential_name", "credentialName"], "credential_name")?;
        let team_id = field(&["team_id", "teamId"], "team_id")?;
        let model = field(&["model"], "model")?;
        if provider.trim().is_empty() || environment.trim().is_empty() || model.trim().is_empty() {
            anyhow::bail!(
                "managed inference authorization provider candidate {index} has an empty provider, environment, or model"
            );
        }
        for (label, value) in [
            ("provider", &provider),
            ("environment", &environment),
            ("credential_name", &credential_name),
            ("team_id", &team_id),
            ("model", &model),
        ] {
            if value != value.trim() {
                anyhow::bail!(
                    "managed inference authorization provider candidate {index} {label} must not have surrounding whitespace"
                );
            }
        }

        projected.push(serde_json::json!({
            "model": model,
            "provider_ref": {
                "provider": provider,
                "environment": environment,
                "credential_name": credential_name,
                "team_id": team_id,
            },
        }));
    }

    Ok(serde_json::Value::Array(projected))
}

fn project_managed_inference_route(
    request: &mut serde_json::Map<String, serde_json::Value>,
    authorization: &serde_json::Value,
) -> Result<()> {
    let authorization_object = authorization
        .as_object()
        .context("managed inference authorization must be a JSON object")?;
    let claims = authorization_object
        .get("claims")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(authorization_object);
    let claim = |snake: &str, camel: &str| {
        claims
            .get(snake)
            .or_else(|| claims.get(camel))
            .with_context(|| format!("managed inference authorization is missing {snake}"))
    };

    let model = claim("model", "model")?
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .context("managed inference authorization model must be a non-empty string")?;
    if model != model.trim() {
        anyhow::bail!("managed inference authorization model must not have surrounding whitespace");
    }
    let provider_candidates = claim("provider_candidates", "providerCandidates")?;
    let projected_provider_candidates = project_managed_provider_candidates(provider_candidates)?;
    let routing = claim("routing", "routingStrategy")?;
    let routing_strategy = match routing {
        serde_json::Value::Object(routing) => routing
            .get("strategy")
            .or_else(|| routing.get("routing_strategy"))
            .or_else(|| routing.get("routingStrategy"))
            .cloned()
            .context("managed inference authorization routing is missing strategy")?,
        routing => routing.clone(),
    };
    let output_token_budget = claim("output_token_budget", "outputTokenBudget")?;
    if !output_token_budget.is_object() {
        anyhow::bail!("managed inference authorization output_token_budget must be an object");
    }

    for key in [
        "provider_ref",
        "providerRef",
        "provider_refs",
        "providerRefs",
        "provider_candidates",
        "providerCandidates",
        "routing_strategy",
        "routingStrategy",
        "output_token_budget",
        "outputTokenBudget",
    ] {
        request.remove(key);
    }
    request.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    request.insert(
        "provider_candidates".to_string(),
        projected_provider_candidates,
    );
    request.insert("routing_strategy".to_string(), routing_strategy);
    request.insert(
        "output_token_budget".to_string(),
        output_token_budget.clone(),
    );
    reconcile_provider_output_limit(request, output_token_budget)?;
    Ok(())
}

/// Keep the provider payload's output cap consistent with the authorized
/// budget.
///
/// llm-gateway rejects a request whose typed budget and payload limit
/// disagree: a `provider_default` budget "cannot include a provider payload
/// limit or origin_reference", and an explicit or route-policy budget "must
/// equal the provider payload limit". The body builders set
/// `max_tokens`/`max_output_tokens` from this client's own model
/// capabilities, which knows nothing about the authorization, so every
/// provider-default turn was refused with `output_token_budget_mismatch`.
/// The authorization is the authority; make the payload agree with it.
fn reconcile_provider_output_limit(
    request: &mut serde_json::Map<String, serde_json::Value>,
    output_token_budget: &serde_json::Value,
) -> Result<()> {
    const PAYLOAD_LIMIT_KEYS: [&str; 2] = ["max_tokens", "max_output_tokens"];
    let budget = output_token_budget
        .as_object()
        .context("managed inference authorization output_token_budget must be an object")?;
    let origin = budget
        .get("origin")
        .or_else(|| budget.get("budget_origin"))
        .or_else(|| budget.get("budgetOrigin"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let value = budget.get("value").and_then(serde_json::Value::as_u64);
    let provider_default = origin.is_empty()
        || origin.ends_with("provider_default")
        || origin.ends_with("providerdefault");
    if provider_default && value.is_none() {
        for key in PAYLOAD_LIMIT_KEYS {
            request.remove(key);
        }
        return Ok(());
    }
    let Some(value) = value else {
        anyhow::bail!(
            "managed inference authorization output_token_budget origin {origin} requires a value"
        );
    };
    // Only rewrite a key the body builder already chose, so the endpoint's own
    // spelling of the limit is preserved.
    let present: Vec<&str> = PAYLOAD_LIMIT_KEYS
        .into_iter()
        .filter(|key| request.contains_key(*key))
        .collect();
    for key in present {
        request.insert(key.to_string(), serde_json::Value::from(value));
    }
    Ok(())
}

fn missing_text_suffix(emitted: &str, aggregate: &str) -> Option<String> {
    if aggregate.is_empty() || emitted.starts_with(aggregate) {
        None
    } else {
        aggregate
            .strip_prefix(emitted)
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_string)
    }
}

/// Error classification for retry logic
///
/// This enum categorizes API errors to determine the appropriate retry strategy.
/// It's used internally by error handling logic to decide whether to retry,
/// wait, or fail immediately.
///
/// # Design Pattern
///
/// This is the "parse, don't validate" pattern - instead of checking error
/// strings repeatedly, we parse once into a structured type that encodes
/// the retry decision.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// Context window exceeded - fatal, need to reduce input
    ///
    /// This error means the conversation history is too long. The client
    /// should truncate messages or use a model with a larger context window.
    ContextWindowExceeded,

    /// Quota exceeded - fatal, billing issue
    ///
    /// The API key has exceeded its usage quota. This requires manual
    /// intervention (upgrading plan, adding billing info).
    QuotaExceeded,

    /// Rate limited - retryable with delay
    ///
    /// Too many requests in a short time. Should retry after the specified
    /// duration (or a default backoff if not specified).
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },

    /// Generic retryable error
    ///
    /// Temporary server issues (overloaded, maintenance). Should retry
    /// with exponential backoff.
    Retryable { message: String },

    /// Fatal error
    ///
    /// Permanent failures (invalid API key, bad request, model not found).
    /// Retrying won't help - the request needs to be fixed.
    Fatal { message: String },
}

fn responses_call_id(id: &str) -> &str {
    id.split_once('|').map_or(id, |(call_id, _)| call_id)
}

fn responses_item_id(id: &str) -> Option<&str> {
    id.split_once('|').map(|(_, item_id)| item_id)
}

// These are the Google-protocol provider IDs and aliases accepted by the
// model-provider-core catalog. This classifies opaque replay metadata only;
// signed route validation and provider selection remain owned by the gateway.
fn supports_gemini_tool_history(provider: Option<&str>) -> bool {
    matches!(
        provider,
        Some("google" | "googleai" | "google-ai" | "gemini" | "vertex-ai" | "vertex_ai")
    )
}

/// Extract function call from a `ResponseItem`
///
/// # Pattern: Option Chaining
///
/// This function uses the `?` operator with `Option` to short-circuit if any
/// field is missing. This is more concise than nested `if let` or `match`:
///
/// ```rust,ignore
/// let item_type = item.get("type")?.as_str()?;  // Returns None if missing
/// ```
///
/// # Returns
///
/// `Some((call_id, name, arguments))` if this is a `function_call` item,
/// `None` otherwise.
fn extract_gemini_context(item: &serde_json::Value) -> Option<crate::GeminiToolContext> {
    serde_json::from_value(item.get("extra_content")?.get("google")?.clone()).ok()
}

fn extract_function_call(item: &serde_json::Value) -> Option<(String, String, serde_json::Value)> {
    let item_type = item.get("type")?.as_str()?;
    if item_type != "function_call" {
        return None;
    }

    let call_id = item.get("call_id")?.as_str()?.to_string();
    let name = item.get("name")?.as_str()?.to_string();
    let arguments = item.get("arguments")?;
    if !matches!(arguments, serde_json::Value::String(_)) {
        let kind = match arguments {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        };
        eprintln!(
            "[openai] function_call.arguments was {kind}; expected string (call_id={call_id}, name={name})"
        );
    }
    let arguments_value = match arguments {
        serde_json::Value::String(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({}))
        }
        serde_json::Value::Null => serde_json::json!({}),
        other => other.clone(),
    };

    let identity = item
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map_or_else(|| call_id.clone(), |item_id| format!("{call_id}|{item_id}"));
    Some((identity, name, arguments_value))
}

/// Whether an error message names a context-window overflow.
///
/// This is the wording test on its own, with no status-code heuristic, so it
/// is safe for any caller that must decide whether an arbitrary error means
/// "the request was too large". [`is_context_overflow_error`] adds the
/// bodyless-4xx heuristic on top for the providers that send no message at
/// all; a caller that would act destructively on a false positive — by
/// rewriting history, say — wants this function instead.
///
/// Every pattern is anchored to overflow wording. A message is not an
/// overflow because it says "took too long".
///
/// # Supported Providers
///
/// - **Anthropic**: "prompt is too long: X tokens > Y maximum"
/// - **`OpenAI`**: "exceeds the context window", `context_length_exceeded`
/// - **Google Gemini**: "input token count exceeds the maximum"
/// - **xAI (Grok)**: "maximum prompt length is X but request contains Y"
/// - **Groq**: "reduce the length of the messages"
/// - **`OpenRouter`**: "maximum context length is X tokens"
/// - **llama.cpp**: "exceeds the available context size"
/// - **LM Studio**: "greater than the context length"
#[must_use]
pub fn is_context_overflow_message(message: &str) -> bool {
    let lower = message.to_lowercase();

    // Provider-specific patterns
    if lower.contains("prompt is too long")
        || lower.contains("exceeds the context window")
        || (lower.contains("input token count") && lower.contains("exceeds the maximum"))
        || (lower.contains("maximum prompt length is") && lower.contains("contains"))
        || lower.contains("reduce the length")
        || lower.contains("maximum context length is")
        || lower.contains("exceeds the available context size")
        || lower.contains("greater than the context length")
    {
        return true;
    }

    // Generic fallback patterns
    if lower.contains("context length")
        || lower.contains("context_length")
        || lower.contains("context limit")
        || lower.contains("maximum context")
        || lower.contains("token limit")
        || lower.contains("too many tokens")
        || (lower.contains("context window") && lower.contains("exceeded"))
        || (lower.contains("maximum") && lower.contains("tokens") && lower.contains("exceeded"))
    {
        return true;
    }

    // An output ceiling named in a rejection about size. Anthropic reports a
    // full window as "input length and `max_tokens` exceed context limit",
    // and a bare mention of the parameter is not on its own an overflow.
    if lower.contains("max_tokens") && (lower.contains("context") || lower.contains("exceed")) {
        return true;
    }

    // "too long" only where it is the request that is too long. A timeout
    // that reports something "took too long" is not an overflow, and acting
    // on it as one would compact a history that was never the problem.
    if lower.contains("too long")
        && [
            "prompt",
            "input",
            "request",
            "message",
            "conversation",
            "context",
        ]
        .iter()
        .any(|subject| lower.contains(subject))
        && !lower.contains("took too long")
        && !lower.contains("taking too long")
    {
        return true;
    }

    false
}

/// Check if an error message indicates context window overflow.
///
/// This function detects context overflow errors from multiple LLM providers
/// by checking for known error message patterns.
///
/// # Supported Providers
///
/// - **Anthropic**: "prompt is too long: X tokens > Y maximum"
/// - **`OpenAI`**: "exceeds the context window"
/// - **Google Gemini**: "input token count exceeds the maximum"
/// - **xAI (Grok)**: "maximum prompt length is X but request contains Y"
/// - **Groq**: "reduce the length of the messages"
/// - **Cerebras/Mistral**: 400/413 status code (no body)
/// - **`OpenRouter`**: "maximum context length is X tokens"
/// - **llama.cpp**: "exceeds the available context size"
/// - **LM Studio**: "greater than the context length"
///
/// # Returns
///
/// `true` if the message indicates a context overflow error.
pub fn is_context_overflow_error(message: &str) -> bool {
    if is_context_overflow_message(message) {
        return true;
    }
    let lower = message.to_lowercase();

    // Cerebras and Mistral return 400/413 with no body
    // Match patterns like "400 status code (no body)" or "413 (no body)"
    if (lower.contains("400") || lower.contains("413")) && lower.contains("(no body)") {
        return true;
    }

    false
}

/// Classify API error for retry logic
///
/// # Strategy
///
/// 1. Check for context overflow first (most important for agent loop)
/// 2. Check error code (most reliable)
/// 3. Fall back to error type
/// 4. Check message for keywords ("overloaded", "temporarily")
///
/// # Pattern: Early Returns
///
/// Uses pattern matching with early returns for clarity:
///
/// ```rust,ignore
/// match code {
///     Some("rate_limit_exceeded") => return ApiError::RateLimited { ... },
///     Some("invalid_api_key") => return ApiError::Fatal { ... },
///     _ => { /* continue checking */ }
/// }
/// ```
///
/// This is more readable than deeply nested if/else chains.
fn classify_error(error: &serde_json::Value) -> ApiError {
    let code = error.get("code").and_then(|c| c.as_str());
    let error_type = error.get("type").and_then(|t| t.as_str());
    let message = error
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("Unknown error")
        .to_string();

    // Check for context overflow first (may not have a specific error code)
    if is_context_overflow_error(&message) {
        return ApiError::ContextWindowExceeded;
    }

    match code {
        Some("context_length_exceeded") => ApiError::ContextWindowExceeded,
        Some("insufficient_quota") => ApiError::QuotaExceeded,
        Some("rate_limit_exceeded") => {
            // Try to parse retry-after from message
            let retry_after = parse_retry_after(&message);
            ApiError::RateLimited { retry_after }
        }
        // Fatal errors that should not be retried
        Some("invalid_api_key" | "model_not_found" | "invalid_request_error") => {
            ApiError::Fatal { message }
        }
        _ => {
            // Check error type for additional classification
            match error_type {
                Some("authentication_error" | "permission_error") => ApiError::Fatal { message },
                Some("server_error" | "service_unavailable") => ApiError::Retryable { message },
                _ => {
                    // Default: check if message suggests retryable
                    if message.contains("overloaded") || message.contains("temporarily") {
                        ApiError::Retryable { message }
                    } else {
                        ApiError::Fatal { message }
                    }
                }
            }
        }
    }
}

fn response_failed_error(
    response: Option<&serde_json::Value>,
) -> (ProviderStreamErrorKind, String) {
    let Some(error) = response.and_then(|response| response.get("error")) else {
        return (
            ProviderStreamErrorKind::ProviderDeclaredFailure,
            "openai_response_failed: Unknown error".to_string(),
        );
    };
    let classified = classify_error(error);
    let kind = match &classified {
        ApiError::RateLimited { .. } | ApiError::Retryable { .. } => {
            ProviderStreamErrorKind::TransientProtocol
        }
        ApiError::ContextWindowExceeded | ApiError::QuotaExceeded | ApiError::Fatal { .. } => {
            ProviderStreamErrorKind::ProviderDeclaredFailure
        }
    };
    let message = match &classified {
        ApiError::ContextWindowExceeded => "Context window exceeded - message too long".to_string(),
        ApiError::QuotaExceeded => "API quota exceeded - check your billing".to_string(),
        ApiError::RateLimited { retry_after } => {
            if let Some(delay) = retry_after {
                format!("Rate limited - retry after {delay:?}")
            } else {
                "Rate limited - please try again".to_string()
            }
        }
        ApiError::Retryable { message } | ApiError::Fatal { message } => message.clone(),
    };
    (kind, format!("openai_response_failed: {message}"))
}

/// Parse retry-after duration from error message
fn parse_retry_after(message: &str) -> Option<std::time::Duration> {
    // Pattern: "try again in X.XXs" or "try again in X seconds"
    let lower = message.to_lowercase();
    if let Some(pos) = lower.find("try again in") {
        let after = &lower[pos + 13..];
        // Try to parse number
        let num_str: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(num) = num_str.parse::<f64>() {
            // Check unit
            let rest = &after[num_str.len()..].trim_start();
            if rest.starts_with("ms") {
                return Some(std::time::Duration::from_millis(num as u64));
            }
            // Assume seconds
            return Some(std::time::Duration::from_secs_f64(num));
        }
    }
    None
}

/// Check if a tool schema has incompatible constructs for Responses API
///
/// The Responses API has stricter schema requirements than Chat Completions.
/// It doesn't support JSON Schema combinators at the top level:
///
/// - `oneOf`: Union types (A OR B)
/// - `anyOf`: Any of multiple schemas
/// - `allOf`: Intersection types (A AND B)
/// - `not`: Negation
/// - `enum`: Top-level enums
///
/// These are often used in complex tool schemas but must be avoided for
/// Responses API compatibility. Consider simplifying schemas or using
/// Chat Completions API instead.
///
/// # Pattern: Object Introspection
///
/// Uses `as_object()` to check if JSON value is an object, then checks
/// for specific keys:
///
/// ```rust,ignore
/// if let Some(obj) = schema.as_object() {
///     obj.contains_key("oneOf") || obj.contains_key("anyOf")
/// }
/// ```
fn has_incompatible_schema(schema: &serde_json::Value) -> bool {
    if let Some(obj) = schema.as_object() {
        obj.contains_key("oneOf")
            || obj.contains_key("anyOf")
            || obj.contains_key("allOf")
            || obj.contains_key("not")
            // Top-level enum is also problematic
            || obj.contains_key("enum")
    } else {
        false
    }
}

/// Filter tools to only include those compatible with Responses API
fn filter_responses_api_tools(tools: &[Tool]) -> Vec<Tool> {
    tools
        .iter()
        .filter(|tool| {
            // Tool must have a name
            if tool.name.trim().is_empty() {
                return false;
            }
            // Check schema compatibility
            !has_incompatible_schema(&tool.input_schema)
        })
        .cloned()
        .collect()
}

/// Extract text from a `ResponseItem` (Message type with `output_text` content)
fn extract_text_from_item(item: &serde_json::Value) -> Option<String> {
    let item_type = item.get("type")?.as_str()?;
    if item_type != "message" {
        return None;
    }

    let content = item.get("content")?.as_array()?;
    let mut text = String::new();
    for part in content {
        if let Some(part_type) = part.get("type").and_then(|v| v.as_str()) {
            if part_type == "output_text" {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }

    if text.is_empty() { None } else { Some(text) }
}

fn strip_managed_model_prefix(model: &str) -> &str {
    for prefix in ["evalops/", "maestro-managed/"] {
        if let Some(candidate) = model.get(..prefix.len()) {
            if candidate.eq_ignore_ascii_case(prefix) {
                return &model[prefix.len()..];
            }
        }
    }
    model
}

fn strip_provider_model_prefix<'a>(model: &'a str, provider: &str) -> &'a str {
    let Some((prefix, model_id)) = model.split_once('/') else {
        return model;
    };
    if prefix.eq_ignore_ascii_case(provider) && !model_id.trim().is_empty() {
        model_id.trim()
    } else {
        model
    }
}

fn uses_responses_api(provider: Option<&str>, model: &str) -> bool {
    crate::openai_request_capabilities(provider, model).protocol
        == crate::OpenAiWireProtocol::OpenAiResponses
}

/// Check if this is a Mistral model (requires special tool ID handling)
///
/// Mistral API has specific requirements:
/// - Tool call IDs must be exactly 9 alphanumeric characters
/// - Tool results must include the `name` field
fn is_mistral_model(model: &str, base_url: Option<&str>) -> bool {
    let model_lower = model.to_lowercase();
    if model_lower.contains("mistral")
        || model_lower.contains("mixtral")
        || model_lower.contains("codestral")
        || model_lower.contains("pixtral")
    {
        return true;
    }
    if let Some(url) = base_url {
        if url.contains("mistral.ai") {
            return true;
        }
    }
    false
}

/// Check if this is a Groq-hosted model
///
/// Groq provides fast inference using their LPU (Language Processing Unit).
/// They host models like Llama, Mixtral, Gemma, `DeepSeek`, and Qwen.
/// Groq uses OpenAI-compatible API format with no special handling required.
fn is_groq_model(model: &str, base_url: Option<&str>) -> bool {
    let model_lower = model.to_lowercase();
    // Explicit groq/ prefix
    if model_lower.starts_with("groq/") {
        return true;
    }
    // A custom base URL pins the provider: only Groq's host is Groq. This keeps
    // Groq-specific request shaping (e.g. omitting parallel_tool_calls) off the
    // direct DeepSeek/DashScope/etc. clients, whose ids also contain "deepseek"
    // or "qwen".
    if let Some(url) = base_url {
        return url.contains("groq.com");
    }
    // No custom base URL: fall back to model-name heuristics for the open models
    // commonly hosted on Groq (Llama, and DeepSeek/Qwen distill/coder variants).
    model_lower.starts_with("llama-")
        || model_lower.starts_with("llama3")
        || model_lower.contains("deepseek")
        || model_lower.contains("qwen")
}

/// Normalize a tool call ID for Mistral compatibility.
///
/// Mistral requires tool IDs to be exactly 9 alphanumeric characters.
/// This function:
/// - Removes non-alphanumeric characters
/// - Pads with zeros if too short
/// - Truncates if too long
fn normalize_mistral_tool_id(id: &str) -> String {
    // Remove non-alphanumeric characters
    let normalized: String = id.chars().filter(|c| c.is_alphanumeric()).collect();

    // Ensure exactly 9 characters
    if normalized.len() < 9 {
        format!("{normalized:0<9}")
    } else if normalized.len() > 9 {
        normalized[..9].to_string()
    } else {
        normalized
    }
}

/// Get the appropriate API URL for the model
fn api_url_for_model(model: &str) -> &'static str {
    if uses_responses_api(None, model) {
        "https://api.openai.com/v1/responses"
    } else {
        "https://api.openai.com/v1/chat/completions"
    }
}

/// `OpenAI` API client
///
/// Handles communication with `OpenAI`'s APIs (Chat Completions and Responses).
/// Automatically selects the appropriate API endpoint based on model name.
/// Also supports Mistral API through OpenAI-compatible endpoint with special
/// tool ID handling.
///
/// # Thread Safety
///
/// Like `AnthropicClient`, this implements `Send + Sync` for safe concurrent use.
/// The `reqwest::Client` is internally synchronized and benefits from connection
/// pooling when reused across requests.
#[derive(Clone)]
pub struct OpenAiClient {
    /// Reusable HTTP client with connection pooling
    client: reqwest::Client,
    /// API key for authentication (via Authorization: Bearer header)
    api_key: String,
    /// Optional base URL override (for Mistral or other OpenAI-compatible APIs)
    base_url: Option<String>,
    extra_headers: HeaderMap,
    request_extensions: serde_json::Map<String, serde_json::Value>,
    managed_gateway: bool,
    managed_organization_id: Option<String>,
    managed_workspace_id: Option<String>,
    managed_request_lineage: Option<ManagedRequestLineage>,
    managed_inference_authorization: Option<String>,
    // Clones share consumption state across rounds, retries, and auxiliary calls.
    managed_authorization_used: std::sync::Arc<std::sync::atomic::AtomicBool>,
    managed_authorization_provider:
        Option<std::sync::Arc<dyn crate::managed_authorization::ManagedAuthorizationProvider>>,
    route_provider: Option<String>,
    #[cfg(test)]
    response_open_timeout_override: Option<std::time::Duration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedRequestLineage {
    pub(crate) lineage_id: String,
}

impl OpenAiClient {
    /// Create a new `OpenAI` client
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_mins(5))
            .build()
            .context("Failed to create HTTP client")?;
        let api_key = api_key.into().trim().to_string();

        Ok(Self {
            client,
            api_key,
            base_url: None,
            extra_headers: HeaderMap::new(),
            request_extensions: serde_json::Map::new(),
            managed_gateway: false,
            managed_organization_id: None,
            managed_workspace_id: None,
            managed_request_lineage: None,
            managed_inference_authorization: None,
            managed_authorization_used: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            managed_authorization_provider: None,
            route_provider: None,
            #[cfg(test)]
            response_open_timeout_override: None,
        })
    }

    /// Customer-controlled route: redirects cannot escape the approved endpoint.
    pub fn with_disconnected_endpoint(
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
        provider: &str,
    ) -> Result<Self> {
        let mut client = Self::with_base_url(api_key, endpoint)?.with_route_provider(provider);
        client.client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_mins(5))
            .build()
            .context("Failed to create disconnected model client")?;
        Ok(client)
    }

    /// Create a new client with a custom base URL (for Mistral or other providers)
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_mins(5))
            .build()
            .context("Failed to create HTTP client")?;
        let api_key = api_key.into().trim().to_string();

        Ok(Self {
            client,
            api_key,
            base_url: Some(base_url.into()),
            extra_headers: HeaderMap::new(),
            request_extensions: serde_json::Map::new(),
            managed_gateway: false,
            managed_organization_id: None,
            managed_workspace_id: None,
            managed_request_lineage: None,
            managed_inference_authorization: None,
            managed_authorization_used: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            managed_authorization_provider: None,
            route_provider: None,
            #[cfg(test)]
            response_open_timeout_override: None,
        })
    }

    pub(crate) fn with_route_provider(mut self, provider: &str) -> Self {
        self.route_provider = Some(provider.trim().to_string());
        self
    }

    pub(crate) fn routed_provider(&self) -> Option<&str> {
        self.route_provider.as_deref()
    }

    pub(crate) fn is_managed_gateway(&self) -> bool {
        self.managed_gateway
    }

    pub(crate) fn managed_gateway_scope(&self) -> Option<(&str, &str)> {
        if !self.managed_gateway {
            return None;
        }
        self.managed_organization_id
            .as_deref()
            .zip(self.managed_workspace_id.as_deref())
    }

    pub(crate) fn cache_scope(
        &self,
    ) -> Result<Option<maestro_runtime_contracts::cache_topology::CacheScope>> {
        let Some((organization_id, workspace_id)) = self.managed_gateway_scope() else {
            return Ok(None);
        };
        let Some(encoded) = self.managed_inference_authorization.as_deref() else {
            return Ok(None);
        };
        let (authorization, _) = parse_managed_inference_authorization(encoded)?;
        let context = managed_inference_context(&authorization)?;
        let session_id = context
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|session| !session.trim().is_empty())
            .context("managed cache topology requires an admitted session")?
            .to_owned();
        Ok(Some(
            maestro_runtime_contracts::cache_topology::CacheScope {
                organization_id: organization_id.into(),
                workspace_id: workspace_id.into(),
                session_id,
            },
        ))
    }

    pub(crate) fn cache_namespace(&self) -> Result<String> {
        if let Some(scope) = self.cache_scope()? {
            return Ok(scope.namespace());
        }
        // Older managed callers expose tenant scope but no session authority.
        // Keep tenant separation for local diagnostics without inventing a session.
        Ok(self.managed_gateway_scope().map_or_else(
            || "local".into(),
            |scope| {
                maestro_runtime_contracts::cache_topology::digest(&("unattested-managed.v1", scope))
            },
        ))
    }

    pub(crate) fn set_managed_request_lineage(&mut self, lineage_id: Option<String>) {
        self.managed_request_lineage =
            lineage_id.map(|lineage_id| ManagedRequestLineage { lineage_id });
    }

    pub(crate) fn set_managed_inference_authorization(&mut self, authorization: Option<String>) {
        self.managed_inference_authorization = authorization;
        self.managed_authorization_used =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    }

    pub(crate) fn set_managed_authorization_provider(
        &mut self,
        provider: std::sync::Arc<dyn crate::managed_authorization::ManagedAuthorizationProvider>,
    ) {
        self.managed_authorization_provider = Some(provider);
    }

    fn response_open_timeout(&self) -> Option<std::time::Duration> {
        #[cfg(test)]
        if let Some(timeout) = self.response_open_timeout_override {
            return Some(timeout);
        }
        self.managed_gateway
            .then_some(MANAGED_GATEWAY_RESPONSE_OPEN_TIMEOUT)
    }

    #[cfg(test)]
    pub(crate) fn with_response_open_timeout_for_test(
        mut self,
        timeout: std::time::Duration,
    ) -> Self {
        self.response_open_timeout_override = Some(timeout);
        self
    }

    pub(crate) fn with_managed_gateway_context(
        mut self,
        organization_id: &str,
        workspace_id: &str,
        provider_ref: serde_json::Value,
    ) -> Result<Self> {
        let organization_id = organization_id.trim();
        let workspace_id = workspace_id.trim();
        if organization_id.is_empty() || workspace_id.is_empty() {
            return Err(anyhow::anyhow!(
                "managed gateway organization and workspace scope are required"
            ));
        }
        self.managed_gateway = true;
        self.managed_organization_id = Some(organization_id.to_owned());
        self.extra_headers.insert(
            HeaderName::from_static("x-organization-id"),
            HeaderValue::from_str(organization_id).context("invalid EvalOps organization id")?,
        );
        self.extra_headers.insert(
            HeaderName::from_static("x-workspace-id"),
            HeaderValue::from_str(workspace_id).context("invalid EvalOps workspace id")?,
        );
        self.request_extensions.insert(
            "organization_id".to_string(),
            serde_json::json!(organization_id),
        );
        self.request_extensions
            .insert("workspace_id".to_string(), serde_json::json!(workspace_id));
        self.request_extensions
            .insert("provider_ref".to_string(), provider_ref);
        self.managed_workspace_id = Some(workspace_id.to_owned());
        Ok(self)
    }

    /// Attach the tenant scope used by a hosted Platform turn. The managed
    /// provider reference and both tenant headers must travel together so the
    /// gateway cannot resolve a model for one workspace while authorizing
    /// another.
    pub(crate) fn with_managed_gateway_scope(
        self,
        organization_id: &str,
        workspace_id: &str,
        provider_ref: serde_json::Value,
    ) -> Result<Self> {
        self.with_managed_gateway_context(organization_id, workspace_id, provider_ref)
    }

    /// Create a new client from environment variable
    pub fn from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["OPENAI_API_KEY"])?;
        Self::new(api_key)
    }

    /// Create a new Mistral client from environment variable
    pub fn mistral_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["MISTRAL_API_KEY"])?;
        Self::with_base_url(api_key, "https://api.mistral.ai/v1")
    }

    /// Create a new Groq client from environment variable
    ///
    /// Groq provides fast inference for open models like Llama, Mixtral, and Gemma.
    /// Uses OpenAI-compatible API format.
    pub fn groq_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["GROQ_API_KEY"])?;
        Self::with_base_url(api_key, "https://api.groq.com/openai/v1")
    }

    /// Create a new DeepSeek client from environment variable.
    ///
    /// DeepSeek exposes an OpenAI-compatible API at `https://api.deepseek.com/v1`.
    pub fn deepseek_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["DEEPSEEK_API_KEY"])?;
        Self::with_base_url(api_key, "https://api.deepseek.com/v1")
    }

    /// Create a new Moonshot (Kimi) client from environment variable.
    ///
    /// Uses the international endpoint `https://api.moonshot.ai/v1`. Accepts
    /// `MOONSHOT_API_KEY` or, as a fallback, `KIMI_API_KEY`.
    pub fn moonshot_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["MOONSHOT_API_KEY", "KIMI_API_KEY"])?;
        Self::with_base_url(api_key, "https://api.moonshot.ai/v1")
    }

    /// Create a new Alibaba Qwen (DashScope) client from environment variable.
    ///
    /// Uses the international compatible-mode endpoint. Accepts
    /// `DASHSCOPE_API_KEY` or, as a fallback, `QWEN_API_KEY`.
    pub fn qwen_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["DASHSCOPE_API_KEY", "QWEN_API_KEY"])?;
        Self::with_base_url(
            api_key,
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        )
    }

    /// Create a new MiniMax client from environment variable.
    ///
    /// Uses the international endpoint `https://api.minimax.io/v1`.
    pub fn minimax_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["MINIMAX_API_KEY"])?;
        Self::with_base_url(api_key, "https://api.minimax.io/v1")
    }

    /// Create a new Z.ai / Zhipu GLM client from environment variable.
    ///
    /// Uses the international endpoint `https://api.z.ai/api/coding/paas/v4`.
    pub fn zai_from_env() -> Result<Self> {
        let api_key = op_secret::env_credential(&["ZAI_API_KEY"])?;
        Self::with_base_url(api_key, "https://api.z.ai/api/coding/paas/v4")
    }

    /// Build request headers
    pub(crate) fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if !self.api_key.is_empty() {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        headers.extend(self.extra_headers.clone());
        headers
    }

    fn managed_request(&self, mut body: serde_json::Value) -> Result<serde_json::Value> {
        if !self.managed_gateway {
            return Ok(body);
        }

        let authorization = self
            .managed_inference_authorization
            .as_deref()
            .map(parse_managed_inference_authorization)
            .transpose()?;
        let managed_context = authorization
            .as_ref()
            .map(|(authorization, _)| managed_inference_context(authorization))
            .transpose()?;
        let explicit_lineage_id = self
            .managed_request_lineage
            .as_ref()
            .map(|lineage| lineage.lineage_id.as_str())
            .filter(|lineage_id| !lineage_id.trim().is_empty())
            .map(str::to_string);
        if let (Some(explicit_lineage_id), Some((_, authorized_lineage_id))) =
            (explicit_lineage_id.as_deref(), authorization.as_ref())
        {
            if explicit_lineage_id != authorized_lineage_id.as_str() {
                anyhow::bail!(
                    "managed request lineage does not match managed inference authorization lineage"
                );
            }
        }
        let lineage_id = explicit_lineage_id.or_else(|| {
            authorization
                .as_ref()
                .map(|(_, lineage_id)| lineage_id.clone())
        });
        if authorization.is_none() && lineage_id.is_none() {
            return Ok(body);
        }

        let object = body
            .as_object_mut()
            .context("managed gateway request body must be an object")?;
        if let Some((authorization, _)) = authorization {
            project_managed_inference_route(object, &authorization)?;
            // Provider history follows the signed route, not the legacy
            // constructor's provider. Keep native Gemini context only when
            // the admitted route can replay it; it is never tool authority.
            let gemini_route = object
                .get("provider_candidates")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|candidates| {
                    candidates.iter().any(|candidate| {
                        supports_gemini_tool_history(
                            candidate
                                .pointer("/provider_ref/provider")
                                .and_then(serde_json::Value::as_str),
                        )
                    })
                });
            if !gemini_route {
                if let Some(input) = object
                    .get_mut("input")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for item in input {
                        if item.get("type").and_then(serde_json::Value::as_str)
                            == Some("function_call")
                        {
                            if let Some(item) = item.as_object_mut() {
                                item.remove("extra_content");
                            }
                        }
                    }
                }
            }
            object.insert("managed_inference_authorization".to_string(), authorization);
        }
        if let Some(context) = managed_context {
            object.insert("managed_inference_context".to_string(), context);
        }
        if let Some(lineage_id) = lineage_id {
            object.insert(
                "lineage_id".to_string(),
                serde_json::Value::String(lineage_id),
            );
        }
        Ok(body)
    }

    /// Convert internal messages to `OpenAI` format
    ///
    /// When `is_mistral` is true, applies Mistral-specific transformations:
    /// - Tool call IDs are normalized to 9 alphanumeric characters
    /// - Tool results include the `name` field (required by Mistral)
    fn convert_messages(
        &self,
        messages: &[Message],
        is_mistral: bool,
        tool_id_to_name: &std::collections::HashMap<String, String>,
    ) -> Vec<OpenAiMessage> {
        let mut converted = Vec::new();
        for msg in messages {
            if let MessageContent::Blocks(blocks) = &msg.content {
                let tool_results: Vec<_> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => Some((tool_use_id, content)),
                        _ => None,
                    })
                    .collect();
                if !tool_results.is_empty() {
                    for (tool_use_id, content) in tool_results {
                        let normalized_id = if is_mistral {
                            normalize_mistral_tool_id(tool_use_id)
                        } else {
                            tool_use_id.clone()
                        };
                        converted.push(OpenAiMessage {
                            role: "tool".to_string(),
                            content: Some(OpenAiContent::Text(content.clone())),
                            tool_calls: None,
                            tool_call_id: Some(normalized_id),
                            name: is_mistral
                                .then(|| tool_id_to_name.get(tool_use_id).cloned())
                                .flatten(),
                        });
                    }
                    continue;
                }
            }
            if let Some(message) = self.convert_message(msg, is_mistral, tool_id_to_name) {
                converted.push(message);
            }
        }
        converted
    }

    /// Managed gateway requests use the exact Platform model key. Direct
    /// provider clients still receive the provider-native model id expected by
    /// their upstream API.
    fn request_model_name(&self, model: &str) -> String {
        let model = model.trim();
        if self.managed_gateway {
            // The managed gateway resolves the provider-native id itself.
            // Remove only Maestro's routing namespace; OpenRouter model ids
            // are opaque and may themselves begin with `openrouter/`.
            strip_managed_model_prefix(model).to_string()
        } else if self
            .route_provider
            .as_deref()
            .is_some_and(|provider| provider.eq_ignore_ascii_case("openrouter"))
        {
            // OpenRouter's native id is often `<vendor>/<model>`. Strip only
            // the optional outer routing prefix; a second pass must not turn
            // `anthropic/claude-...` into `claude-...`.
            strip_provider_model_prefix(model, "openrouter").to_string()
        } else {
            provider_model_name(model)
        }
    }

    fn authorized_responses_api_for(&self, model: &str) -> Result<bool> {
        let Some(encoded) = self.managed_inference_authorization.as_deref() else {
            return Ok(self.uses_responses_api_for(model));
        };
        anyhow::ensure!(
            self.managed_gateway,
            "managed inference authorization requires a managed Gateway"
        );
        let (authorization, _) = parse_managed_inference_authorization(encoded)?;
        let claims = authorization.get("claims").unwrap_or(&authorization);
        // Select the request shape from the authorization without modifying it.
        // Gateway remains responsible for signature and exact endpoint verification.
        match claims.get("endpoint").and_then(serde_json::Value::as_str) {
            Some("responses") => Ok(true),
            Some("chat.completions") => Ok(false),
            _ => anyhow::bail!(
                "managed inference authorization has an unsupported or missing endpoint"
            ),
        }
    }

    fn uses_responses_api_for(&self, model: &str) -> bool {
        uses_responses_api(self.route_provider.as_deref(), model)
    }

    fn convert_message(
        &self,
        msg: &Message,
        is_mistral: bool,
        tool_id_to_name: &std::collections::HashMap<String, String>,
    ) -> Option<OpenAiMessage> {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };

        // Convert content
        match &msg.content {
            MessageContent::Text(text) => Some(OpenAiMessage {
                role: role.to_string(),
                content: Some(OpenAiContent::Text(text.clone())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }),
            MessageContent::Blocks(blocks) => {
                // Handle tool results specially
                if let Some(ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                }) = blocks.first()
                {
                    let normalized_id = if is_mistral {
                        normalize_mistral_tool_id(tool_use_id)
                    } else {
                        tool_use_id.clone()
                    };

                    // Mistral requires the tool name in tool results
                    let name = if is_mistral {
                        tool_id_to_name.get(tool_use_id).cloned()
                    } else {
                        None
                    };

                    return Some(OpenAiMessage {
                        role: "tool".to_string(),
                        content: Some(OpenAiContent::Text(content.clone())),
                        tool_calls: None,
                        tool_call_id: Some(normalized_id),
                        name,
                    });
                }

                // Chat Completions requires an assistant `tool_calls`
                // array before any subsequent `role: tool` result.
                // Serializing tool uses as text leaves tool results
                // orphaned and is rejected by OpenAI.
                if msg.role == Role::Assistant {
                    let tool_calls: Vec<OpenAiToolCall> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolUse {
                                id, name, input, ..
                            } => Some(OpenAiToolCall {
                                index: None,
                                id: Some(id.clone()),
                                tool_type: Some("function".to_string()),
                                function: Some(OpenAiFunctionCall {
                                    name: Some(name.clone()),
                                    arguments: Some(
                                        serde_json::to_string(input)
                                            .unwrap_or_else(|_| "{}".to_string()),
                                    ),
                                }),
                            }),
                            _ => None,
                        })
                        .collect();
                    if !tool_calls.is_empty() {
                        let text = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<String>();
                        return Some(OpenAiMessage {
                            role: "assistant".to_string(),
                            content: (!text.is_empty()).then_some(OpenAiContent::Text(text)),
                            tool_calls: Some(tool_calls),
                            tool_call_id: None,
                            name: None,
                        });
                    }
                }

                // Convert blocks to OpenAI content parts
                let parts: Vec<OpenAiContentPart> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => {
                            Some(OpenAiContentPart::Text { text: text.clone() })
                        }
                        ContentBlock::Image { source } => match source {
                            ImageSource::Url { url } => Some(OpenAiContentPart::ImageUrl {
                                image_url: ImageUrlData {
                                    url: url.clone(),
                                    detail: None,
                                },
                            }),
                            ImageSource::Base64 { media_type, data } => {
                                Some(OpenAiContentPart::ImageUrl {
                                    image_url: ImageUrlData {
                                        url: format!("data:{media_type};base64,{data}"),
                                        detail: None,
                                    },
                                })
                            }
                        },
                        ContentBlock::ToolUse { .. } => None,
                        _ => None,
                    })
                    .collect();

                if parts.is_empty() {
                    None
                } else if parts.len() == 1 {
                    if let OpenAiContentPart::Text { text } = &parts[0] {
                        Some(OpenAiMessage {
                            role: role.to_string(),
                            content: Some(OpenAiContent::Text(text.clone())),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        })
                    } else {
                        Some(OpenAiMessage {
                            role: role.to_string(),
                            content: Some(OpenAiContent::Parts(parts)),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        })
                    }
                } else {
                    Some(OpenAiMessage {
                        role: role.to_string(),
                        content: Some(OpenAiContent::Parts(parts)),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    })
                }
            }
        }
    }

    /// Build a mapping from tool call IDs to tool names from the message history.
    /// This is needed for Mistral which requires the tool name in tool results.
    fn build_tool_id_to_name_map(
        messages: &[Message],
    ) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for msg in messages {
            if let MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        map.insert(id.clone(), name.clone());
                    }
                }
            }
        }
        map
    }

    /// Convert internal tools to `OpenAI` format
    fn convert_tools(&self, tools: &[Tool]) -> Vec<OpenAiTool> {
        tools
            .iter()
            .map(|tool| OpenAiTool {
                tool_type: "function".to_string(),
                function: OpenAiFunction {
                    name: tool.name.clone(),
                    description: Some(tool.description.clone()),
                    parameters: Some(tool.input_schema.clone()),
                },
            })
            .collect()
    }

    /// Build the request body for Chat Completions API
    fn build_chat_request_body(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> serde_json::Value {
        let model = self.request_model_name(&config.model);
        // Check if this is a Mistral model (needs special tool handling)
        let is_mistral = is_mistral_model(&model, self.base_url.as_deref());
        // Check if this is a Groq model (may need parameter adjustments)
        let is_groq = is_groq_model(&model, self.base_url.as_deref());

        let transformed_messages =
            transform_messages_for_target(messages, OutboundTarget::OpenAiChat);

        // Build tool ID to name mapping for Mistral
        let tool_id_to_name = if is_mistral {
            Self::build_tool_id_to_name_map(&transformed_messages)
        } else {
            std::collections::HashMap::new()
        };

        let openai_messages =
            self.convert_messages(&transformed_messages, is_mistral, &tool_id_to_name);

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": crate::openai_request_capabilities(self.route_provider.as_deref(), &config.model).output_tokens.map_or(config.max_tokens, |limit| config.max_tokens.min(limit)),
            "messages": openai_messages,
            "stream": true,
            "stream_options": {
                "include_usage": true
            },
            // Nudge model to actually choose a tool when tools are present
            "tool_choice": if config.tools.is_empty() { serde_json::json!("none") } else { serde_json::json!("auto") }
        });

        // Only add parallel_tool_calls for providers that support it
        // Groq doesn't support this parameter, Mistral ignores it
        if !is_groq {
            body["parallel_tool_calls"] = serde_json::json!(true);
        }

        // Add system message if provided
        if let Some(system) = &config.system {
            // Prepend system message
            if let Some(msgs) = body["messages"].as_array_mut() {
                msgs.insert(
                    0,
                    serde_json::json!({
                        "role": "system",
                        "content": system
                    }),
                );
            }
        }

        if let Some(temp) = config.temperature.filter(|_| {
            crate::openai_request_capabilities(self.route_provider.as_deref(), &config.model)
                .temperature
        }) {
            body["temperature"] = serde_json::json!(temp);
        }

        if !config.tools.is_empty() {
            body["tools"] = serde_json::json!(self.convert_tools(&config.tools));
        }

        // llama.cpp supports request-level prompt caching. Keeping this
        // provider-scoped avoids sending an extension field to hosted APIs.
        if self.route_provider.as_deref() == Some("llamacpp") {
            body["cache_prompt"] = serde_json::json!(true);
        }

        // GPT-5.1 supports reasoning_effort for adaptive thinking
        if let Some(thinking) = &config.thinking {
            // Map thinking budget to reasoning effort
            let capabilities =
                crate::openai_request_capabilities(self.route_provider.as_deref(), &config.model);
            let effort = capabilities.reasoning_effort(thinking.budget_tokens);
            body["reasoning_effort"] = serde_json::json!(effort);
        }

        body
    }

    /// Build the request body for Responses API (gpt-5.1-codex-* models)
    fn build_responses_request_body(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<serde_json::Value> {
        let model = self.request_model_name(&config.model);
        let messages = transform_messages_for_target(messages, OutboundTarget::OpenAiResponses);
        // Convert messages to Responses API format
        // The input array contains ResponseItems, which can be messages, function calls, or function outputs
        let mut input: Vec<serde_json::Value> = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => continue, // System goes in instructions
                Role::User => {
                    // User messages use "input_text" content type
                    match &msg.content {
                        MessageContent::Text(text) => {
                            input.push(serde_json::json!({
                                "type": "message",
                                "role": "user",
                                "content": [{
                                    "type": "input_text",
                                    "text": text
                                }]
                            }));
                        }
                        MessageContent::Blocks(blocks) => {
                            // Check if this is tool results (they go as separate items, not in a message)
                            let mut has_tool_results = false;
                            for block in blocks {
                                if let ContentBlock::ToolResult {
                                    tool_use_id,
                                    content,
                                    is_error: _,
                                } = block
                                {
                                    has_tool_results = true;
                                    // The Responses API expects output as a plain string for success
                                    // Format: { type: "function_call_output", call_id: "...", output: "..." }
                                    input.push(serde_json::json!({
                                        "type": "function_call_output",
                                        "call_id": responses_call_id(tool_use_id),
                                        "output": content
                                    }));
                                }
                            }

                            // If not tool results, treat as regular user message
                            if !has_tool_results {
                                let content: Vec<serde_json::Value> = blocks
                                    .iter()
                                    .filter_map(|block| match block {
                                        ContentBlock::Text { text } => Some(serde_json::json!({
                                            "type": "input_text",
                                            "text": text
                                        })),
                                        _ => None,
                                    })
                                    .collect();

                                if !content.is_empty() {
                                    input.push(serde_json::json!({
                                        "type": "message",
                                        "role": "user",
                                        "content": content
                                    }));
                                }
                            }
                        }
                    }
                }
                Role::Assistant => {
                    // Assistant messages use "output_text" content type
                    // Tool calls go as separate "function_call" items
                    match &msg.content {
                        MessageContent::Text(text) => {
                            input.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": text
                                }]
                            }));
                        }
                        MessageContent::Blocks(blocks) => {
                            // First, collect any text content into a message
                            let text_content: Vec<serde_json::Value> = blocks
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::Text { text } => Some(serde_json::json!({
                                        "type": "output_text",
                                        "text": text
                                    })),
                                    _ => None,
                                })
                                .collect();

                            if !text_content.is_empty() {
                                input.push(serde_json::json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": text_content
                                }));
                            }

                            // Then, add tool calls as separate items
                            for block in blocks {
                                if let ContentBlock::ToolUse {
                                    id,
                                    name,
                                    input: args,
                                    gemini_context,
                                } = block
                                {
                                    let mut function_call = serde_json::json!({
                                        "type": "function_call",
                                        "call_id": responses_call_id(id),
                                        "name": name,
                                        "arguments": serde_json::to_string(args).unwrap_or_default()
                                    });
                                    if let Some(item_id) = responses_item_id(id) {
                                        function_call["id"] = serde_json::json!(item_id);
                                    }
                                    if self.managed_gateway
                                        && (self.managed_inference_authorization.is_some()
                                            || supports_gemini_tool_history(
                                                self.request_extensions
                                                    .get("provider_ref")
                                                    .and_then(|provider| provider.get("provider"))
                                                    .and_then(serde_json::Value::as_str),
                                            ))
                                    {
                                        if let Some(context) = gemini_context {
                                            function_call["extra_content"] =
                                                serde_json::json!({"google": context});
                                        }
                                    }
                                    input.push(function_call);
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "model": model,
            "input": input,
            "max_output_tokens": crate::openai_request_capabilities(self.route_provider.as_deref(), &config.model).output_tokens.map_or(config.max_tokens, |limit| config.max_tokens.min(limit)),
            "stream": true,
            "parallel_tool_calls": true,
            // Tell the model tools are available and should be used when appropriate
            "tool_choice": if config.tools.is_empty() { serde_json::json!("none") } else { serde_json::json!("auto") },
        });

        // Add instructions (system prompt)
        if let Some(system) = &config.system {
            body["instructions"] = serde_json::json!(system);
        }

        // Add tools (filtered for Responses API compatibility)
        if !config.tools.is_empty() {
            for tool in config
                .tools
                .iter()
                .filter(|tool| !tool.name.trim().is_empty())
            {
                if tool.schema_enforcement == ToolSchemaEnforcement::Require {
                    crate::constrained_sampling::resolve_strict_json_schema(tool, true)?;
                }
            }
            let compatible_tools = filter_responses_api_tools(&config.tools);
            if !compatible_tools.is_empty() {
                let tools: Vec<serde_json::Value> = compatible_tools
                    .iter()
                    .map(|tool| {
                        let strict_schema =
                            crate::constrained_sampling::resolve_strict_json_schema(tool, true)?;
                        let strict = strict_schema.is_some();
                        let parameters = strict_schema.unwrap_or_else(|| tool.input_schema.clone());
                        Ok(serde_json::json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "strict": strict,
                            "parameters": parameters
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?;
                body["tools"] = serde_json::json!(tools);
            }
        }

        // Add reasoning configuration
        // Codex models do reasoning by default, we need to include the content to see it
        if let Some(thinking) = &config.thinking {
            let capabilities =
                crate::openai_request_capabilities(self.route_provider.as_deref(), &config.model);
            let effort = capabilities.reasoning_effort(thinking.budget_tokens);
            body["reasoning"] = serde_json::json!({
                "effort": effort,
                "summary": "auto"  // Request reasoning summaries
            });
        }

        // Always include reasoning content for visibility
        // This enables streaming of reasoning text (only encrypted_content is valid)
        body["include"] = serde_json::json!(["reasoning.encrypted_content"]);

        Ok(body)
    }

    /// Build the appropriate request body based on model
    #[cfg(test)]
    fn build_request_body(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> serde_json::Value {
        self.build_request_body_for_api(
            messages,
            config,
            self.uses_responses_api_for(&config.model),
        )
        .expect("test request configuration must be valid")
    }

    pub(crate) fn build_request_body_for_api(
        &self,
        messages: &[Message],
        config: &RequestConfig,
        responses: bool,
    ) -> Result<serde_json::Value> {
        let mut body = if responses {
            self.build_responses_request_body(messages, config)?
        } else {
            self.build_chat_request_body(messages, config)
        };
        if let Some(prepared) = &config.cache_topology {
            // The admitted session selects a fixed one-hour policy. Auxiliary
            // summaries never acquire breakpoints or affinity.
            if prepared.topology().transition != crate::cache_topology::CacheTransition::Auxiliary
                && self.route_provider.as_deref() == Some("openrouter")
                && config.model.contains("claude")
                && !responses
            {
                let ttl = if self.managed_gateway && self.managed_inference_authorization.is_some()
                {
                    "1h"
                } else {
                    "5m"
                };
                // Chat completions split tool results into their own messages,
                // so canonical history indexes do not map onto the wire; only
                // the newest boundary is marked here.
                crate::cache_topology::mark_stable_history(&mut body, ttl, None);
            }
            prepared.append_volatile_tail(&mut body);
        }
        let legacy_affinity = if config.cache_topology.is_none() && !self.managed_gateway {
            std::env::var("MAESTRO_OPENROUTER_PROMPT_CACHE_KEY").ok()
        } else {
            None
        };
        let affinity = config
            .cache_topology
            .as_ref()
            .and_then(|prepared| prepared.affinity())
            .or(legacy_affinity.as_deref());
        // Hosted affinity is derived by the gateway from admitted scope and the resolved route.
        apply_prompt_cache_key(
            &mut body,
            self.route_provider.as_deref(),
            if self.managed_gateway { None } else { affinity },
        );
        if let Some(object) = body.as_object_mut() {
            object.extend(self.request_extensions.clone());
        }
        Ok(body)
    }

    /// Resolve the full request URL for a model.
    ///
    /// When a custom `base_url` is configured (Mistral, Groq, DeepSeek, Moonshot,
    /// DashScope, MiniMax, Z.ai, and other OpenAI-compatible providers), the
    /// request must target that provider's endpoint rather than the hardcoded
    /// OpenAI host. We append the OpenAI-compatible path (`/chat/completions` or
    /// `/responses`) to the configured base. Without a custom base, fall back to
    /// the OpenAI defaults.
    #[cfg(test)]
    fn request_url(&self, model: &str) -> String {
        self.request_url_for_api(model, self.uses_responses_api_for(model))
    }

    fn request_url_for_api(&self, model: &str, responses: bool) -> String {
        match &self.base_url {
            Some(base) => {
                let trimmed = base.trim_end_matches('/');
                if responses {
                    format!("{trimmed}/responses")
                } else {
                    format!("{trimmed}/chat/completions")
                }
            }
            None if self.managed_inference_authorization.is_none() => {
                api_url_for_model(model).to_string()
            }
            None => if responses {
                "https://api.openai.com/v1/responses"
            } else {
                "https://api.openai.com/v1/chat/completions"
            }
            .to_string(),
        }
    }
}

impl OpenAiClient {
    pub(crate) async fn stream_with_producer(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<CancellableStream> {
        if self.managed_gateway && self.managed_inference_authorization.is_some() {
            let mut invocation = self.clone();
            if self
                .managed_authorization_used
                .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                let provider = self
                    .managed_authorization_provider
                    .as_ref()
                    .context("managed inference authorization renewal is unavailable")?;
                let authorization = provider.renew().await?;
                authorization.validate().map_err(anyhow::Error::msg)?;
                invocation.managed_inference_authorization = Some(authorization.into_inner());
            }
            return invocation
                .stream_authorized_invocation(messages, config)
                .await;
        }
        self.stream_authorized_invocation(messages, config).await
    }

    async fn stream_authorized_invocation(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<CancellableStream> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Build request body
        crate::cache_topology::validate_prepared(messages, config)?;
        if let Some(prepared) = &config.cache_topology {
            prepared.validate_namespace(&self.cache_namespace()?)?;
        }
        if config.cache_topology.is_some()
            && self.request_extensions.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "model"
                        | "messages"
                        | "input"
                        | "system"
                        | "instructions"
                        | "tools"
                        | "thinking"
                        | "reasoning"
                        | "reasoning_effort"
                        | "tool_choice"
                        | "cache_control"
                        | "session_id"
                        | "prompt_cache_key"
                        | "prompt_cache_retention"
                        | "prompt_cache_options"
                        | "cache_prompt"
                        | "cache_topology"
                )
            })
        {
            anyhow::bail!("cache topology forbids late prompt extensions");
        }

        let is_responses_api = self.authorized_responses_api_for(&config.model)?;
        let request_body = self.build_request_body_for_api(messages, config, is_responses_api)?;
        let mut body = self.managed_request(request_body)?;

        if self.managed_inference_authorization.is_some() {
            let (required, forbidden) = if is_responses_api {
                ("input", "messages")
            } else {
                ("messages", "input")
            };
            anyhow::ensure!(
                body.get(required).is_some() && body.get(forbidden).is_none(),
                "managed inference request body does not match authorized endpoint"
            );
        }

        if let Some(scope) = self.cache_scope()? {
            use maestro_runtime_contracts::cache_topology::{
                CacheTopology, HostedCacheTopology, wire_shape,
            };
            let mut topology = CacheTopology::prepare(
                wire_shape(&body, scope.namespace()).map_err(anyhow::Error::msg)?,
                None,
            )
            .map_err(anyhow::Error::msg)?;
            if let Some(prepared) = &config.cache_topology {
                topology.generation = prepared.topology().generation;
                topology.transition = prepared.topology().transition;
            }
            body["cache_topology"] = serde_json::to_value(HostedCacheTopology { scope, topology })?;
        }

        // Get the appropriate API URL for this model, honoring any custom
        // provider base URL (Mistral/Groq/DeepSeek/Moonshot/DashScope/etc.).
        let api_url = self.request_url_for_api(&config.model, is_responses_api);

        // Make request
        let request = self
            .client
            .post(&api_url)
            .headers(self.headers())
            .json(&body);
        let response =
            send_with_response_open_timeout(request, self.response_open_timeout()).await?;

        // Classify the provider response before enforcing success-only receipt
        // headers. Managed gateways may legitimately omit receipts on an
        // authentication, throttling, or upstream failure response; that must
        // not hide the actionable HTTP status behind a receipt protocol error.
        if !response.status().is_success() {
            let status = response.status();
            if self.managed_gateway {
                let expected_lineage = body.get("lineage_id").and_then(serde_json::Value::as_str);
                if let Ok(receipt) = managed_gateway_receipt(response.headers(), expected_lineage) {
                    let _ = tx.send(StreamEvent::ManagedGatewayReceipt(receipt));
                }
            }
            let error_text = response.text().await.unwrap_or_default();
            let kind = if status.is_server_error()
                || matches!(
                    status,
                    reqwest::StatusCode::REQUEST_TIMEOUT
                        | reqwest::StatusCode::CONFLICT
                        | reqwest::StatusCode::TOO_MANY_REQUESTS
                ) {
                ProviderStreamErrorKind::TransientProtocol
            } else {
                ProviderStreamErrorKind::ProviderDeclaredFailure
            };
            let _ = tx.send(StreamEvent::ProviderError {
                kind,
                message: format!(
                    "API error {status}: {}",
                    super::summarize_error_body(&error_text)
                ),
            });
            return Ok(CancellableStream::detached(rx));
        }

        if self.managed_gateway {
            let expected_lineage = body.get("lineage_id").and_then(serde_json::Value::as_str);
            match managed_gateway_receipt(response.headers(), expected_lineage) {
                Ok(mut receipt) => {
                    let prompt_digest = format!(
                        "sha256:{:x}",
                        Sha256::digest(config.system.as_deref().unwrap_or_default().as_bytes())
                    );
                    // Gateway attests its final serialized provider instruction.
                    // Legacy gateways and transformed prompts cannot prove this
                    // experiment's delivery; ordinary inference still works.
                    receipt.provider_prompt_sha256 = response
                        .headers()
                        .get("x-evalops-provider-prompt-sha256")
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| *value == prompt_digest)
                        .map(str::to_owned);
                    // Accept only a complete, bounded evidence pair. The gateway
                    // owns adapter conversion, so the client cannot recompute it.
                    if let Some((digest, count)) =
                        managed_provider_tools_evidence(response.headers())
                    {
                        receipt.provider_tools_sha256 = Some(digest);
                        receipt.provider_tool_count = Some(count);
                    }
                    let _ = tx.send(StreamEvent::ManagedGatewayReceipt(receipt));
                }
                Err(error) => {
                    let _ = tx.send(StreamEvent::ProviderError {
                        kind: ProviderStreamErrorKind::TransientProtocol,
                        message: format!("managed_gateway_receipt_error: {error}"),
                    });
                    return Ok(CancellableStream::detached(rx));
                }
            }
        }

        // Spawn task to process SSE stream
        let model = config.model.clone();

        let producer = if is_responses_api {
            // Use eventsource-stream for proper SSE parsing (Responses API)
            let stream = response.bytes_stream();
            tokio::spawn(async move {
                let mut sse_stream = stream
                    .map(|result| result.map_err(std::io::Error::other))
                    .eventsource();

                let mut content_started = false;
                let mut emitted_text = String::new();
                let mut emitted_tool_call_ids = std::collections::HashSet::new();
                let mut streamed_tool_argument_indices = std::collections::HashSet::new();
                let mut tool_indices_by_output = std::collections::HashMap::new();
                let mut tool_call_index = 1; // Start at 1, reserve 0 for text content

                loop {
                    let event_result = tokio::select! {
                        () = tx.closed() => return,
                        event_result = sse_stream.next() => event_result,
                    };
                    let Some(event_result) = event_result else {
                        break;
                    };
                    match event_result {
                        Ok(sse) => {
                            // Parse the SSE data as JSON
                            let event: ResponsesSseEvent = match serde_json::from_str(&sse.data) {
                                Ok(e) => e,
                                Err(_) => continue,
                            };

                            match event.kind.as_str() {
                                "response.created" => {
                                    if let Some(resp) = &event.response {
                                        if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                                            let _ = tx.send(StreamEvent::MessageStart {
                                                id: id.to_string(),
                                                model: model.clone(),
                                            });
                                        }
                                    }
                                }
                                // Reasoning/thinking events - stream as thinking deltas
                                "response.reasoning_summary_text.delta"
                                | "response.reasoning_text.delta" => {
                                    if let Some(delta) = &event.delta {
                                        let _ = tx.send(StreamEvent::ThinkingDelta {
                                            index: 0,
                                            thinking: delta.clone(),
                                        });
                                    }
                                }
                                "response.output_item.added" => {
                                    // Check if this is a message item (not reasoning)
                                    if let Some(item) = &event.item {
                                        let item_type = item.get("type").and_then(|v| v.as_str());
                                        if item_type == Some("message") && !content_started {
                                            content_started = true;
                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                index: 0,
                                                block: ContentBlock::Text {
                                                    text: String::new(),
                                                },
                                            });
                                        }
                                    }
                                }
                                "response.content_part.added"
                                    if event.part.as_ref().and_then(|part| {
                                        part.get("type").and_then(serde_json::Value::as_str)
                                    }) == Some("output_text")
                                        && !content_started =>
                                {
                                    content_started = true;
                                    let _ = tx.send(StreamEvent::ContentBlockStart {
                                        index: 0,
                                        block: ContentBlock::Text {
                                            text: String::new(),
                                        },
                                    });
                                }
                                "response.content_part.done" => {
                                    if let Some(text) = event
                                        .part
                                        .as_ref()
                                        .and_then(|part| part.get("text"))
                                        .and_then(serde_json::Value::as_str)
                                        .and_then(|text| missing_text_suffix(&emitted_text, text))
                                    {
                                        if !content_started {
                                            content_started = true;
                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                index: 0,
                                                block: ContentBlock::Text {
                                                    text: String::new(),
                                                },
                                            });
                                        }
                                        emitted_text.push_str(&text);
                                        let _ = tx.send(StreamEvent::TextDelta { index: 0, text });
                                    }
                                }
                                "response.output_item.done" => {
                                    if let Some(item) = &event.item {
                                        // Check if this is a function call
                                        if let Some((call_id, name, arguments)) =
                                            extract_function_call(item)
                                        {
                                            if !emitted_tool_call_ids.insert(call_id.clone()) {
                                                continue;
                                            }
                                            // Emit tool use block
                                            let tool_index =
                                                if let Some(output_index) = event.output_index {
                                                    *tool_indices_by_output
                                                        .entry(output_index)
                                                        .or_insert_with(|| {
                                                            let index = tool_call_index;
                                                            tool_call_index += 1;
                                                            index
                                                        })
                                                } else {
                                                    let index = tool_call_index;
                                                    tool_call_index += 1;
                                                    index
                                                };

                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                index: tool_index,
                                                block: ContentBlock::ToolUse {
                                                    id: call_id.clone(),
                                                    name: name.clone(),
                                                    input: arguments.clone(),
                                                    gemini_context: extract_gemini_context(item),
                                                },
                                            });

                                            if event.output_index.is_none_or(|output_index| {
                                                !streamed_tool_argument_indices
                                                    .contains(&output_index)
                                            }) {
                                                let _ = tx.send(StreamEvent::InputJsonDelta {
                                                    index: tool_index,
                                                    partial_json: arguments.to_string(),
                                                });
                                            }

                                            let _ = tx.send(StreamEvent::ContentBlockStop {
                                                index: tool_index,
                                                thinking_signature: None,
                                            });
                                        } else if let Some(text) = extract_text_from_item(item)
                                            .and_then(|text| {
                                                missing_text_suffix(&emitted_text, &text)
                                            })
                                        {
                                            if !content_started {
                                                content_started = true;
                                                let _ = tx.send(StreamEvent::ContentBlockStart {
                                                    index: 0,
                                                    block: ContentBlock::Text {
                                                        text: String::new(),
                                                    },
                                                });
                                            }
                                            emitted_text.push_str(&text);
                                            let _ =
                                                tx.send(StreamEvent::TextDelta { index: 0, text });
                                        }
                                    }
                                }
                                // Handle streaming function call arguments
                                "response.function_call_arguments.delta" => {
                                    if let Some(delta) = &event.delta {
                                        // Get or create tool call index
                                        let output_index = event.output_index.unwrap_or(0);
                                        streamed_tool_argument_indices.insert(output_index);
                                        let tool_idx = *tool_indices_by_output
                                            .entry(output_index)
                                            .or_insert_with(|| {
                                                let index = tool_call_index;
                                                tool_call_index += 1;
                                                index
                                            });

                                        let _ = tx.send(StreamEvent::InputJsonDelta {
                                            index: tool_idx,
                                            partial_json: delta.clone(),
                                        });
                                    }
                                }
                                "response.output_text.delta" => {
                                    if !content_started {
                                        content_started = true;
                                        let _ = tx.send(StreamEvent::ContentBlockStart {
                                            index: 0,
                                            block: ContentBlock::Text {
                                                text: String::new(),
                                            },
                                        });
                                    }
                                    if let Some(delta) = &event.delta {
                                        emitted_text.push_str(delta);
                                        let _ = tx.send(StreamEvent::TextDelta {
                                            index: 0,
                                            text: delta.clone(),
                                        });
                                    }
                                }
                                "response.output_text.done" => {
                                    // Text content finished - but don't stop yet, more might come
                                }
                                "response.completed" => {
                                    if let Some(output) = event
                                        .response
                                        .as_ref()
                                        .and_then(|response| response.get("output"))
                                        .and_then(serde_json::Value::as_array)
                                    {
                                        for (output_index, item) in output.iter().enumerate() {
                                            if let Some((call_id, name, arguments)) =
                                                extract_function_call(item)
                                            {
                                                if emitted_tool_call_ids.insert(call_id.clone()) {
                                                    let output_index = output_index as i64;
                                                    let tool_index = *tool_indices_by_output
                                                        .entry(output_index)
                                                        .or_insert_with(|| {
                                                            let index = tool_call_index;
                                                            tool_call_index += 1;
                                                            index
                                                        });
                                                    let partial_json = arguments.to_string();
                                                    let _ =
                                                        tx.send(StreamEvent::ContentBlockStart {
                                                            index: tool_index,
                                                            block: ContentBlock::ToolUse {
                                                                id: call_id,
                                                                name,
                                                                input: arguments,
                                                                gemini_context:
                                                                    extract_gemini_context(item),
                                                            },
                                                        });
                                                    if !streamed_tool_argument_indices
                                                        .contains(&output_index)
                                                    {
                                                        let _ =
                                                            tx.send(StreamEvent::InputJsonDelta {
                                                                index: tool_index,
                                                                partial_json,
                                                            });
                                                    }
                                                    let _ =
                                                        tx.send(StreamEvent::ContentBlockStop {
                                                            index: tool_index,
                                                            thinking_signature: None,
                                                        });
                                                }
                                            } else if let Some(text) = extract_text_from_item(item)
                                                .and_then(|text| {
                                                    missing_text_suffix(&emitted_text, &text)
                                                })
                                            {
                                                if !content_started {
                                                    content_started = true;
                                                    let _ =
                                                        tx.send(StreamEvent::ContentBlockStart {
                                                            index: 0,
                                                            block: ContentBlock::Text {
                                                                text: String::new(),
                                                            },
                                                        });
                                                }
                                                emitted_text.push_str(&text);
                                                let _ = tx.send(StreamEvent::TextDelta {
                                                    index: 0,
                                                    text,
                                                });
                                            }
                                        }
                                    }
                                    // Now close the content block
                                    if content_started {
                                        let _ = tx.send(StreamEvent::ContentBlockStop {
                                            index: 0,
                                            thinking_signature: None,
                                        });
                                    }
                                    // Extract usage from response if present
                                    if let Some(resp) = &event.response {
                                        if let Some(usage) = resp.get("usage") {
                                            let input = usage
                                                .get("input_tokens")
                                                .and_then(serde_json::Value::as_u64)
                                                .unwrap_or(0);
                                            let output = usage
                                                .get("output_tokens")
                                                .and_then(serde_json::Value::as_u64)
                                                .unwrap_or(0);
                                            // Extract cached tokens from input_tokens_details
                                            let cache_read = usage
                                                .get("input_tokens_details")
                                                .and_then(|d| d.get("cached_tokens"))
                                                .and_then(serde_json::Value::as_u64);
                                            // Extract reasoning tokens from output_tokens_details
                                            let reasoning_tokens = usage
                                                .get("output_tokens_details")
                                                .and_then(|d| d.get("reasoning_tokens"))
                                                .and_then(serde_json::Value::as_u64);
                                            if let Some(tokens) = reasoning_tokens {
                                                let _ =
                                                    tx.send(StreamEvent::ReasoningUsage { tokens });
                                            }
                                            if let Some(cost_usd) = usage
                                                .get("cost")
                                                .and_then(serde_json::Value::as_f64)
                                                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                                            {
                                                let _ =
                                                    tx.send(StreamEvent::ProviderCost { cost_usd });
                                            }
                                            let cache_write = usage
                                                .get("input_tokens_details")
                                                .and_then(|details| {
                                                    details.get("cache_write_tokens")
                                                })
                                                .and_then(serde_json::Value::as_u64);
                                            let _ = tx.send(StreamEvent::Usage {
                                                input_tokens: uncached_input_tokens(
                                                    input,
                                                    cache_read,
                                                    cache_write,
                                                ),
                                                output_tokens: output,
                                                cache_read_tokens: cache_read,
                                                cache_creation_tokens: cache_write,
                                            });
                                        }
                                    }
                                    let _ = tx.send(StreamEvent::MessageStop { stop_reason: None });
                                    return;
                                }
                                "response.incomplete" => {
                                    let error = incomplete_response_error(event.response.as_ref());
                                    let _ = tx.send(StreamEvent::ProviderError {
                                        kind: error.kind,
                                        message: error.to_string(),
                                    });
                                    return;
                                }
                                "response.failed" => {
                                    let (kind, message) =
                                        response_failed_error(event.response.as_ref());
                                    let _ = tx.send(StreamEvent::ProviderError { kind, message });
                                    return;
                                }
                                _ => {
                                    // Unknown event type, ignore
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(StreamEvent::ProviderError {
                                kind: ProviderStreamErrorKind::TransientProtocol,
                                message: format!("SSE stream error: {e}"),
                            });
                            return;
                        }
                    }
                }

                let _ = tx.send(StreamEvent::ProviderError {
                    kind: ProviderStreamErrorKind::TransientProtocol,
                    message: RESPONSES_MISSING_TERMINAL_EVENT_ERROR.to_string(),
                });
            })
        } else {
            // Chat Completions API - uses simpler line-based SSE
            let mut stream = response.bytes_stream();
            tokio::spawn(async move {
                let mut buffer = String::new();
                let mut message_id = String::new();
                let mut current_tool_calls: Vec<ToolCallAccumulator> = Vec::new();
                let mut content_started = false;
                let mut thinking_started = false;
                let mut content_block_stopped = false;

                loop {
                    let chunk = tokio::select! {
                        () = tx.closed() => return,
                        chunk = stream.next() => chunk,
                    };
                    let Some(chunk) = chunk else {
                        break;
                    };
                    match chunk {
                        Ok(bytes) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));

                            // Process complete SSE lines
                            while let Some(pos) = buffer.find('\n') {
                                let line = buffer[..pos].trim().to_string();
                                buffer = buffer[pos + 1..].to_string();

                                if line.is_empty() {
                                    continue;
                                }

                                if line == "data: [DONE]" {
                                    if !content_block_stopped
                                        && (content_started || thinking_started)
                                        && current_tool_calls.is_empty()
                                    {
                                        let _ = tx.send(StreamEvent::ContentBlockStop {
                                            index: 0,
                                            thinking_signature: None,
                                        });
                                    }
                                    // Flush any completed tool calls
                                    for (idx, call) in current_tool_calls.iter().enumerate() {
                                        if !call.name.is_empty()
                                            && !call.arguments.trim().is_empty()
                                        {
                                            let block_idx = idx + 1; // reserve 0 for text
                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                index: block_idx,
                                                block: ContentBlock::ToolUse {
                                                    id: call.id.clone(),
                                                    name: call.name.clone(),
                                                    input: serde_json::from_str(&call.arguments)
                                                        .unwrap_or(serde_json::json!({})),
                                                    gemini_context: None,
                                                },
                                            });
                                            let _ = tx.send(StreamEvent::InputJsonDelta {
                                                index: block_idx,
                                                partial_json: call.arguments.clone(),
                                            });
                                            let _ = tx.send(StreamEvent::ContentBlockStop {
                                                index: block_idx,
                                                thinking_signature: None,
                                            });
                                        }
                                    }
                                    let _ = tx.send(StreamEvent::MessageStop { stop_reason: None });
                                    return;
                                }

                                if let Some(data) = line.strip_prefix("data: ") {
                                    if let Ok(chunk) = serde_json::from_str::<OpenAiChunk>(data) {
                                        if message_id.is_empty() {
                                            message_id = chunk.id.clone();
                                            let _ = tx.send(StreamEvent::MessageStart {
                                                id: chunk.id.clone(),
                                                model: model.clone(),
                                            });
                                        }

                                        for choice in &chunk.choices {
                                            if let Some(thinking) = choice.delta.thinking_text() {
                                                if !thinking_started {
                                                    thinking_started = true;
                                                    let _ =
                                                        tx.send(StreamEvent::ContentBlockStart {
                                                            index: 0,
                                                            block: ContentBlock::Thinking {
                                                                thinking: String::new(),
                                                                signature: None,
                                                            },
                                                        });
                                                }
                                                let _ = tx.send(StreamEvent::ThinkingDelta {
                                                    index: 0,
                                                    thinking: thinking.to_owned(),
                                                });
                                            }

                                            if let Some(content) = choice.delta.visible_text() {
                                                if !content_started {
                                                    content_started = true;
                                                    let _ =
                                                        tx.send(StreamEvent::ContentBlockStart {
                                                            index: 0,
                                                            block: ContentBlock::Text {
                                                                text: String::new(),
                                                            },
                                                        });
                                                }
                                                let _ = tx.send(StreamEvent::TextDelta {
                                                    index: 0,
                                                    text: content.to_owned(),
                                                });
                                            }

                                            if let Some(tool_calls) = &choice.delta.tool_calls {
                                                for tc in tool_calls {
                                                    let idx = tc.index.unwrap_or(0);

                                                    while current_tool_calls.len() <= idx {
                                                        current_tool_calls.push(
                                                            ToolCallAccumulator {
                                                                id: String::new(),
                                                                name: String::new(),
                                                                arguments: String::new(),
                                                            },
                                                        );
                                                    }

                                                    if let Some(id) = &tc.id {
                                                        current_tool_calls[idx].id = id.clone();
                                                    }
                                                    if let Some(func) = &tc.function {
                                                        if let Some(name) = &func.name {
                                                            current_tool_calls[idx].name =
                                                                name.clone();
                                                        }
                                                        if let Some(args) = &func.arguments {
                                                            current_tool_calls[idx]
                                                                .arguments
                                                                .push_str(args);
                                                        }
                                                    }
                                                }
                                            }

                                            if choice.finish_reason.is_some() {
                                                if !content_block_stopped
                                                    && (content_started || thinking_started)
                                                    && current_tool_calls.is_empty()
                                                {
                                                    content_block_stopped = true;
                                                    let _ =
                                                        tx.send(StreamEvent::ContentBlockStop {
                                                            index: 0,
                                                            thinking_signature: None,
                                                        });
                                                }
                                                // On tool_calls finish, flush completed tool calls
                                                if choice.finish_reason.as_deref()
                                                    == Some("tool_calls")
                                                {
                                                    for (idx, call) in
                                                        current_tool_calls.iter().enumerate()
                                                    {
                                                        if call.name.is_empty()
                                                            || call.arguments.trim().is_empty()
                                                        {
                                                            continue;
                                                        }
                                                        let block_idx = idx + 1; // reserve 0 for text
                                                        let _ = tx.send(
                                                            StreamEvent::ContentBlockStart {
                                                                index: block_idx,
                                                                block: ContentBlock::ToolUse {
                                                                    id: call.id.clone(),
                                                                    name: call.name.clone(),
                                                                    input: serde_json::from_str(
                                                                        &call.arguments,
                                                                    )
                                                                    .unwrap_or(
                                                                        serde_json::json!({}),
                                                                    ),
                                                                    gemini_context: None,
                                                                },
                                                            },
                                                        );
                                                        let _ =
                                                            tx.send(StreamEvent::InputJsonDelta {
                                                                index: block_idx,
                                                                partial_json: call
                                                                    .arguments
                                                                    .clone(),
                                                            });
                                                        let _ = tx.send(
                                                            StreamEvent::ContentBlockStop {
                                                                index: block_idx,
                                                                thinking_signature: None,
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        }

                                        if let Some(usage) = &chunk.usage {
                                            if let Some(cost_usd) = usage.cost_usd() {
                                                let _ =
                                                    tx.send(StreamEvent::ProviderCost { cost_usd });
                                            }
                                            let _ = tx.send(StreamEvent::Usage {
                                                input_tokens: uncached_input_tokens(
                                                    usage.prompt_tokens.unwrap_or(0),
                                                    usage
                                                        .prompt_tokens_details
                                                        .as_ref()
                                                        .and_then(|d| d.cached_tokens),
                                                    usage
                                                        .prompt_tokens_details
                                                        .as_ref()
                                                        .and_then(|d| d.cache_write_tokens),
                                                ),
                                                output_tokens: usage.completion_tokens.unwrap_or(0),
                                                cache_read_tokens: usage
                                                    .prompt_tokens_details
                                                    .as_ref()
                                                    .and_then(|d| d.cached_tokens),
                                                cache_creation_tokens: usage
                                                    .prompt_tokens_details
                                                    .as_ref()
                                                    .and_then(|d| d.cache_write_tokens),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(StreamEvent::ProviderError {
                                kind: ProviderStreamErrorKind::TransientProtocol,
                                message: format!("Stream error: {e}"),
                            });
                            break;
                        }
                    }
                }
            })
        };

        Ok(CancellableStream::with_producer(rx, producer))
    }
}

impl AiClient for OpenAiClient {
    fn provider(&self) -> AiProvider {
        AiProvider::OpenAI
    }

    async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        Ok(self
            .stream_with_producer(messages, config)
            .await?
            .into_receiver())
    }
}

// ============================================================================
// OpenAI API Types
// ============================================================================
//
// These types represent the OpenAI Chat Completions API request/response format.
// They use serde for serialization (requests) and deserialization (responses).
//
// # Serde Serialization Attributes
//
// - `#[serde(skip_serializing_if = "Option::is_none")]`: Omit null fields from JSON
// - `#[serde(untagged)]`: Serialize enum as the inner type (no type field)
// - `#[serde(tag = "type")]`: Add a type discriminator field for enums

/// Message in `OpenAI` format (role + content)
///
/// # Serde Conditional Serialization
///
/// Fields marked with `skip_serializing_if` are omitted from JSON if None:
///
/// ```rust,ignore
/// #[serde(skip_serializing_if = "Option::is_none")]
/// tool_calls: Option<Vec<OpenAiToolCall>>,
/// ```
///
/// This generates cleaner JSON - `{"role": "user", "content": "Hi"}` instead
/// of `{"role": "user", "content": "Hi", "tool_calls": null}`.
#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Tool name - required by Mistral for tool results
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

/// Content can be either simple text or structured parts
///
/// # Serde Untagged Enum
///
/// The `#[serde(untagged)]` attribute means this enum serializes without
/// a type discriminator. Instead, serde tries each variant in order:
///
/// ```json
/// "Hello"                              -> Text("Hello")
/// [{"type": "text", "text": "Hi"}]     -> Parts([...])
/// ```
///
/// This matches `OpenAI`'s API which accepts both formats.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAiContent {
    Text(String),
    Parts(Vec<OpenAiContentPart>),
}

/// Content part (text or image) in a multi-part message
///
/// # Serde Tagged Enum
///
/// Unlike `OpenAiContent` (untagged), this uses `tag = "type"` to add
/// a discriminator field:
///
/// ```json
/// {"type": "text", "text": "Hello"}
/// {"type": "image_url", "image_url": {"url": "..."}}
/// ```
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlData },
}

#[derive(Debug, Serialize)]
struct ImageUrlData {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAiFunction,
}

#[derive(Debug, Serialize)]
struct OpenAiFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    tool_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function: Option<OpenAiFunctionCall>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
}

// Streaming response types
#[derive(Debug, Deserialize)]
struct OpenAiChunk {
    id: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
    created: u64,
    #[allow(dead_code)]
    model: String,
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    #[allow(dead_code)]
    index: usize,
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiDelta {
    /// Role of the message (typically "assistant" for streaming responses)
    #[serde(default)]
    #[allow(dead_code)] // Part of API structure, role is implicit in streaming context
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    /// Vertex OpenAI-compat and LiteLLM put Gemini thought parts here.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    extra_content: Option<OpenAiExtraContent>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiExtraContent {
    #[serde(default)]
    google: Option<OpenAiGoogleExtraContent>,
}

#[derive(Debug, Deserialize)]
struct OpenAiGoogleExtraContent {
    #[serde(default)]
    thought: Option<bool>,
}

impl OpenAiDelta {
    fn google_thought(&self) -> bool {
        self.extra_content
            .as_ref()
            .and_then(|extra| extra.google.as_ref())
            .and_then(|google| google.thought)
            == Some(true)
    }

    fn thinking_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .filter(|text| !text.is_empty())
            .or_else(|| {
                self.google_thought()
                    .then_some(self.content.as_deref())
                    .flatten()
                    .filter(|text| !text.is_empty())
            })
    }

    fn visible_text(&self) -> Option<&str> {
        if self.google_thought() {
            return None;
        }
        self.content.as_deref()
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    cost: Option<f64>,
    // Gateway emits both spellings together; serde aliases would reject that
    // response as a duplicate field and discard its token usage as well.
    cost_micros: Option<u64>,
    #[serde(rename = "costMicros")]
    cost_micros_camel: Option<u64>,
    pricing_source: Option<String>,
    #[serde(rename = "pricingSource")]
    pricing_source_camel: Option<String>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

impl OpenAiUsage {
    fn cost_usd(&self) -> Option<f64> {
        if [
            self.pricing_source.as_deref(),
            self.pricing_source_camel.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|source| matches!(source, "missing" | "unconfigured" | "cost_unavailable"))
        {
            return None;
        }
        if self
            .cost_micros
            .zip(self.cost_micros_camel)
            .is_some_and(|(snake, camel)| snake != camel)
        {
            return None;
        }
        self.cost_micros
            .or(self.cost_micros_camel)
            .map(|micros| micros as f64 / 1_000_000.0)
            .or_else(|| self.cost.filter(|cost| cost.is_finite() && *cost >= 0.0))
    }
}

// OpenAI-compatible prompt counts include cached reads/writes. The native
// usage contract keeps these as separate buckets, like Anthropic usage.
fn uncached_input_tokens(total: u64, read: Option<u64>, write: Option<u64>) -> u64 {
    total
        .saturating_sub(read.unwrap_or(0))
        .saturating_sub(write.unwrap_or(0))
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

fn apply_prompt_cache_key(body: &mut serde_json::Value, provider: Option<&str>, key: Option<&str>) {
    if let (Some("openrouter"), Some(key)) = (
        provider,
        key.filter(|key| !key.is_empty() && key.len() <= 256),
    ) {
        body["prompt_cache_key"] = serde_json::json!(key);
    }
}

/// Accumulator for building tool calls from streaming deltas
///
/// # Problem
///
/// In Chat Completions API, tool calls arrive as incremental deltas:
///
/// ```json
/// {"index": 0, "id": "call_123"}
/// {"index": 0, "function": {"name": "read"}}
/// {"index": 0, "function": {"arguments": "{\"pa"}}
/// {"index": 0, "function": {"arguments": "th\":"}}
/// {"index": 0, "function": {"arguments": "\"/tmp\""}}
/// {"index": 0, "function": {"arguments": "}"}}
/// ```
///
/// # Solution
///
/// This struct accumulates the fragments into complete tool calls.
/// We maintain a `Vec<ToolCallAccumulator>` indexed by the `index` field,
/// appending to the appropriate accumulator as deltas arrive.
///
/// Once the stream completes, we serialize the complete tool calls.
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn disconnected_endpoint_refuses_redirects() {
        use std::io::{Read, Write};
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_url = format!("http://{}", origin.local_addr().unwrap());
        let location = format!("http://{}", destination.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "request ended before HTTP headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() <= 16 * 1024, "request headers too large");
            }
            write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client =
            super::OpenAiClient::with_disconnected_endpoint("fixture", &origin_url, "ollama")
                .unwrap();
        let response = client.client.get(&origin_url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            client.request_model_name("ollama/customer-model"),
            "customer-model"
        );
        server.join().unwrap();
    }

    #[test]
    fn cached_prompt_buckets_do_not_inflate_native_context() {
        // Counts observed during the governed Fireworks tmux trial.
        assert_eq!(uncached_input_tokens(11_958, Some(10_875), Some(0)), 1_083);
        assert_eq!(uncached_input_tokens(100, None, None), 100);
        assert_eq!(uncached_input_tokens(100, Some(70), Some(30)), 0);
        assert_eq!(uncached_input_tokens(10, Some(20), None), 0);
    }

    #[test]
    fn cache_topology_hosted_checkpoint_uses_one_hour_before_tail() {
        let mut client = OpenAiClient::new("fixture")
            .unwrap()
            .with_route_provider("openrouter")
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider":"openrouter", "environment":"production", "credential_name":"default"
                }),
            )
            .unwrap();
        client.set_managed_inference_authorization(Some(managed_authorization_fixture("cache-1")));
        let scope = client.cache_scope().unwrap().unwrap();
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::text("checkpoint"),
        }];
        let mut config = RequestConfig {
            model: "claude-sonnet-4-5".into(),
            ..Default::default()
        };
        config.cache_topology = Some(
            crate::cache_topology::PreparedPrompt::prepare(
                &history,
                &config,
                scope.namespace(),
                None,
            )
            .unwrap()
            .with_volatile_tail(Some("clock and plan".into())),
        );
        let body = client
            .build_request_body_for_api(&history, &config, false)
            .unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
        assert_eq!(body["messages"][1]["content"], "clock and plan");
        let wire = maestro_runtime_contracts::cache_topology::CacheTopology::prepare(
            maestro_runtime_contracts::cache_topology::wire_shape(&body, scope.namespace())
                .unwrap(),
            None,
        )
        .unwrap();
        let mut changed = body.clone();
        changed["messages"][1]["content"] = serde_json::json!("late change");
        assert!(
            wire.validate(
                &maestro_runtime_contracts::cache_topology::wire_shape(&changed, scope.namespace())
                    .unwrap()
            )
            .is_err()
        );
        config.cache_topology = Some(
            crate::cache_topology::PreparedPrompt::auxiliary(&history, &config, scope.namespace())
                .unwrap(),
        );
        let auxiliary = client
            .build_request_body_for_api(&history, &config, false)
            .unwrap();
        assert_eq!(auxiliary["messages"][0]["content"], "checkpoint");
        assert!(auxiliary.get("prompt_cache_key").is_none());
    }

    #[test]
    fn prompt_cache_affinity_is_opt_in_and_openrouter_only() {
        let mut body = serde_json::json!({});
        super::apply_prompt_cache_key(&mut body, Some("openai"), Some("review-v1"));
        assert!(body.get("prompt_cache_key").is_none());
        super::apply_prompt_cache_key(&mut body, Some("openrouter"), None);
        assert!(body.get("prompt_cache_key").is_none());
        super::apply_prompt_cache_key(&mut body, Some("openrouter"), Some(&"x".repeat(257)));
        assert!(body.get("prompt_cache_key").is_none());
        super::apply_prompt_cache_key(&mut body, Some("openrouter"), Some("review-v1"));
        assert_eq!(body["prompt_cache_key"], "review-v1");
    }

    #[test]
    fn openrouter_usage_preserves_cache_writes_and_actual_cost() {
        let usage: super::OpenAiUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100, "completion_tokens": 5, "cost": 0.012,
            "prompt_tokens_details": {"cached_tokens": 70, "cache_write_tokens": 30}
        }))
        .unwrap();
        assert_eq!(usage.cost, Some(0.012));
        let details = usage.prompt_tokens_details.unwrap();
        assert_eq!(details.cached_tokens, Some(70));
        assert_eq!(details.cache_write_tokens, Some(30));
        let absent: super::OpenAiUsage = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(absent.cost, None);
    }

    #[test]
    fn gateway_usage_cost_micros_preserves_units_and_unavailable_states() {
        for value in [
            serde_json::json!({"cost_micros":12345}),
            serde_json::json!({"costMicros":12345}),
            serde_json::json!({"cost_micros":12345,"costMicros":12345,"cost":9.0}),
        ] {
            let usage: super::OpenAiUsage = serde_json::from_value(value).unwrap();
            assert_eq!(usage.cost_usd(), Some(0.012345));
        }
        for value in [
            serde_json::json!({}),
            serde_json::json!({"cost_micros":1,"costMicros":2,"cost":9.0}),
            serde_json::json!({"cost_micros":0,"pricing_source":"unconfigured"}),
            serde_json::json!({"costMicros":0,"pricingSource":"missing"}),
            serde_json::json!({"cost_micros":12345,"cost":9.0,"pricing_source":"cost_unavailable"}),
        ] {
            let usage: super::OpenAiUsage = serde_json::from_value(value).unwrap();
            assert_eq!(usage.cost_usd(), None);
        }
        let zero: super::OpenAiUsage = serde_json::from_value(
            serde_json::json!({"cost_micros":0,"pricing_source":"provider_ref"}),
        )
        .unwrap();
        assert_eq!(zero.cost_usd(), Some(0.0));
    }

    #[tokio::test]
    async fn gateway_usage_cost_micros_reaches_stream_without_losing_tokens() {
        for (source, expected) in [("provider_ref", Some(0.012345)), ("unconfigured", None)] {
            let chunk = serde_json::json!({"id":"gateway-cost","object":"chat.completion.chunk","created":1,"model":"glm-5p3","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"cost_micros":12345,"costMicros":12345,"pricing_source":source,"pricingSource":source}});
            let wire = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            let client = client_with_responses_sse(&wire).with_route_provider("fireworks");
            let mut stream = client
                .stream(
                    &[],
                    &RequestConfig {
                        model: "glm-5p3".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let (cost, tokens) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut cost = None;
                let mut tokens = None;
                while let Some(event) = stream.recv().await {
                    match event {
                        StreamEvent::ProviderCost { cost_usd } => cost = Some(cost_usd),
                        StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            ..
                        } => tokens = Some((input_tokens, output_tokens)),
                        _ => {}
                    }
                }
                (cost, tokens)
            })
            .await
            .expect("Gateway usage stream should terminate");
            assert_eq!(tokens, Some((100, 5)));
            assert_eq!(cost, expected);
        }
    }

    use super::*;
    use crate::client::UnifiedClient;

    fn client_with_responses_sse(sse_body: &str) -> OpenAiClient {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Responses server");
        let address = listener.local_addr().expect("mock server address");
        let sse_body = sse_body.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Responses request");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("set request timeout");
            let mut request = [0_u8; 16 * 1024];
            let _ = stream.read(&mut request).expect("read Responses request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse_body.len(),
                sse_body
            )
            .expect("write Responses SSE");
        });

        OpenAiClient::with_base_url("test-key", format!("http://{address}/v1"))
            .expect("construct mock Responses client")
    }

    async fn collect_responses_sse(sse_body: &str) -> Vec<StreamEvent> {
        let client = client_with_responses_sse(sse_body);
        let config = RequestConfig {
            model: "gpt-5.1-codex-max".to_string(),
            ..Default::default()
        };
        let mut receiver = client
            .stream(&[], &config)
            .await
            .expect("start Responses stream");

        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let mut events = Vec::new();
            while let Some(event) = receiver.recv().await {
                events.push(event);
            }
            events
        })
        .await
        .expect("Responses stream should terminate")
    }

    #[tokio::test]
    async fn responses_reasoning_usage_is_reported_without_double_counting_output() {
        let events = collect_responses_sse(
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":30,\"output_tokens_details\":{\"reasoning_tokens\":20}}}}\n\n"
        ).await;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ReasoningUsage { tokens: 20 }))
                .count(),
            1
        );
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Usage {
                input_tokens: 100,
                output_tokens: 30,
                ..
            }
        )));
        let unknown = collect_responses_sse("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-2\",\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":30}}}\n\n").await;
        assert!(
            !unknown
                .iter()
                .any(|e| matches!(e, StreamEvent::ReasoningUsage { .. }))
        );
    }

    #[tokio::test]
    async fn openrouter_tool_only_stream_reports_cost_and_cache_writes() {
        let client = client_with_responses_sse(concat!(
            "data: {\"id\":\"gen-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openai/gpt-6-astra\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"id\":\"gen-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openai/gpt-6-astra\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5,\"cost\":0.012,\"prompt_tokens_details\":{\"cached_tokens\":70,\"cache_write_tokens\":30}}}\n\n",
            "data: [DONE]\n\n"
        )).with_route_provider("openrouter");
        let mut stream = client
            .stream(
                &[],
                &RequestConfig {
                    model: "openai/gpt-6-astra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut saw_cost = false;
        let mut saw_usage = false;
        while let Some(event) = stream.recv().await {
            match event {
                StreamEvent::ProviderCost { cost_usd } => {
                    assert_eq!(cost_usd, 0.012);
                    saw_cost = true;
                }
                StreamEvent::Usage {
                    input_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    ..
                } => {
                    assert_eq!(input_tokens, 0);
                    assert_eq!(cache_read_tokens, Some(70));
                    assert_eq!(cache_creation_tokens, Some(30));
                    assert_eq!(
                        input_tokens + cache_read_tokens.unwrap() + cache_creation_tokens.unwrap(),
                        100
                    );
                    saw_usage = true;
                }
                StreamEvent::MessageStop { .. } => {
                    assert!(saw_cost && saw_usage);
                }
                _ => {}
            }
        }
        assert!(saw_cost && saw_usage);
    }

    #[test]
    fn chat_delta_keeps_gemini_thoughts_out_of_visible_text() {
        let reasoning: OpenAiDelta = serde_json::from_str(
            r#"{"role":"assistant","content":null,"reasoning_content":"plan the review"}"#,
        )
        .expect("reasoning_content delta");
        assert_eq!(reasoning.thinking_text(), Some("plan the review"));
        assert_eq!(reasoning.visible_text(), None);

        let tagged: OpenAiDelta = serde_json::from_str(
            r#"{"content":"hidden chain of thought","extra_content":{"google":{"thought":true}}}"#,
        )
        .expect("google thought delta");
        assert_eq!(tagged.thinking_text(), Some("hidden chain of thought"));
        assert_eq!(tagged.visible_text(), None);

        let answer: OpenAiDelta =
            serde_json::from_str(r#"{"content":"{\"escalate_to\":\"none\"}"}"#)
                .expect("visible content delta");
        assert_eq!(answer.thinking_text(), None);
        assert_eq!(answer.visible_text(), Some("{\"escalate_to\":\"none\"}"));
    }

    #[tokio::test]
    async fn vertex_chat_thought_only_stream_emits_thinking_and_usage() {
        let client = client_with_responses_sse(concat!(
            r#"data: {"id":"thought-1","object":"chat.completion.chunk","created":1,"model":"google/gemini-2.5-pro","choices":[{"index":0,"delta":{"reasoning_content":"weigh the diff"},"finish_reason":null}]}"#,
            "\n\n",
            r#"data: {"id":"thought-1","object":"chat.completion.chunk","created":1,"model":"google/gemini-2.5-pro","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "\n\n",
            r#"data: {"id":"thought-1","object":"chat.completion.chunk","created":1,"model":"google/gemini-2.5-pro","choices":[],"usage":{"prompt_tokens":21853,"completion_tokens":390}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        ));
        let mut stream = client
            .stream(
                &[],
                &RequestConfig {
                    model: "google/gemini-2.5-pro".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut thinking = String::new();
        let mut saw_text = false;
        let mut output_tokens = None;
        while let Some(event) = stream.recv().await {
            match event {
                StreamEvent::ThinkingDelta {
                    thinking: delta, ..
                } => thinking.push_str(&delta),
                StreamEvent::TextDelta { .. } => saw_text = true,
                StreamEvent::Usage {
                    output_tokens: n, ..
                } => output_tokens = Some(n),
                _ => {}
            }
        }
        assert_eq!(thinking, "weigh the diff");
        assert!(
            !saw_text,
            "thought-only Vertex chunks must not become assistant text"
        );
        assert_eq!(output_tokens, Some(390));
    }

    fn budget_authorization(budget: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "claims": {
                "model": "openai/gpt-5.6-terra",
                "providerCandidates": [{
                    "provider": "openai",
                    "environment": "production",
                    "credentialName": "default",
                    "teamId": "",
                    "model": "openai/gpt-5.6-terra"
                }],
                "routing": {"strategy": "ordered"},
                "output_token_budget": budget
            }
        })
    }

    // llm-gateway refuses a provider-default budget that arrives with a payload
    // limit ("output_token_budget_mismatch"), which is what broke every Dex
    // plain_answer turn: the body builder sets max_output_tokens from this
    // client's model capabilities, which cannot know the authorization.
    #[test]
    fn provider_default_budget_drops_the_payload_output_limit() {
        let authorization = budget_authorization(serde_json::json!({
            "origin": "provider_default",
            "origin_reference": ""
        }));
        let mut request = serde_json::json!({
            "model": "client-chosen",
            "max_output_tokens": 128_000,
            "max_tokens": 128_000
        })
        .as_object()
        .expect("request object")
        .clone();

        project_managed_inference_route(&mut request, &authorization)
            .expect("project authorized route");

        assert!(request.get("max_output_tokens").is_none());
        assert!(request.get("max_tokens").is_none());
        assert_eq!(request["output_token_budget"]["origin"], "provider_default");
    }

    // The other half of the same contract: an explicit budget must equal the
    // payload limit, so the authorized value wins over the client's own.
    #[test]
    fn explicit_budget_rewrites_the_payload_output_limit() {
        let authorization = budget_authorization(serde_json::json!({
            "value": 4096,
            "origin": "explicit",
            "origin_reference": "turn"
        }));
        let mut request = serde_json::json!({
            "model": "client-chosen",
            "max_output_tokens": 128_000
        })
        .as_object()
        .expect("request object")
        .clone();

        project_managed_inference_route(&mut request, &authorization)
            .expect("project authorized route");

        assert_eq!(request["max_output_tokens"], 4096);
        // Only the key the body builder chose is rewritten.
        assert!(request.get("max_tokens").is_none());
    }

    fn managed_authorization_fixture(lineage_id: &str) -> String {
        serde_json::json!({
            "claims": {
                "endpoint": "responses",
                "schema_version": 1,
                "authorization_id": "auth-1",
                "key_id": "key-1",
                "audience": "evalops.llm-gateway",
                "issued_at_ms": 1_700_000_000_000_i64,
                "not_before_ms": 1_700_000_000_000_i64,
                "expires_at_ms": 1_700_000_060_000_i64,
                "grant_epoch": 7,
                "organization_id": "org_123",
                "workspace_id": "workspace_456",
                "session_id": "session-1",
                "thread_id": "thread-1",
                "run_id": "run-1",
                "turn_id": "turn-1",
                "lineage_id": lineage_id,
                "runtime_generation": 3,
                "endpoint": "responses",
                "model": "openai/gpt-5.6-terra",
                "providerCandidates": [{
                    "provider": "openai",
                    "environment": "production",
                    "credentialName": "default",
                    "teamId": "",
                    "model": "openai/gpt-5.6-terra"
                }],
                "routing": {
                    "strategy": "ordered",
                    "max_attempts": 1
                },
                "output_token_budget": {
                    "value": 4096,
                    "origin": "explicit",
                    "origin_reference": "turn"
                },
                "scope_digest": "scope-digest"
            },
            "signature": "signature-marker"
        })
        .to_string()
    }

    fn read_complete_http_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;

        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set managed request timeout");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = stream.read(&mut chunk).expect("read managed request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers =
                std::str::from_utf8(&request[..header_end]).expect("managed request headers");
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or_default();
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(request).expect("managed request is UTF-8")
    }

    fn captured_request_headers(request: &str) -> std::collections::HashMap<String, String> {
        let (headers, _) = request
            .split_once("\r\n\r\n")
            .expect("captured request has headers");
        headers
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect()
    }

    fn captured_request_body(request: &str) -> serde_json::Value {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("captured request has a body");
        serde_json::from_str(body).expect("captured request body is JSON")
    }

    fn managed_gateway_test_server(
        sse_body: &str,
        receipt_headers: &[(&str, &str)],
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock managed Gateway");
        let address = listener.local_addr().expect("managed Gateway address");
        let sse_body = sse_body.to_string();
        let receipt_headers = receipt_headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect::<Vec<_>>();
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept managed Gateway request");
            let request = read_complete_http_request(&mut stream);
            request_tx
                .send(request)
                .expect("capture managed Gateway request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
                sse_body.len()
            )
            .expect("write managed Gateway response headers");
            for (name, value) in receipt_headers {
                write!(stream, "{name}: {value}\r\n").expect("write managed receipt header");
            }
            write!(stream, "\r\n{sse_body}").expect("write managed Gateway response body");
        });
        (format!("http://{address}/v1"), request_rx)
    }

    fn managed_gateway_test_client(
        sse_body: &str,
        receipt_headers: &[(&str, &str)],
    ) -> (OpenAiClient, std::sync::mpsc::Receiver<String>) {
        let (base_url, request_rx) = managed_gateway_test_server(sse_body, receipt_headers);
        let client = OpenAiClient::with_base_url("delegated-token", base_url)
            .expect("construct managed Gateway client")
            .with_route_provider("openai")
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default"
                }),
            )
            .expect("attach managed Gateway scope");
        (client, request_rx)
    }

    fn managed_gateway_error_client(status: &str, body: &str) -> OpenAiClient {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock managed Gateway");
        let address = listener.local_addr().expect("managed Gateway address");
        let status = status.to_owned();
        let body = body.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept managed Gateway request");
            let _request = read_complete_http_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            )
            .expect("write managed Gateway error response");
        });

        OpenAiClient::with_base_url("delegated-token", format!("http://{address}/v1"))
            .expect("construct managed Gateway client")
            .with_route_provider("openai")
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default"
                }),
            )
            .expect("attach managed Gateway scope")
    }

    async fn collect_stream_events(
        mut receiver: mpsc::UnboundedReceiver<StreamEvent>,
    ) -> Vec<StreamEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let mut events = Vec::new();
            while let Some(event) = receiver.recv().await {
                events.push(event);
            }
            events
        })
        .await
        .expect("managed stream should terminate")
    }

    fn managed_receipt_headers() -> [(&'static str, &'static str); 4] {
        [
            ("X-Request-ID", "request-1"),
            ("x-evalops-record-id", "record-1"),
            ("X-EVALOPS-LINEAGE-ID", "lineage-receipt"),
            ("x-EvalOps-Record-Status", "planned"),
        ]
    }

    const MANAGED_COMPLETED_SSE: &str = r#"data: {"type":"response.created","response":{"id":"resp_managed"}}

data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":1}}}

"#;

    #[tokio::test]
    async fn managed_endpoint_selects_url_body_and_stream_parser_over_catalog() {
        for (endpoint, model, sse, path, field) in [
            (
                "responses",
                "gemini-3.6-flash",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
                "/v1/responses",
                "input",
            ),
            (
                "chat.completions",
                "gpt-5.6",
                "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
                "/v1/chat/completions",
                "messages",
            ),
        ] {
            let (mut client, request_rx) =
                managed_gateway_test_client(sse, &managed_receipt_headers());
            let mut authorization: serde_json::Value =
                serde_json::from_str(&managed_authorization_fixture("lineage-receipt")).unwrap();
            authorization["claims"]["endpoint"] = endpoint.into();
            client.set_managed_inference_authorization(Some(authorization.to_string()));
            let stream = client
                .stream(
                    &[],
                    &RequestConfig {
                        model: model.into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let events = collect_stream_events(stream).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::ProviderError { .. })),
                "{events:?}"
            );
            assert!(
                events.iter().any(
                    |event| matches!(event, StreamEvent::TextDelta { text, .. } if text == "ok")
                ),
                "authorized parser did not emit content: {events:?}"
            );
            let request = request_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                request.starts_with(&format!("POST {path} ")),
                "wrong authorized endpoint"
            );
            let body = captured_request_body(&request);
            assert!(body[field].is_array());
            assert!(
                body.get(if field == "input" {
                    "messages"
                } else {
                    "input"
                })
                .is_none()
            );
            assert_eq!(body["managed_inference_authorization"], authorization);
            let topology: maestro_runtime_contracts::cache_topology::HostedCacheTopology =
                serde_json::from_value(body["cache_topology"].clone()).unwrap();
            topology
                .topology
                .validate(
                    &maestro_runtime_contracts::cache_topology::wire_shape(
                        &body,
                        topology.scope.namespace(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
    }

    #[tokio::test]
    async fn managed_endpoint_rejects_unsupported_or_missing_scope_before_network() {
        for endpoint in [
            serde_json::Value::Null,
            "embeddings".into(),
            " responses".into(),
            "chat/completions".into(),
        ] {
            let mut client = OpenAiClient::with_base_url("test", "http://127.0.0.1:1/v1")
                .unwrap()
                .with_managed_gateway_scope(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({"provider":"openai","environment":"production"}),
                )
                .unwrap();
            let mut authorization: serde_json::Value =
                serde_json::from_str(&managed_authorization_fixture("lineage-receipt")).unwrap();
            authorization["claims"]["endpoint"] = endpoint;
            client.set_managed_inference_authorization(Some(authorization.to_string()));
            let error = client
                .stream(
                    &[],
                    &RequestConfig {
                        model: "gpt-5.6".into(),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("reject endpoint");
            assert!(
                error
                    .to_string()
                    .contains("unsupported or missing endpoint"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn managed_endpoint_rejects_conflicting_body_extension_before_network() {
        let mut client = OpenAiClient::with_base_url("test", "http://127.0.0.1:1/v1")
            .unwrap()
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({"provider":"openai","environment":"production"}),
            )
            .unwrap();
        client.set_managed_inference_authorization(Some(managed_authorization_fixture(
            "lineage-receipt",
        )));
        client
            .request_extensions
            .insert("messages".into(), serde_json::json!([]));
        let error = client
            .stream(
                &[],
                &RequestConfig {
                    model: "gemini-3.6-flash".into(),
                    ..Default::default()
                },
            )
            .await
            .expect_err("reject mismatched shape");
        assert!(
            error
                .to_string()
                .contains("does not match authorized endpoint"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn managed_request_sends_authorization_and_workspace_scope() {
        let authorization = managed_authorization_fixture("lineage-explicit");
        let (base_url, captured_request) =
            managed_gateway_test_server(MANAGED_COMPLETED_SSE, &managed_receipt_headers());
        let client = crate::openai::OpenAiClient::with_base_url("delegated-token", base_url)
            .expect("construct wrapped managed Gateway client")
            .with_route_provider("openai")
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default"
                }),
            )
            .expect("attach wrapped managed Gateway scope");
        let mut client = UnifiedClient::OpenAI(client);
        client.set_managed_request_lineage(Some("lineage-explicit".to_string()));
        client.set_managed_inference_authorization(Some(authorization.clone()));

        let receiver = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("start managed request");
        let events = collect_stream_events(receiver).await;
        let request = captured_request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("capture managed request");
        let headers = captured_request_headers(&request);
        let body = captured_request_body(&request);

        assert_eq!(headers["x-organization-id"], "org_123");
        assert_eq!(headers["x-workspace-id"], "workspace_456");
        assert_eq!(body["lineage_id"], "lineage-explicit");
        let topology: maestro_runtime_contracts::cache_topology::HostedCacheTopology =
            serde_json::from_value(body["cache_topology"].clone()).expect("typed cache contract");
        assert_eq!(topology.scope.organization_id, "org_123");
        assert_eq!(topology.scope.workspace_id, "workspace_456");
        topology
            .topology
            .validate(
                &maestro_runtime_contracts::cache_topology::wire_shape(
                    &body,
                    topology.scope.namespace(),
                )
                .unwrap(),
            )
            .unwrap();

        assert_eq!(
            body["managed_inference_authorization"],
            serde_json::from_str::<serde_json::Value>(&authorization)
                .expect("authorization fixture is JSON")
        );
        assert_eq!(
            body["managed_inference_context"],
            serde_json::json!({
                "session_id": "session-1",
                "thread_id": "thread-1",
                "run_id": "run-1",
                "turn_id": "turn-1"
            })
        );
        assert_eq!(
            body["provider_candidates"],
            serde_json::json!([{
                "model": "openai/gpt-5.6-terra",
                "provider_ref": {
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default",
                    "team_id": ""
                }
            }])
        );
        assert!(body.get("provider_ref").is_none());
        assert!(
            !format!("{events:?}").contains("signature-marker"),
            "stream events must not expose the opaque authorization"
        );
    }

    #[test]
    fn astra_request_output_limit_matches_advertised_capability() {
        let config = RequestConfig {
            model: "gpt-6-astra".into(),
            max_tokens: 200_000,
            ..Default::default()
        };
        let client = OpenAiClient::new("test-key").unwrap();
        let body = client.build_responses_request_body(&[], &config).unwrap();
        assert_eq!(body["max_output_tokens"], 128_000);
    }

    #[test]
    fn managed_provider_candidate_projection_preserves_signed_values() {
        let projected = project_managed_provider_candidates(&serde_json::json!([{
            "provider": "OpenAI",
            "environment": "production-us",
            "credentialName": "team/shared",
            "teamId": "team-1",
            "model": "openai/gpt-5.6-terra",
        }]))
        .expect("signed candidate values should project without normalization");

        assert_eq!(
            projected,
            serde_json::json!([{
                "model": "openai/gpt-5.6-terra",
                "provider_ref": {
                    "provider": "OpenAI",
                    "environment": "production-us",
                    "credential_name": "team/shared",
                    "team_id": "team-1",
                }
            }])
        );
    }

    #[test]
    fn managed_provider_candidate_projection_rejects_noncanonical_signed_values() {
        let error = project_managed_provider_candidates(&serde_json::json!([{
            "provider": "openai",
            "environment": "production",
            "credential_name": "default",
            "team_id": "team-1",
            "model": " openai/gpt-5.6-terra",
        }]))
        .expect_err("surrounding whitespace would fail Gateway scope admission");

        assert!(
            error
                .to_string()
                .contains("model must not have surrounding whitespace")
        );
    }

    #[test]
    fn managed_request_projects_the_authorized_route_bundle() {
        let authorization = managed_authorization_fixture("lineage-route");
        let authorization_json =
            serde_json::from_str::<serde_json::Value>(&authorization).expect("authorization JSON");
        let mut client = OpenAiClient::with_base_url("delegated-token", "http://127.0.0.1:9/v1")
            .expect("construct managed Gateway client")
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default"
                }),
            )
            .expect("attach managed Gateway scope");
        client.set_managed_inference_authorization(Some(authorization));

        let request = client
            .managed_request(client.build_request_body(
                &[],
                &RequestConfig {
                    model: "openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            ))
            .expect("project authorized route");

        assert_eq!(
            request["provider_candidates"],
            serde_json::json!([{
                "model": "openai/gpt-5.6-terra",
                "provider_ref": {
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default",
                    "team_id": ""
                }
            }])
        );
        assert_eq!(request["model"], "openai/gpt-5.6-terra");
        assert_eq!(request["routing_strategy"], "ordered");
        assert_eq!(
            request["output_token_budget"],
            authorization_json["claims"]["output_token_budget"]
        );
        assert!(
            request.get("provider_ref").is_none(),
            "the signed candidate set must replace the unsigned fallback provider"
        );
        assert!(request.get("provider_refs").is_none());
    }

    #[tokio::test]
    async fn managed_request_derives_lineage_from_authorization_when_not_explicit() {
        let authorization = managed_authorization_fixture("lineage-derived");
        let (mut client, captured_request) =
            managed_gateway_test_client(MANAGED_COMPLETED_SSE, &managed_receipt_headers());
        client.set_managed_inference_authorization(Some(authorization));

        let receiver = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("start managed request");
        let _events = collect_stream_events(receiver).await;
        let request = captured_request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("capture managed request");
        let body = captured_request_body(&request);

        assert_eq!(body["lineage_id"], "lineage-derived");
    }

    #[test]
    fn managed_request_rejects_conflicting_explicit_and_authorized_lineage() {
        let mut client = OpenAiClient::with_base_url("delegated-token", "http://127.0.0.1:9/v1")
            .expect("construct managed Gateway client")
            .with_managed_gateway_scope(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider": "openai",
                    "environment": "production",
                    "credential_name": "default"
                }),
            )
            .expect("attach managed Gateway scope");
        client.set_managed_request_lineage(Some("lineage-explicit".to_string()));
        client.set_managed_inference_authorization(Some(managed_authorization_fixture(
            "lineage-authorized",
        )));

        let error = client
            .managed_request(serde_json::json!({}))
            .expect_err("conflicting lineage IDs must fail before sending");

        assert!(error.to_string().contains(
            "managed request lineage does not match managed inference authorization lineage"
        ));
        assert!(!error.to_string().contains("signature-marker"));
    }

    #[tokio::test]
    async fn managed_request_rejects_authorization_without_lineage_before_sending() {
        for authorization in [
            {
                let mut authorization = serde_json::from_str::<serde_json::Value>(
                    &managed_authorization_fixture("lineage-present"),
                )
                .expect("authorization fixture");
                authorization["claims"]
                    .as_object_mut()
                    .expect("authorization claims")
                    .remove("lineage_id");
                authorization.to_string()
            },
            managed_authorization_fixture(""),
        ] {
            let mut client =
                OpenAiClient::with_base_url("delegated-token", "http://127.0.0.1:9/v1")
                    .expect("construct managed Gateway client")
                    .with_managed_gateway_scope(
                        "org_123",
                        "workspace_456",
                        serde_json::json!({
                            "provider": "openai",
                            "environment": "production",
                            "credential_name": "default"
                        }),
                    )
                    .expect("attach managed Gateway scope");
            client.set_managed_inference_authorization(Some(authorization));

            let error = match client
                .stream(
                    &[],
                    &RequestConfig {
                        model: "evalops/openai/gpt-5.6-terra".to_string(),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => panic!("authorization without lineage must fail before sending"),
                Err(error) => error,
            };

            assert!(
                error
                    .to_string()
                    .contains("managed inference authorization is missing lineage_id")
            );
            assert!(!error.to_string().contains("signature-marker"));
        }
    }

    #[test]
    fn managed_tools_evidence_requires_valid_complete_pair() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for (hash, count, valid) in [
            (digest.as_str(), "3", true),
            (digest.as_str(), "0", true),
            ("sha256:bad", "3", false),
            (digest.as_str(), "-1", false),
            (digest.as_str(), "4294967296", false),
            (digest.as_str(), "+3", false),
        ] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("x-evalops-provider-tools-sha256", hash.parse().unwrap());
            headers.insert("x-evalops-provider-tool-count", count.parse().unwrap());
            assert_eq!(managed_provider_tools_evidence(&headers).is_some(), valid);
            headers.remove("x-evalops-provider-tool-count");
            assert!(managed_provider_tools_evidence(&headers).is_none());
        }
    }

    #[tokio::test]
    async fn managed_tools_receipt_preserves_provider_attestation() {
        let digest = format!("sha256:{}", "b".repeat(64));
        let mut headers = managed_receipt_headers().to_vec();
        headers.push(("x-evalops-provider-tools-sha256", digest.as_str()));
        headers.push(("x-evalops-provider-tool-count", "2"));
        let (mut client, captured) = managed_gateway_test_client(MANAGED_COMPLETED_SSE, &headers);
        client.set_managed_inference_authorization(Some(managed_authorization_fixture(
            "lineage-receipt",
        )));
        let mut events = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".into(),
                    ..Default::default()
                },
            )
            .await
            .expect("stream");
        let Some(StreamEvent::ManagedGatewayReceipt(receipt)) = events.recv().await else {
            panic!("receipt precedes content")
        };
        assert_eq!(
            receipt.provider_tools_sha256.as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(receipt.provider_tool_count, Some(2));
        assert!(
            captured
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn managed_prompt_exposure_requires_gateway_attestation_of_exact_request() {
        for prompt in [
            "base",
            "base\n\nverified completion",
            "base\n\ndynamic context",
        ] {
            for matches in [true, false] {
                let digest = format!(
                    "sha256:{:x}",
                    Sha256::digest(if matches { prompt } else { "stale" }.as_bytes())
                );
                let mut headers = managed_receipt_headers().to_vec();
                headers.push(("x-evalops-provider-prompt-sha256", digest.as_str()));
                let (mut client, captured) =
                    managed_gateway_test_client(MANAGED_COMPLETED_SSE, &headers);
                client.set_managed_inference_authorization(Some(managed_authorization_fixture(
                    "lineage-receipt",
                )));
                let mut events = client
                    .stream(
                        &[],
                        &RequestConfig {
                            model: "evalops/openai/gpt-5.6-terra".into(),
                            system: Some(prompt.into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("stream");
                let Some(StreamEvent::ManagedGatewayReceipt(receipt)) = events.recv().await else {
                    panic!("receipt must precede content");
                };
                assert_eq!(
                    receipt.provider_prompt_sha256.as_deref(),
                    matches.then_some(digest.as_str())
                );
                let captured = captured
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("captured wire request");
                let (_, body) = captured.split_once("\r\n\r\n").expect("HTTP body");
                let body: serde_json::Value = serde_json::from_str(body).expect("JSON body");
                assert_eq!(body["instructions"], prompt);
            }
        }
    }

    #[tokio::test]
    async fn managed_stream_emits_receipt_headers_before_provider_content() {
        let authorization = managed_authorization_fixture("lineage-receipt");
        let (mut client, _captured_request) =
            managed_gateway_test_client(MANAGED_COMPLETED_SSE, &managed_receipt_headers());
        client.set_managed_inference_authorization(Some(authorization));

        let mut receiver = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("start managed stream");
        let first = receiver.recv().await.expect("managed receipt event");

        assert!(matches!(
            &first,
            StreamEvent::ManagedGatewayReceipt(receipt)
                if receipt.request_id == "request-1"
                    && receipt.record_id == "record-1"
                    && receipt.lineage_id == "lineage-receipt"
                    && receipt.record_status == "planned"
        ));
        assert!(
            !format!("{first:?}").contains("signature-marker"),
            "receipt must contain only safe identifiers"
        );
        assert!(matches!(
            receiver.recv().await,
            Some(StreamEvent::MessageStart { .. })
        ));
    }

    #[tokio::test]
    async fn managed_stream_rejects_missing_receipt_headers_before_provider_content() {
        let incomplete_headers = [
            ("X-Request-ID", "request-1"),
            ("x-evalops-record-id", "record-1"),
            ("X-EVALOPS-LINEAGE-ID", "lineage-receipt"),
        ];
        let (mut client, _captured_request) =
            managed_gateway_test_client(MANAGED_COMPLETED_SSE, &incomplete_headers);
        client.set_managed_inference_authorization(Some(managed_authorization_fixture(
            "lineage-receipt",
        )));

        let receiver = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("managed receipt failure is a stream event");
        let events = collect_stream_events(receiver).await;

        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message,
            }] if message.contains("x-evalops-record-status")
                && !message.contains("signature-marker")
        ));
    }

    #[tokio::test]
    async fn managed_error_status_without_receipts_preserves_http_classification() {
        let cases = [
            (
                "401 Unauthorized",
                r#"{"error":{"message":"invalid delegated token"}}"#,
                ProviderStreamErrorKind::ProviderDeclaredFailure,
                "invalid delegated token",
            ),
            (
                "429 Too Many Requests",
                r#"{"error":{"message":"managed quota exhausted"}}"#,
                ProviderStreamErrorKind::TransientProtocol,
                "managed quota exhausted",
            ),
            (
                "500 Internal Server Error",
                r#"{"error":{"message":"gateway unavailable"}}"#,
                ProviderStreamErrorKind::TransientProtocol,
                "gateway unavailable",
            ),
        ];

        for (status, body, expected_kind, expected_message) in cases {
            let mut client = managed_gateway_error_client(status, body);
            client.set_managed_request_lineage(Some("lineage-error".to_string()));
            let receiver = client
                .stream(
                    &[],
                    &RequestConfig {
                        model: "evalops/openai/gpt-5.6-terra".to_string(),
                        ..Default::default()
                    },
                )
                .await
                .expect("HTTP error is represented as a stream event");
            let events = collect_stream_events(receiver).await;

            assert!(matches!(
                events.as_slice(),
                [StreamEvent::ProviderError { kind, message }]
                    if *kind == expected_kind
                        && message.contains(status)
                        && message.contains(expected_message)
                        && !message.contains("managed_gateway_receipt_error")
            ));
        }
    }

    #[tokio::test]
    async fn managed_stream_rejects_receipt_with_mismatched_request_lineage() {
        let mismatched_headers = [
            ("X-Request-ID", "request-1"),
            ("x-evalops-record-id", "record-1"),
            ("X-EVALOPS-LINEAGE-ID", "lineage-receipt"),
            ("x-EvalOps-Record-Status", "planned"),
        ];
        let (mut client, _captured_request) =
            managed_gateway_test_client(MANAGED_COMPLETED_SSE, &mismatched_headers);
        client.set_managed_request_lineage(Some("lineage-request".to_string()));

        let receiver = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("managed receipt failure is a stream event");
        let events = collect_stream_events(receiver).await;

        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message,
            }] if message.contains("receipt lineage mismatch")
        ));
    }

    #[tokio::test]
    async fn managed_stream_rejects_oversized_receipt_headers() {
        let oversized_record_id = "r".repeat(MANAGED_GATEWAY_RECEIPT_ID_MAX_LEN + 1);
        let headers = [
            ("X-Request-ID", "request-1"),
            ("x-evalops-record-id", oversized_record_id.as_str()),
            ("X-EVALOPS-LINEAGE-ID", "lineage-receipt"),
            ("x-EvalOps-Record-Status", "planned"),
        ];
        let (mut client, _captured_request) =
            managed_gateway_test_client(MANAGED_COMPLETED_SSE, &headers);
        client.set_managed_request_lineage(Some("lineage-receipt".to_string()));

        let receiver = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("managed receipt failure is a stream event");
        let events = collect_stream_events(receiver).await;

        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message,
            }] if message.contains("x-evalops-record-id header exceeds")
        ));
    }

    #[tokio::test]
    async fn managed_stream_maps_rate_limit_and_retryable_response_failed_to_transient() {
        for error in [
            serde_json::json!({
                "code": "rate_limit_exceeded",
                "message": "try again in 1s"
            }),
            serde_json::json!({
                "type": "server_error",
                "message": "provider temporarily unavailable"
            }),
        ] {
            let sse_body = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type": "response.failed",
                    "response": {"id": "resp_failed", "error": error}
                })
            );
            let (mut client, _captured_request) =
                managed_gateway_test_client(&sse_body, &managed_receipt_headers());
            client.set_managed_inference_authorization(Some(managed_authorization_fixture(
                "lineage-receipt",
            )));

            let receiver = client
                .stream(
                    &[],
                    &RequestConfig {
                        model: "evalops/openai/gpt-5.6-terra".to_string(),
                        ..Default::default()
                    },
                )
                .await
                .expect("start managed stream");
            let events = collect_stream_events(receiver).await;

            assert!(matches!(
                events.last(),
                Some(StreamEvent::ProviderError {
                    kind: ProviderStreamErrorKind::TransientProtocol,
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn managed_stream_keeps_fatal_response_failed_non_retryable() {
        for error in [
            serde_json::json!({
                "code": "context_length_exceeded",
                "message": "context is too large"
            }),
            serde_json::json!({
                "code": "insufficient_quota",
                "message": "quota exhausted"
            }),
            serde_json::json!({
                "code": "invalid_api_key",
                "message": "provider rejected the request"
            }),
        ] {
            let sse_body = format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type": "response.failed",
                    "response": {"id": "resp_failed", "error": error}
                })
            );
            let (mut client, _captured_request) =
                managed_gateway_test_client(&sse_body, &managed_receipt_headers());
            client.set_managed_inference_authorization(Some(managed_authorization_fixture(
                "lineage-receipt",
            )));

            let receiver = client
                .stream(
                    &[],
                    &RequestConfig {
                        model: "evalops/openai/gpt-5.6-terra".to_string(),
                        ..Default::default()
                    },
                )
                .await
                .expect("start managed stream");
            let events = collect_stream_events(receiver).await;

            assert!(matches!(
                events.last(),
                Some(StreamEvent::ProviderError {
                    kind: ProviderStreamErrorKind::ProviderDeclaredFailure,
                    ..
                })
            ));
        }
    }

    #[test]
    fn test_provider_detection() {
        assert_eq!(
            AiProvider::from_model("gpt-5.1-codex-max"),
            AiProvider::OpenAI
        );
        assert_eq!(AiProvider::from_model("gpt-4o"), AiProvider::OpenAI);
        assert_eq!(
            AiProvider::from_model("claude-opus-4-5-20251101"),
            AiProvider::Anthropic
        );
        assert_eq!(
            AiProvider::from_model("claude-sonnet-4-5"),
            AiProvider::Anthropic
        );
    }

    #[test]
    fn astra_uses_responses_with_tools_and_preserves_provider_routes() {
        let client = OpenAiClient::new("test-key").unwrap();
        for model in ["gpt-6-astra", "openai/gpt-6-astra", "evalops/gpt-6-astra"] {
            let config = RequestConfig {
                model: model.to_string(),
                temperature: Some(0.5),
                tools: vec![
                    Tool::new("read", "Read a file")
                        .with_schema(serde_json::json!({"type": "object"})),
                ]
                .into(),
                ..Default::default()
            };
            assert!(client.request_url(model).ends_with("/responses"), "{model}");
            let body = client.build_request_body(&[], &config);
            assert_eq!(body["model"], "gpt-6-astra");
            assert_eq!(body["tools"][0]["type"], "function");
            assert_eq!(body["tools"][0]["name"], "read");
            assert!(body.get("temperature").is_none());
            assert!(body.get("messages").is_none());
            assert_eq!(
                body["include"],
                serde_json::json!(["reasoning.encrypted_content"])
            );
        }
        assert_eq!(AiProvider::from_model("gpt-6-astra"), AiProvider::OpenAI);
        for provider in ["llamacpp", "lmstudio", "ollama", "openrouter"] {
            assert!(!uses_responses_api(Some(provider), "gpt-6-astra"));
            if provider != "openrouter" {
                let local = OpenAiClient::with_base_url("", "http://127.0.0.1:8080/v1")
                    .unwrap()
                    .with_route_provider(provider);
                let body = local.build_request_body(
                    &[],
                    &RequestConfig {
                        model: "gpt-6-astra".to_string(),
                        temperature: Some(0.5),
                        ..Default::default()
                    },
                );
                assert_eq!(body["temperature"], serde_json::json!(0.5));
            }
        }
        assert!(!uses_responses_api(
            Some("openrouter"),
            "openai/gpt-6-astra"
        ));
        let routed = OpenAiClient::with_base_url("test-key", "https://openrouter.ai/api/v1")
            .unwrap()
            .with_route_provider("openrouter");
        let body = routed.build_request_body(
            &[],
            &RequestConfig {
                model: "openai/gpt-6-astra".to_string(),
                temperature: Some(0.5),
                tools: vec![Tool::new("read", "Read a file")].into(),
                ..Default::default()
            },
        );
        assert_eq!(body["model"], "openai/gpt-6-astra");
        assert!(body.get("messages").is_some());
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(
            routed.request_url("openai/gpt-6-astra"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn test_uses_responses_api() {
        // gpt-5.1-codex-* should use Responses API
        assert!(uses_responses_api(None, "gpt-5.1-codex-max"));
        assert!(uses_responses_api(None, "openai/gpt-5.1-codex-max"));
        assert!(uses_responses_api(None, "gpt-5.1-codex-lite"));
        // o3 models use Responses API
        assert!(uses_responses_api(None, "o3"));
        assert!(uses_responses_api(None, "o3-mini"));
        // Other models should not
        assert!(!uses_responses_api(None, "gpt-4o"));
        assert!(!uses_responses_api(None, "gpt-4-turbo"));
        assert!(!uses_responses_api(None, "o1"));
    }

    #[test]
    fn native_local_providers_always_use_chat_completions() {
        for provider in ["llamacpp", "lmstudio", "ollama"] {
            for model in ["gpt-5-local", "o3-local"] {
                assert!(
                    !uses_responses_api(Some(provider), model),
                    "native local provider must use Chat Completions: {provider}/{model}"
                );
            }
        }

        assert!(uses_responses_api(Some("openai"), "gpt-5-local"));
        assert!(uses_responses_api(Some("openai"), "o3-local"));
    }

    #[test]
    fn openrouter_models_use_stable_chat_completions_except_plain_gpt_5_6() {
        for model in [
            "openrouter/anthropic/claude-sonnet-4.5",
            "openrouter/google/gemini-2.5-pro",
            "openrouter/meta-llama/llama-4-maverick",
            "openrouter/openai/gpt-5.5",
            "openrouter/openai/o3-mini",
            "openrouter/openrouter/auto",
            "evalops/openrouter/gpt-5.6",
            "openrouter/gpt-5.6-terra",
            "openrouter/openai/gpt-5.6-terra",
        ] {
            assert!(
                !uses_responses_api(None, model),
                "OpenRouter model must use Chat Completions: {model}"
            );
        }
        assert!(uses_responses_api(None, "openrouter/gpt-5.6"));
        assert!(uses_responses_api(
            Some("openrouter"),
            "evalops/openai/gpt-5.6"
        ));
        assert!(!uses_responses_api(
            Some("openrouter"),
            "evalops/openai/gpt-5.6-terra"
        ));
        assert!(!uses_responses_api(
            Some("openrouter"),
            "evalops/anthropic/claude-sonnet-4.5"
        ));
        assert!(!uses_responses_api(
            Some("openrouter"),
            "evalops/openrouter/gpt-5.6"
        ));
    }

    #[test]
    fn test_api_url_selection() {
        // Responses API models go to /v1/responses
        assert_eq!(
            api_url_for_model("gpt-5.1-codex-max"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            api_url_for_model("openai/gpt-5.1-codex-max"),
            "https://api.openai.com/v1/responses"
        );
        // Chat Completions models go to /v1/chat/completions
        assert_eq!(
            api_url_for_model("gpt-4o"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_request_url_honors_custom_base_url() {
        // Default client (no base URL) targets OpenAI.
        let openai = OpenAiClient::new("k").unwrap();
        assert_eq!(
            openai.request_url("gpt-4o"),
            "https://api.openai.com/v1/chat/completions"
        );

        // Custom OpenAI-compatible providers must hit their own endpoint, not
        // OpenAI. A trailing slash on the base URL is tolerated.
        let deepseek = OpenAiClient::with_base_url("k", "https://api.deepseek.com/v1").unwrap();
        assert_eq!(
            deepseek.request_url("deepseek-chat"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        let zai = OpenAiClient::with_base_url("k", "https://api.z.ai/api/coding/paas/v4/").unwrap();
        assert_eq!(
            zai.request_url("glm-4.6"),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
        let groq = OpenAiClient::with_base_url("k", "https://api.groq.com/openai/v1").unwrap();
        assert_eq!(
            groq.request_url("llama-3.3-70b-versatile"),
            "https://api.groq.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn test_has_incompatible_schema() {
        // Simple schema is compatible
        let simple = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        assert!(!has_incompatible_schema(&simple));

        // oneOf is incompatible
        let one_of = serde_json::json!({
            "oneOf": [
                {"type": "string"},
                {"type": "number"}
            ]
        });
        assert!(has_incompatible_schema(&one_of));

        // anyOf is incompatible
        let any_of = serde_json::json!({
            "anyOf": [
                {"type": "string"},
                {"type": "number"}
            ]
        });
        assert!(has_incompatible_schema(&any_of));

        // allOf is incompatible
        let all_of = serde_json::json!({
            "allOf": [
                {"type": "object"},
                {"properties": {"x": {"type": "number"}}}
            ]
        });
        assert!(has_incompatible_schema(&all_of));

        // Top-level enum is incompatible
        let top_enum = serde_json::json!({
            "enum": ["a", "b", "c"]
        });
        assert!(has_incompatible_schema(&top_enum));
    }

    #[test]
    fn test_filter_responses_api_tools() {
        let tools = vec![
            Tool::new("read", "Read a file").with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            })),
            Tool::new("", "Empty name tool") // Should be filtered out
                .with_schema(serde_json::json!({})),
            Tool::new("bad", "Has oneOf").with_schema(serde_json::json!({
                "oneOf": [{"type": "string"}, {"type": "number"}]
            })),
        ];

        let filtered = filter_responses_api_tools(&tools);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "read");
    }

    #[test]
    fn responses_tool_schema_prefer_supported_is_strict() {
        let client = OpenAiClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "gpt-5.6".into(),
            tools: vec![
                Tool::new("read", "Read a file")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["path"]
                    }))
                    .with_schema_enforcement(ToolSchemaEnforcement::Prefer),
            ]
            .into(),
            ..RequestConfig::default()
        };

        let body = client
            .build_responses_request_body(&[], &config)
            .expect("supported strict schema");
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(
            body["tools"][0]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(
            body["tools"][0]["parameters"]["required"],
            serde_json::json!(["limit", "path"])
        );
    }

    #[test]
    fn responses_tool_schema_prefer_unsupported_falls_back_without_dropping_tool() {
        let client = OpenAiClient::new("test-key").unwrap();
        let original = serde_json::json!({
            "type": "object",
            "properties": {
                "value": {"$ref": "#/$defs/value"}
            }
        });
        let config = RequestConfig {
            model: "gpt-5.6".into(),
            tools: vec![
                Tool::new("lookup", "Look up a value")
                    .with_schema(original.clone())
                    .with_schema_enforcement(ToolSchemaEnforcement::Prefer),
            ]
            .into(),
            ..RequestConfig::default()
        };

        let body = client
            .build_responses_request_body(&[], &config)
            .expect("prefer must fall back");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tools"][0]["parameters"], original);
    }

    #[test]
    fn responses_tool_schema_off_preserves_existing_wire_shape() {
        let client = OpenAiClient::new("test-key").unwrap();
        let original = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}}
        });
        let config = RequestConfig {
            model: "gpt-5.6".into(),
            tools: vec![Tool::new("read", "Read a file").with_schema(original.clone())].into(),
            ..RequestConfig::default()
        };

        let body = client
            .build_responses_request_body(&[], &config)
            .expect("off is infallible");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tools"][0]["parameters"], original);
    }

    #[test]
    fn responses_tool_schema_require_unsupported_fails_request_preparation() {
        let client = OpenAiClient::new("test-key").unwrap();
        let config = RequestConfig {
            model: "gpt-5.6".into(),
            tools: vec![
                Tool::new("lookup", "Look up a value")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {"value": {"oneOf": [{"type": "string"}]}}
                    }))
                    .with_schema_enforcement(ToolSchemaEnforcement::Require),
            ]
            .into(),
            ..RequestConfig::default()
        };

        let error = client
            .build_responses_request_body(&[], &config)
            .expect_err("required strict schema must fail closed");
        assert!(error.to_string().contains("lookup"));
        assert!(error.to_string().contains("oneOf"));
    }

    #[test]
    fn trims_api_key_before_building_headers() {
        let client = OpenAiClient::new("  test-key\n").unwrap();
        let headers = client.headers();

        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer test-key");
    }

    #[test]
    fn anonymous_compatible_client_omits_authorization_header() {
        let client = OpenAiClient::with_base_url("", "http://127.0.0.1:8080/v1").unwrap();

        assert!(!client.headers().contains_key(AUTHORIZATION));
    }

    #[test]
    fn strips_provider_prefix_from_request_body_model() {
        let client = OpenAiClient::new("test-key").unwrap();
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let responses_body = client.build_request_body(
            &messages,
            &RequestConfig {
                model: "openai/gpt-5.1-codex-max".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(responses_body["model"], "gpt-5.1-codex-max");

        let chat_body = client.build_request_body(
            &messages,
            &RequestConfig {
                model: "openai/gpt-4o".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(chat_body["model"], "gpt-4o");
    }

    #[test]
    fn llama_cpp_requests_enable_prompt_cache_without_leaking_to_other_providers() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];
        let config = RequestConfig {
            model: "llamacpp/Qwen3.8-27B".to_string(),
            ..Default::default()
        };
        let llama = OpenAiClient::with_base_url("", "http://127.0.0.1:8080/v1")
            .unwrap()
            .with_route_provider("llamacpp");
        let openai = OpenAiClient::new("test-key").unwrap();

        assert_eq!(
            llama.build_request_body(&messages, &config)["cache_prompt"],
            true
        );
        assert!(
            openai
                .build_request_body(&messages, &config)
                .get("cache_prompt")
                .is_none()
        );
    }

    #[test]
    fn qwen38_maps_high_thinking_to_supported_xhigh_effort() {
        let client = OpenAiClient::with_base_url("", "http://127.0.0.1:8080/v1")
            .unwrap()
            .with_route_provider("llamacpp");
        let messages = vec![];
        let config = RequestConfig {
            model: "llamacpp/Qwen3.8-27B".to_string(),
            thinking: Some(crate::types::ThinkingConfig::enabled(12_000)),
            ..Default::default()
        };
        let other = RequestConfig {
            model: "llamacpp/another-reasoning-model".to_string(),
            ..config.clone()
        };

        assert_eq!(
            client.build_request_body(&messages, &config)["reasoning_effort"],
            "xhigh"
        );
        assert_eq!(
            client.build_request_body(&messages, &other)["reasoning_effort"],
            "high"
        );

        for provider in ["lmstudio", "ollama", "openai"] {
            let other_provider =
                OpenAiClient::with_base_url("test-key", "http://127.0.0.1:1234/v1")
                    .unwrap()
                    .with_route_provider(provider);
            assert_eq!(
                other_provider.build_request_body(&messages, &config)["reasoning_effort"],
                "high",
                "{provider} must retain the standard reasoning-effort contract"
            );
        }
    }

    #[test]
    fn namespaced_qwen38_maps_high_thinking_to_supported_xhigh_effort() {
        let client = OpenAiClient::with_base_url("", "http://127.0.0.1:8080/v1")
            .unwrap()
            .with_route_provider("llamacpp");
        let config = RequestConfig {
            model: "llamacpp/Qwen/Qwen3.8-27B".to_owned(),
            thinking: Some(crate::types::ThinkingConfig::enabled(12_000)),
            ..Default::default()
        };

        let body = client.build_request_body(&[], &config);

        assert_eq!(body["reasoning_effort"], "xhigh");
    }

    #[test]
    fn openrouter_nested_model_ids_remain_opaque_on_chat_completions() {
        let client = OpenAiClient::with_base_url("test-key", "https://openrouter.example/v1")
            .unwrap()
            .with_route_provider("openrouter");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];
        let config = RequestConfig {
            model: "openrouter/anthropic/claude-sonnet-4.5".to_string(),
            ..Default::default()
        };

        let body = client.build_request_body(&messages, &config);

        assert_eq!(body["model"], "anthropic/claude-sonnet-4.5");
        assert!(body["messages"].is_array());
        assert!(body.get("input").is_none());
        assert_eq!(
            client.request_url(&config.model),
            "https://openrouter.example/v1/chat/completions"
        );
    }

    #[test]
    fn openrouter_normalized_model_ids_keep_the_vendor_namespace() {
        let client = OpenAiClient::with_base_url("test-key", "https://openrouter.example/v1")
            .unwrap()
            .with_route_provider("openrouter");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let chat_body = client.build_request_body(
            &messages,
            &RequestConfig {
                model: "anthropic/claude-sonnet-4.5".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(chat_body["model"], "anthropic/claude-sonnet-4.5");

        let terra_body = client.build_request_body(
            &messages,
            &RequestConfig {
                model: "openai/gpt-5.6-terra".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(terra_body["model"], "openai/gpt-5.6-terra");
        assert!(terra_body.get("messages").is_some());
        assert!(terra_body.get("input").is_none());
        assert_eq!(
            client.request_url("openai/gpt-5.6-terra"),
            "https://openrouter.example/v1/chat/completions"
        );
    }

    #[test]
    fn openrouter_owned_model_namespace_is_not_stripped_twice() {
        let client = OpenAiClient::with_base_url("test-key", "https://openrouter.example/v1")
            .unwrap()
            .with_route_provider("openrouter");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];
        let config = RequestConfig {
            model: provider_model_name("openrouter/openrouter/auto"),
            ..Default::default()
        };

        assert_eq!(config.model, "openrouter/openrouter/auto");
        let body = client.build_request_body(&messages, &config);

        assert_eq!(body["model"], "openrouter/auto");
    }

    fn tool_call_history(calls: &[(&str, &str, serde_json::Value, &str)]) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(
                    calls
                        .iter()
                        .map(|(id, name, input, _)| ContentBlock::ToolUse {
                            id: (*id).to_string(),
                            name: (*name).to_string(),
                            input: input.clone(),
                            gemini_context: None,
                        })
                        .collect(),
                ),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(
                    calls
                        .iter()
                        .map(|(id, _, _, output)| ContentBlock::ToolResult {
                            tool_use_id: (*id).to_string(),
                            content: (*output).to_string(),
                            is_error: Some(false),
                        })
                        .collect(),
                ),
            },
        ]
    }

    #[test]
    fn handoff_reasoning_and_tool_pairs_reach_both_openai_protocols() {
        let client = OpenAiClient::new("test-key").unwrap();
        let mut history = tool_call_history(&[(
            "foreign/+id=",
            "read",
            serde_json::json!({"path":"a"}),
            "file evidence",
        )]);
        let MessageContent::Blocks(blocks) = &mut history[0].content else {
            panic!("blocks")
        };
        blocks.insert(
            0,
            ContentBlock::Thinking {
                thinking: "Claude reasoning evidence".into(),
                signature: Some("signed".into()),
            },
        );
        let before = serde_json::to_value(&history).unwrap();
        let config = RequestConfig {
            model: "gpt-5.6".into(),
            ..Default::default()
        };
        let chat = client.build_chat_request_body(&history, &config);
        assert!(chat.to_string().contains("Claude reasoning evidence"));
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["id"],
            chat["messages"][1]["tool_call_id"]
        );
        let responses = client
            .build_responses_request_body(&history, &config)
            .unwrap();
        assert!(responses.to_string().contains("Claude reasoning evidence"));
        let input = responses["input"].as_array().unwrap();
        let call = input.iter().find(|x| x["type"] == "function_call").unwrap();
        let output = input
            .iter()
            .find(|x| x["type"] == "function_call_output")
            .unwrap();
        assert_eq!(call["call_id"], output["call_id"]);
        assert_eq!(serde_json::to_value(history).unwrap(), before);
    }

    #[tokio::test]
    async fn responses_distinct_items_with_same_call_id_are_not_deduplicated() {
        let a = serde_json::json!({"type":"function_call","id":"fc_a","call_id":"shared","name":"read","arguments":"{}"});
        let b = serde_json::json!({"type":"function_call","id":"fc_b","call_id":"shared","name":"read","arguments":"{}"});
        let events = [
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":a}),
            serde_json::json!({"type":"response.output_item.done","output_index":1,"item":b}),
            serde_json::json!({"type":"response.completed","response":{"output":[a,b]}}),
        ];
        let sse = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let events = collect_responses_sse(&sse).await;
        let ids = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ContentBlockStart {
                    block: ContentBlock::ToolUse { id, .. },
                    ..
                } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, ["shared|fc_a", "shared|fc_b"]);
        let client = OpenAiClient::new("test-key").unwrap();
        let history = tool_call_history(&[
            (ids[0], "read", serde_json::json!({}), "a"),
            (ids[1], "read", serde_json::json!({}), "b"),
        ]);
        let body = client
            .build_responses_request_body(&history, &RequestConfig::default())
            .unwrap();
        assert_eq!(body["input"][0]["id"], "fc_a");
        assert_eq!(body["input"][1]["id"], "fc_b");
        assert_eq!(body["input"][2]["call_id"], "shared");
        assert_eq!(body["input"][3]["call_id"], "shared");
    }

    #[test]
    fn chat_history_keeps_single_and_parallel_tool_calls_with_their_results() {
        let client = OpenAiClient::new("test-key").unwrap();
        for calls in [
            vec![(
                "call_read",
                "read",
                serde_json::json!({"path": "Cargo.toml"}),
                "contents",
            )],
            vec![
                (
                    "call_read",
                    "read",
                    serde_json::json!({"path": "Cargo.toml"}),
                    "contents",
                ),
                (
                    "call_glob",
                    "glob",
                    serde_json::json!({"pattern": "*.rs"}),
                    "src/main.rs",
                ),
            ],
        ] {
            let body = client.build_chat_request_body(
                &tool_call_history(&calls),
                &RequestConfig {
                    model: "openai/gpt-4.1-mini".to_string(),
                    ..Default::default()
                },
            );
            let messages = body["messages"].as_array().expect("chat messages");
            assert_eq!(messages.len(), calls.len() + 1);

            let tool_calls = messages[0]["tool_calls"].as_array().expect("tool calls");
            assert_eq!(tool_calls.len(), calls.len());
            for (tool_call, (id, name, input, _)) in tool_calls.iter().zip(&calls) {
                assert_eq!(tool_call["id"], *id);
                assert_eq!(tool_call["type"], "function");
                assert_eq!(tool_call["function"]["name"], *name);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(
                        tool_call["function"]["arguments"]
                            .as_str()
                            .expect("function arguments"),
                    )
                    .expect("valid function arguments"),
                    *input
                );
            }
            for (result, (id, _, _, output)) in messages[1..].iter().zip(&calls) {
                assert_eq!(result["role"], "tool");
                assert_eq!(result["tool_call_id"], *id);
                assert_eq!(result["content"], *output);
            }
        }
    }

    #[test]
    fn responses_history_keeps_single_and_parallel_tool_calls_with_their_results() {
        let client = OpenAiClient::new("test-key").unwrap();
        for calls in [
            vec![(
                "call_read",
                "read",
                serde_json::json!({"path": "Cargo.toml"}),
                "contents",
            )],
            vec![
                (
                    "call_read",
                    "read",
                    serde_json::json!({"path": "Cargo.toml"}),
                    "contents",
                ),
                (
                    "call_glob",
                    "glob",
                    serde_json::json!({"pattern": "*.rs"}),
                    "src/main.rs",
                ),
            ],
        ] {
            let body = client
                .build_responses_request_body(
                    &tool_call_history(&calls),
                    &RequestConfig {
                        model: "openai/gpt-5.1-codex-max".to_string(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let input = body["input"].as_array().expect("Responses input");
            assert_eq!(input.len(), calls.len() * 2);

            for (function_call, (id, name, args, _)) in input[..calls.len()].iter().zip(&calls) {
                assert_eq!(function_call["type"], "function_call");
                assert_eq!(function_call["call_id"], *id);
                assert_eq!(function_call["name"], *name);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(
                        function_call["arguments"]
                            .as_str()
                            .expect("function arguments"),
                    )
                    .expect("valid function arguments"),
                    *args
                );
            }
            for (result, (id, _, _, output)) in input[calls.len()..].iter().zip(&calls) {
                assert_eq!(result["type"], "function_call_output");
                assert_eq!(result["call_id"], *id);
                assert_eq!(result["output"], *output);
            }
        }
    }

    #[tokio::test]
    async fn gemini_context_survives_stream_checkpoint_and_next_request() {
        let item = serde_json::json!({
            "type": "function_call", "id": "item-1", "call_id": "call-1",
            "name": "computer.write_file", "arguments": "{\"content\":\"500500\"}",
            "extra_content": {"google": {
                "native_name": "computer_write_file", "native_id": "native-1",
                "thought_signature": "opaque-provider-signature+/=="
            }}
        });
        for include_done in [false, true] {
            let mut wire = String::new();
            if include_done {
                wire.push_str(&format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "type":"response.output_item.done", "output_index":0, "item":item
                    })
                ));
            }
            wire.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type":"response.completed", "response":{"output":[item]}
                })
            ));
            let events = collect_responses_sse(&wire).await;
            let calls: Vec<_> = events
                .into_iter()
                .filter_map(|event| match event {
                    StreamEvent::ContentBlockStart {
                        block: block @ ContentBlock::ToolUse { .. },
                        ..
                    } => Some(block),
                    _ => None,
                })
                .collect();
            assert_eq!(
                calls.len(),
                1,
                "terminal fallback must not duplicate the call"
            );
            let ContentBlock::ToolUse {
                id,
                input,
                gemini_context,
                ..
            } = &calls[0]
            else {
                unreachable!()
            };
            assert_eq!(input, &serde_json::json!({"content":"500500"}));
            assert_eq!(
                gemini_context
                    .as_ref()
                    .unwrap()
                    .thought_signature
                    .as_deref(),
                Some("opaque-provider-signature+/==")
            );
            assert!(!format!("{:?}", gemini_context).contains("opaque-provider-signature"));
            let history = vec![
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(calls.clone()),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "wrote 6 bytes\n".into(),
                        is_error: None,
                    }]),
                },
            ];
            let restored: Vec<Message> =
                serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
            let config = RequestConfig {
                model: "gemini-3.6-flash".into(),
                ..Default::default()
            };
            for provider in [
                "vertex-ai",
                "vertex_ai",
                "gemini",
                "google",
                "google-ai",
                "googleai",
                "openai",
            ] {
                let client = OpenAiClient::new("test-key")
                    .unwrap()
                    .with_managed_gateway_context(
                        "org-test",
                        "workspace-test",
                        serde_json::json!({"provider":provider}),
                    )
                    .unwrap();
                let body = client
                    .build_responses_request_body(&restored, &config)
                    .unwrap();
                if provider == "openai" {
                    assert!(body["input"][0].get("extra_content").is_none());
                } else {
                    assert_eq!(body["input"][0]["extra_content"], item["extra_content"]);
                }
                assert_eq!(body["input"][0]["call_id"], body["input"][1]["call_id"]);
                assert_eq!(body["input"][1]["output"], "wrote 6 bytes\n");
            }
            // Hosted turns select the provider from signed candidates. The
            // legacy constructor route can name a different provider.
            for provider in [
                "vertex-ai",
                "vertex_ai",
                "gemini",
                "google",
                "google-ai",
                "googleai",
                "openai",
            ] {
                let mut client = OpenAiClient::new("test-key")
                    .unwrap()
                    .with_managed_gateway_context(
                        "org_123",
                        "workspace_456",
                        serde_json::json!({"provider": if provider == "openai" { "vertex_ai" } else { "openrouter" }}),
                    )
                    .unwrap();
                let mut authorization: serde_json::Value =
                    serde_json::from_str(&managed_authorization_fixture("gemini-history")).unwrap();
                authorization["claims"]["providerCandidates"][0]["provider"] =
                    serde_json::json!(provider);
                client.set_managed_inference_authorization(Some(authorization.to_string()));
                let body = client
                    .managed_request(
                        client
                            .build_responses_request_body(&restored, &config)
                            .unwrap(),
                    )
                    .unwrap();
                assert!(body.get("provider_ref").is_none());
                assert_eq!(
                    body["provider_candidates"][0]["provider_ref"]["provider"],
                    provider
                );
                if provider == "openai" {
                    assert!(body["input"][0].get("extra_content").is_none());
                } else {
                    assert_eq!(body["input"][0]["extra_content"], item["extra_content"]);
                }
                assert_eq!(body["input"][0]["call_id"], body["input"][1]["call_id"]);
            }
            let other = OpenAiClient::new("test-key")
                .unwrap()
                .build_responses_request_body(&restored, &config)
                .unwrap();
            assert!(other["input"][0].get("extra_content").is_none());
        }
    }

    #[test]
    fn responses_requests_use_max_output_tokens() {
        let client = OpenAiClient::new("test-key").unwrap();
        let body = client
            .build_responses_request_body(
                &[],
                &RequestConfig {
                    model: "gpt-5.1-codex-max".to_string(),
                    max_tokens: 1234,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(body["max_output_tokens"], 1234);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn responses_history_pairs_synthesized_cancel_result_with_its_call() {
        // History shape left by `repair_orphaned_tool_calls` after a turn is
        // cancelled mid-tool-execution: the assistant tool call is immediately
        // followed by a user message carrying only the synthesized failure
        // result, then the next prompt. The Responses payload must pair every
        // function_call with a function_call_output or OpenAI answers 400.
        let client = OpenAiClient::new("test-key").unwrap();
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("run sleep 120 via bash".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "sleep 120"}),
                    gemini_context: None,
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "Tool execution cancelled by user.".to_string(),
                    is_error: Some(true),
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Text("next prompt".to_string()),
            },
        ];

        let body = client
            .build_responses_request_body(
                &messages,
                &RequestConfig {
                    model: "gpt-5.1-codex-max".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        let input = body["input"].as_array().expect("Responses input");
        let call_ids: Vec<&str> = input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["call_id"].as_str().expect("function call id"))
            .collect();
        let output_ids: Vec<&str> = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["call_id"].as_str().expect("function output id"))
            .collect();
        assert_eq!(call_ids, vec!["call_1"]);
        assert_eq!(call_ids, output_ids);

        // The Chat Completions serializer must pair them too: an assistant
        // tool_calls message immediately followed by a matching tool message.
        let chat_body = client.build_request_body(
            &messages,
            &RequestConfig {
                model: "gpt-4o".to_string(),
                ..Default::default()
            },
        );
        let chat_messages = chat_body["messages"].as_array().expect("chat messages");
        let assistant_index = chat_messages
            .iter()
            .position(|msg| msg["tool_calls"].is_array())
            .expect("assistant tool_calls message");
        let tool_msg = &chat_messages[assistant_index + 1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(
            tool_msg["tool_call_id"],
            chat_messages[assistant_index]["tool_calls"][0]["id"]
        );
    }

    #[test]
    fn managed_gateway_adds_org_header_and_provider_reference() {
        let client =
            OpenAiClient::with_base_url("delegated-token", "https://llm-gateway.evalops.dev/v1")
                .unwrap()
                .with_managed_gateway_context(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({
                        "provider": "anthropic",
                        "environment": "prod",
                        "credential_name": "team-shared"
                    }),
                )
                .unwrap();
        assert_eq!(
            client.headers().get("x-organization-id").unwrap(),
            "org_123"
        );
        assert_eq!(
            client.response_open_timeout(),
            Some(MANAGED_GATEWAY_RESPONSE_OPEN_TIMEOUT)
        );

        let body = client.build_request_body(
            &[Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            }],
            &RequestConfig {
                model: "evalops/gpt-4o-mini".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(body["provider_ref"]["provider"], "anthropic");
        assert_eq!(body["provider_ref"]["credential_name"], "team-shared");
    }

    #[test]
    fn managed_gateway_turn_lineage_is_stable_and_body_scoped() {
        let mut client =
            OpenAiClient::with_base_url("delegated-token", "https://llm-gateway.evalops.dev/v1")
                .unwrap()
                .with_managed_gateway_scope(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({
                        "provider": "openrouter",
                        "environment": "production",
                        "credential_name": "default"
                    }),
                )
                .unwrap();
        client.set_managed_request_lineage(Some("maestro-turn-v2:digest".to_string()));

        let request = |content: &str| {
            client
                .managed_request(client.build_request_body(
                    &[Message {
                        role: Role::User,
                        content: MessageContent::Text(content.to_string()),
                    }],
                    &RequestConfig {
                        model: "evalops/openrouter/auto".to_string(),
                        ..Default::default()
                    },
                ))
                .expect("managed request")
        };
        let first_body = request("first");
        let replay_body = request("first");
        let next_body = request("second");

        assert_eq!(first_body["lineage_id"], replay_body["lineage_id"]);
        assert_eq!(first_body["lineage_id"], next_body["lineage_id"]);
        assert!(
            first_body["lineage_id"]
                .as_str()
                .is_some_and(|value| value.starts_with("maestro-turn-v2:"))
        );
        assert_eq!(first_body, replay_body);
        assert_ne!(first_body, next_body);
    }

    #[tokio::test]
    async fn managed_gateway_timeout_response_is_a_typed_transient_stream_failure() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
        let address = listener.local_addr().expect("mock gateway address");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read gateway request");
            let body = r#"{"error":{"type":"server_error","message":"operation timeout"}}"#;
            write!(
                stream,
                "HTTP/1.1 504 Gateway Timeout\r\nX-Request-ID: request-timeout\r\nX-EvalOps-Record-ID: record-timeout\r\nX-EvalOps-Lineage-ID: lineage-timeout\r\nX-EvalOps-Record-Status: failed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            )
            .expect("write gateway timeout");
        });

        let client = OpenAiClient::with_base_url("delegated-token", format!("http://{address}/v1"))
            .unwrap()
            .with_managed_gateway_context(
                "org_123",
                "workspace_456",
                serde_json::json!({
                    "provider": "openrouter",
                    "environment": "prod",
                    "credential_name": "default"
                }),
            )
            .unwrap();
        let mut events = client
            .stream(
                &[],
                &RequestConfig {
                    model: "evalops/openai/gpt-5.6-terra".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("gateway response opened");

        assert!(matches!(
            events.recv().await,
            Some(StreamEvent::ManagedGatewayReceipt(ManagedGatewayReceipt {
                request_id,
                record_id,
                lineage_id,
                record_status,
                ..
            })) if request_id == "request-timeout"
                && record_id == "record-timeout"
                && lineage_id == "lineage-timeout"
                && record_status == "failed"
        ));
        assert!(matches!(
            events.recv().await,
            Some(StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message,
            }) if message.contains("504 Gateway Timeout")
        ));
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn managed_gateway_open_waits_for_its_provider_attempt_to_finish() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0_u8; 4096];
            assert!(
                socket.read(&mut bytes).await.unwrap() > 0,
                "request must reach server"
            );
            accepted.send(()).unwrap();
            // The deployed gateway owns a 45-second provider attempt.
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            let _ = socket.write_all(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        });
        let request = reqwest::Client::new().get(format!("http://{address}/responses"));
        let client = tokio::spawn(send_with_response_open_timeout(
            request,
            Some(MANAGED_GATEWAY_RESPONSE_OPEN_TIMEOUT),
        ));
        received.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(46)).await;
        tokio::time::resume();
        let response = client.await.unwrap().expect(
            "client must receive the gateway terminal instead of abandoning a still-owned provider attempt",
        );
        assert_eq!(response.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn managed_gateway_response_open_timeout_stops_stalled_headers() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
        let address = listener.local_addr().expect("mock gateway address");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read gateway request");
            std::thread::sleep(std::time::Duration::from_millis(150));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });

        let request = reqwest::Client::new().get(format!("http://{address}/responses"));
        let error =
            send_with_response_open_timeout(request, Some(std::time::Duration::from_millis(25)))
                .await
                .expect_err("stalled response opening must time out");

        assert!(
            error
                .to_string()
                .contains("managed gateway response headers timed out"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn managed_gateway_response_open_timeout_does_not_cover_stream_body() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
        let address = listener.local_addr().expect("mock gateway address");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read gateway request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .expect("write response headers");
            stream.flush().expect("flush response headers");
            std::thread::sleep(std::time::Duration::from_millis(150));
            stream.write_all(b"hello").expect("write delayed body");
        });

        let request = reqwest::Client::new().get(format!("http://{address}/responses"));
        let response =
            send_with_response_open_timeout(request, Some(std::time::Duration::from_millis(50)))
                .await
                .expect("response headers should arrive within the open timeout");
        let body = response
            .text()
            .await
            .expect("body may continue past the response-open timeout");

        assert_eq!(body, "hello");
    }

    #[test]
    fn managed_gateway_scope_adds_workspace_header_without_changing_provider_reference() {
        let client =
            OpenAiClient::with_base_url("delegated-token", "https://llm-gateway.evalops.dev/v1")
                .unwrap()
                .with_managed_gateway_scope(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({
                        "provider": "openrouter",
                        "environment": "prod",
                    }),
                )
                .unwrap();
        assert_eq!(
            client.headers().get("x-organization-id").unwrap(),
            "org_123"
        );
        assert_eq!(
            client.headers().get("x-workspace-id").unwrap(),
            "workspace_456"
        );
        assert_eq!(
            client.request_extensions["provider_ref"]["provider"],
            "openrouter"
        );
        assert_eq!(
            client.request_extensions["provider_ref"]["environment"],
            "prod"
        );
        assert_eq!(client.request_extensions["organization_id"], "org_123");
        assert_eq!(client.request_extensions["workspace_id"], "workspace_456");
        let body = client.build_request_body(
            &[Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            }],
            &RequestConfig {
                model: "openai/gpt-5.6".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(body["model"], "openai/gpt-5.6");
        assert_eq!(body["organization_id"], "org_123");
        assert_eq!(body["workspace_id"], "workspace_456");
    }

    #[test]
    fn managed_openrouter_gateway_preserves_opaque_model_namespace() {
        let client =
            OpenAiClient::with_base_url("delegated-token", "https://llm-gateway.evalops.dev/v1")
                .unwrap()
                .with_route_provider("openrouter")
                .with_managed_gateway_scope(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({
                        "provider": "openrouter",
                        "environment": "prod",
                    }),
                )
                .unwrap();

        for model in ["openrouter/auto", "evalops/openrouter/auto"] {
            let body = client.build_request_body(
                &[Message {
                    role: Role::User,
                    content: MessageContent::Text("Hello".to_string()),
                }],
                &RequestConfig {
                    model: model.to_string(),
                    ..Default::default()
                },
            );

            assert_eq!(body["model"], "openrouter/auto");
        }
    }

    #[test]
    fn managed_gateway_classifies_namespaced_responses_models_before_protocol_selection() {
        let client =
            OpenAiClient::with_base_url("delegated-token", "https://llm-gateway.evalops.dev/v1")
                .unwrap()
                .with_managed_gateway_scope(
                    "org_123",
                    "workspace_456",
                    serde_json::json!({
                        "provider": "openai",
                        "environment": "prod",
                    }),
                )
                .unwrap();
        for model in [
            "evalops/openai/gpt-5.6",
            "EVALOPS/openai/gpt-5.6",
            "maestro-managed/openai/gpt-5.6",
            "MAESTRO-MANAGED/openai/gpt-5.6",
        ] {
            let body = client.build_request_body(
                &[Message {
                    role: Role::User,
                    content: MessageContent::Text("Hello".to_string()),
                }],
                &RequestConfig {
                    model: model.to_string(),
                    ..Default::default()
                },
            );

            assert!(client.uses_responses_api_for(model));
            assert!(body["input"].is_array());
            assert_eq!(
                client.request_url(model),
                "https://llm-gateway.evalops.dev/v1/responses"
            );
            assert_eq!(body["model"], "openai/gpt-5.6");
        }
    }

    #[test]
    fn test_extract_function_call() {
        // Valid function call
        let valid = serde_json::json!({
            "type": "function_call",
            "call_id": "call_123",
            "name": "read",
            "arguments": "{\"path\": \"/tmp/test.txt\"}"
        });
        let result = extract_function_call(&valid);
        assert!(result.is_some());
        let (call_id, name, args) = result.unwrap();
        assert_eq!(call_id, "call_123");
        assert_eq!(name, "read");
        assert_eq!(args, serde_json::json!({ "path": "/tmp/test.txt" }));

        // Arguments can arrive as an object
        let object_args = serde_json::json!({
            "type": "function_call",
            "call_id": "call_456",
            "name": "read",
            "arguments": { "path": "/tmp/other.txt" }
        });
        let result = extract_function_call(&object_args);
        assert!(result.is_some());
        let (_, _, args) = result.unwrap();
        assert_eq!(args, serde_json::json!({ "path": "/tmp/other.txt" }));

        // Invalid JSON strings fall back to empty object
        let invalid_args = serde_json::json!({
            "type": "function_call",
            "call_id": "call_789",
            "name": "read",
            "arguments": "{not-json}"
        });
        let result = extract_function_call(&invalid_args);
        assert!(result.is_some());
        let (_, _, args) = result.unwrap();
        assert_eq!(args, serde_json::json!({}));

        // Null arguments are treated as empty object
        let null_args = serde_json::json!({
            "type": "function_call",
            "call_id": "call_999",
            "name": "read",
            "arguments": null
        });
        let result = extract_function_call(&null_args);
        assert!(result.is_some());
        let (_, _, args) = result.unwrap();
        assert_eq!(args, serde_json::json!({}));

        // Not a function call
        let message = serde_json::json!({
            "type": "message",
            "content": "Hello"
        });
        assert!(extract_function_call(&message).is_none());

        // Missing fields
        let incomplete = serde_json::json!({
            "type": "function_call",
            "name": "read"
        });
        assert!(extract_function_call(&incomplete).is_none());
    }

    #[test]
    fn test_extract_text_from_item() {
        // Message with output_text content
        let msg = serde_json::json!({
            "type": "message",
            "content": [
                {
                    "type": "output_text",
                    "text": "Hello, world!"
                }
            ]
        });
        let text = extract_text_from_item(&msg);
        assert_eq!(text, Some("Hello, world!".to_string()));

        // Message with no content
        let empty = serde_json::json!({
            "type": "message",
            "content": []
        });
        assert!(extract_text_from_item(&empty).is_none());

        // Not a message
        let other = serde_json::json!({
            "type": "function_call"
        });
        assert!(extract_text_from_item(&other).is_none());
    }

    #[test]
    fn test_classify_error() {
        // Context length exceeded
        let ctx_error = serde_json::json!({
            "code": "context_length_exceeded",
            "message": "Maximum context length exceeded"
        });
        match classify_error(&ctx_error) {
            ApiError::ContextWindowExceeded => {}
            _ => panic!("Expected ContextWindowExceeded"),
        }

        // Quota exceeded
        let quota_error = serde_json::json!({
            "code": "insufficient_quota",
            "message": "You exceeded your quota"
        });
        match classify_error(&quota_error) {
            ApiError::QuotaExceeded => {}
            _ => panic!("Expected QuotaExceeded"),
        }

        // Rate limited
        let rate_error = serde_json::json!({
            "code": "rate_limit_exceeded",
            "message": "Rate limit exceeded"
        });
        match classify_error(&rate_error) {
            ApiError::RateLimited { .. } => {}
            _ => panic!("Expected RateLimited"),
        }

        // Unknown error without retryable keywords is Fatal
        let generic = serde_json::json!({
            "code": "something_else",
            "message": "Something went wrong"
        });
        match classify_error(&generic) {
            ApiError::Fatal { message } => {
                assert_eq!(message, "Something went wrong");
            }
            _ => panic!("Expected Fatal for unknown errors"),
        }

        // Error with "temporarily" keyword is Retryable
        let temp_error = serde_json::json!({
            "code": "some_error",
            "message": "The service is temporarily unavailable"
        });
        match classify_error(&temp_error) {
            ApiError::Retryable { .. } => {}
            _ => panic!("Expected Retryable for temporary errors"),
        }

        // Error with "overloaded" keyword is Retryable
        let overload_error = serde_json::json!({
            "code": "some_error",
            "message": "Server is overloaded, please retry"
        });
        match classify_error(&overload_error) {
            ApiError::Retryable { .. } => {}
            _ => panic!("Expected Retryable for overload errors"),
        }

        // Server error type is Retryable
        let server_error = serde_json::json!({
            "type": "server_error",
            "message": "Internal server error"
        });
        match classify_error(&server_error) {
            ApiError::Retryable { .. } => {}
            _ => panic!("Expected Retryable for server errors"),
        }

        // Authentication error is Fatal
        let auth_error = serde_json::json!({
            "type": "authentication_error",
            "message": "Invalid API key"
        });
        match classify_error(&auth_error) {
            ApiError::Fatal { .. } => {}
            _ => panic!("Expected Fatal for auth errors"),
        }

        // Invalid API key is Fatal
        let invalid_key = serde_json::json!({
            "code": "invalid_api_key",
            "message": "Provided API key is invalid"
        });
        match classify_error(&invalid_key) {
            ApiError::Fatal { .. } => {}
            _ => panic!("Expected Fatal for invalid API key"),
        }
    }

    #[test]
    fn test_parse_retry_after() {
        // Seconds
        let secs = parse_retry_after("Please try again in 30s");
        assert!(secs.is_some());
        assert_eq!(secs.unwrap(), std::time::Duration::from_secs(30));

        // Milliseconds
        let ms = parse_retry_after("Try again in 500ms");
        assert!(ms.is_some());
        assert_eq!(ms.unwrap(), std::time::Duration::from_millis(500));

        // Float seconds
        let float = parse_retry_after("try again in 2.5s");
        assert!(float.is_some());
        assert_eq!(float.unwrap(), std::time::Duration::from_secs_f64(2.5));

        // No retry-after info
        let none = parse_retry_after("Rate limit exceeded");
        assert!(none.is_none());
    }

    #[test]
    fn test_responses_sse_event_parsing() {
        // Test parsing response.created event
        let created_data = r#"{"type":"response.created","response":{"id":"resp_123"}}"#;
        let event: ResponsesSseEvent = serde_json::from_str(created_data).unwrap();
        assert_eq!(event.kind, "response.created");
        assert!(event.response.is_some());
        let resp = event.response.unwrap();
        assert_eq!(resp.get("id").unwrap().as_str().unwrap(), "resp_123");

        // Test parsing output_text.delta event
        let delta_data = r#"{"type":"response.output_text.delta","delta":"Hello"}"#;
        let event: ResponsesSseEvent = serde_json::from_str(delta_data).unwrap();
        assert_eq!(event.kind, "response.output_text.delta");
        assert_eq!(event.delta, Some("Hello".to_string()));

        // Test parsing output_item.done event
        let done_data =
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[]}}"#;
        let event: ResponsesSseEvent = serde_json::from_str(done_data).unwrap();
        assert_eq!(event.kind, "response.output_item.done");
        assert!(event.item.is_some());

        let part_data = r#"{"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#;
        let event: ResponsesSseEvent = serde_json::from_str(part_data).unwrap();
        assert_eq!(event.kind, "response.content_part.added");
        assert_eq!(event.part.unwrap()["type"], "output_text");
    }

    #[tokio::test]
    async fn responses_empty_current_shape_emits_clean_terminal_sequence() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_empty"}}

data: {"type":"response.completed","response":{"output":[],"usage":{"input_tokens":17,"output_tokens":0}}}

"#,
        )
        .await;

        assert!(matches!(
            events.as_slice(),
            [
                StreamEvent::MessageStart { id, .. },
                StreamEvent::Usage {
                    input_tokens: 17,
                    output_tokens: 0,
                    ..
                },
                StreamEvent::MessageStop { .. }
            ] if id == "resp_empty"
        ));
    }

    #[tokio::test]
    async fn responses_current_content_part_shape_emits_text_once() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_text"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","content":[]}}

data: {"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}

data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hello"}

data: {"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello, world!"}}

data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"Hello, world!"}]}}

data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"Hello, world!"}]}],"usage":{"input_tokens":3,"output_tokens":4}}}

"#,
        )
        .await;

        let text = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(text, ["Hello", ", world!"]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ContentBlockStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ContentBlockStop { .. }))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageStop { .. })
        ));
    }

    #[tokio::test]
    async fn responses_terminal_output_recovers_tool_only_with_arguments() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_tool"}}

data: {"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}],"usage":{"input_tokens":5,"output_tokens":2}}}

"#,
        )
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlock::ToolUse { id, name, input , .. }
            } if id == "call_1" && name == "read" && input == &serde_json::json!({"path": "Cargo.toml"})
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::InputJsonDelta { index: 1, partial_json }
                if partial_json == "{\"path\":\"Cargo.toml\"}"
        )));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageStop { .. })
        ));
    }

    #[tokio::test]
    async fn responses_streamed_tool_arguments_keep_one_index_after_reasoning() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_tool_index"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}

data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"Cargo.toml\"}"}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}}

data: {"type":"response.completed","response":{"output":[{"type":"reasoning"},{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}],"usage":{"input_tokens":5,"output_tokens":2}}}

"#,
        )
        .await;

        let start_index = events.iter().find_map(|event| match event {
            StreamEvent::ContentBlockStart {
                index,
                block: ContentBlock::ToolUse { id, .. },
            } if id == "call_1" => Some(*index),
            _ => None,
        });
        let delta_indices = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::InputJsonDelta {
                    index,
                    partial_json,
                } if partial_json == "{\"path\":\"Cargo.toml\"}" => Some(*index),
                _ => None,
            })
            .collect::<Vec<_>>();
        let stop_index = events.iter().find_map(|event| match event {
            StreamEvent::ContentBlockStop { index, .. } => Some(*index),
            _ => None,
        });

        assert_eq!(start_index, Some(1));
        assert_eq!(delta_indices, [1]);
        assert_eq!(stop_index, start_index);
    }

    #[tokio::test]
    async fn responses_terminal_tool_fallback_reuses_streamed_argument_index() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_tool_fallback_index"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning"}}

data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"Cargo.toml\"}"}

data: {"type":"response.completed","response":{"output":[{"type":"reasoning"},{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"Cargo.toml\"}"}],"usage":{"input_tokens":5,"output_tokens":2}}}

"#,
        )
        .await;

        let start_index = events.iter().find_map(|event| match event {
            StreamEvent::ContentBlockStart {
                index,
                block: ContentBlock::ToolUse { id, .. },
            } if id == "call_1" => Some(*index),
            _ => None,
        });
        let delta_indices = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::InputJsonDelta {
                    index,
                    partial_json,
                } if partial_json == "{\"path\":\"Cargo.toml\"}" => Some(*index),
                _ => None,
            })
            .collect::<Vec<_>>();
        let stop_index = events.iter().find_map(|event| match event {
            StreamEvent::ContentBlockStop { index, .. } => Some(*index),
            _ => None,
        });

        assert_eq!(start_index, Some(1));
        assert_eq!(delta_indices, [1]);
        assert_eq!(stop_index, start_index);
    }

    #[tokio::test]
    async fn responses_incomplete_is_terminal_error_without_message_stop() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_incomplete"}}

data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","content":[{"type":"output_text","text":"do not expose"}]}]}}

"#,
        )
        .await;

        assert!(matches!(
            events.last(),
            Some(StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::OutputTokenExhaustion,
                message,
            })
                if message == "openai_response_exhausted: reason=max_output_tokens"
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::MessageStop { .. }))
        );
    }

    #[tokio::test]
    async fn responses_failed_is_typed_provider_declared_failure() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.failed","response":{"id":"resp_failed","error":{"code":"provider_error","message":"provider rejected the response"}}}

"#,
        )
        .await;

        assert!(matches!(
            events.last(),
            Some(StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::ProviderDeclaredFailure,
                message,
            }) if message.contains("provider rejected the response")
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            StreamEvent::Error { .. } | StreamEvent::MessageStop { .. }
        )));
    }

    #[tokio::test]
    async fn responses_failed_retryable_is_typed_transient_protocol() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.failed","response":{"id":"resp_retryable","error":{"type":"server_error","message":"temporarily overloaded"}}}

"#,
        )
        .await;

        assert!(matches!(
            events.last(),
            Some(StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message,
            }) if message.contains("temporarily overloaded")
        ));
    }

    #[tokio::test]
    async fn responses_eof_before_terminal_frame_is_transient_error() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.created","response":{"id":"resp_cut"}}

"#,
        )
        .await;

        assert!(matches!(
            events.last(),
            Some(StreamEvent::ProviderError {
                kind: ProviderStreamErrorKind::TransientProtocol,
                message,
            })
                if message
                    == "openai_response_protocol_error: kind=transient reason=missing_terminal_event"
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::MessageStop { .. }))
        );
    }

    #[tokio::test]
    async fn responses_completed_with_assistant_text_is_success() {
        let events = collect_responses_sse(
            r#"data: {"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":1,"output_tokens":1}}}

"#,
        )
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TextDelta { text, .. } if text == "hello"
        )));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageStop { .. })
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            StreamEvent::Error { .. } | StreamEvent::ProviderError { .. }
        )));
    }

    #[test]
    fn responses_terminal_output_fills_only_missing_text_and_tools() {
        assert_eq!(
            missing_text_suffix("Hello", "Hello, world!"),
            Some(", world!".to_string())
        );
        assert_eq!(missing_text_suffix("Hello, world!", "Hello, world!"), None);

        let completed_data = r#"{
            "type":"response.completed",
            "response":{"output":[
                {"type":"message","content":[{"type":"output_text","text":"fallback"}]},
                {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}
            ]}
        }"#;
        let event: ResponsesSseEvent = serde_json::from_str(completed_data).unwrap();
        let response = event.response.unwrap();
        let output = response["output"].as_array().expect("terminal output");
        assert_eq!(
            extract_text_from_item(&output[0]).and_then(|text| missing_text_suffix("", &text)),
            Some("fallback".to_string())
        );
        let (call_id, _, _) = extract_function_call(&output[1]).expect("terminal tool call");
        let mut emitted = std::collections::HashSet::new();
        assert!(emitted.insert(call_id.clone()));
        assert!(!emitted.insert(call_id));
    }

    #[test]
    fn incomplete_response_error_exposes_only_stable_reason() {
        let response = serde_json::json!({
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"content": [{"text": "private response content"}]}]
        });
        let error = incomplete_response_error(Some(&response));

        assert_eq!(error.kind, ProviderStreamErrorKind::OutputTokenExhaustion);
        assert_eq!(error.reason, "max_output_tokens");
        assert_eq!(
            error.to_string(),
            "openai_response_exhausted: reason=max_output_tokens"
        );
        assert!(!error.to_string().contains("private response content"));
    }

    #[test]
    fn test_responses_sse_event_defaults() {
        // Missing optional fields should default to None
        let minimal = r#"{"type":"response.created"}"#;
        let event: ResponsesSseEvent = serde_json::from_str(minimal).unwrap();
        assert_eq!(event.kind, "response.created");
        assert!(event.response.is_none());
        assert!(event.item.is_none());
        assert!(event.part.is_none());
        assert!(event.delta.is_none());
        assert!(event.output_index.is_none());
    }

    #[test]
    fn test_is_context_overflow_error() {
        // Anthropic pattern
        assert!(is_context_overflow_error(
            "prompt is too long: 213462 tokens > 200000 maximum"
        ));

        // OpenAI pattern
        assert!(is_context_overflow_error(
            "Your input exceeds the context window of this model"
        ));

        // Google Gemini pattern
        assert!(is_context_overflow_error(
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)"
        ));

        // xAI (Grok) pattern
        assert!(is_context_overflow_error(
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens"
        ));

        // Groq pattern
        assert!(is_context_overflow_error(
            "Please reduce the length of the messages or completion"
        ));

        // OpenRouter pattern
        assert!(is_context_overflow_error(
            "This endpoint's maximum context length is 128000 tokens"
        ));

        // llama.cpp pattern
        assert!(is_context_overflow_error(
            "the request exceeds the available context size"
        ));

        // LM Studio pattern
        assert!(is_context_overflow_error(
            "tokens to keep from the initial prompt is greater than the context length"
        ));

        // Cerebras/Mistral 400 status code
        assert!(is_context_overflow_error("400 status code (no body)"));
        assert!(is_context_overflow_error("413 (no body)"));

        // Generic patterns
        assert!(is_context_overflow_error("context length exceeded"));
        assert!(is_context_overflow_error("too many tokens in request"));
        assert!(is_context_overflow_error("token limit exceeded"));

        // Non-overflow errors should return false
        assert!(!is_context_overflow_error("Rate limit exceeded"));
        assert!(!is_context_overflow_error("Invalid API key"));
        assert!(!is_context_overflow_error("Server error"));
    }

    #[test]
    fn test_is_mistral_model() {
        // Mistral models by name
        assert!(is_mistral_model("mistral-large", None));
        assert!(is_mistral_model("mistral-small", None));
        assert!(is_mistral_model("mixtral-8x7b", None));
        assert!(is_mistral_model("codestral", None));
        assert!(is_mistral_model("pixtral-12b", None));

        // Case insensitive
        assert!(is_mistral_model("Mistral-Large", None));
        assert!(is_mistral_model("MIXTRAL-8x22b", None));

        // By base URL
        assert!(is_mistral_model(
            "some-model",
            Some("https://api.mistral.ai/v1")
        ));

        // Non-Mistral models
        assert!(!is_mistral_model("gpt-4o", None));
        assert!(!is_mistral_model("claude-opus-4-5", None));
        assert!(!is_mistral_model("llama-3.1", None));
    }

    #[test]
    fn test_is_groq_model() {
        // Explicit groq/ prefix
        assert!(is_groq_model("groq/llama-3.1-70b", None));
        assert!(is_groq_model("groq/mixtral-8x7b", None));

        // By base URL
        assert!(is_groq_model(
            "some-model",
            Some("https://api.groq.com/openai/v1")
        ));

        // Llama models
        assert!(is_groq_model("llama-3.1-70b-versatile", None));
        assert!(is_groq_model("llama3-8b-8192", None));
        assert!(is_groq_model("llama-guard-3-8b", None));

        // DeepSeek models
        assert!(is_groq_model("deepseek-r1-distill-llama-70b", None));

        // Qwen models
        assert!(is_groq_model("qwen-2.5-coder-32b", None));

        // Case insensitive
        assert!(is_groq_model("Llama-3.1-8B", None));
        assert!(is_groq_model("GROQ/llama", None));

        // Non-Groq models
        assert!(!is_groq_model("gpt-4o", None));
        assert!(!is_groq_model("claude-opus-4-5", None));
        assert!(!is_groq_model("gemini-pro", None));

        // A custom non-Groq base URL pins the provider, so deepseek/qwen ids do
        // NOT get Groq-specific request shaping on direct provider clients.
        assert!(!is_groq_model(
            "deepseek-chat",
            Some("https://api.deepseek.com/v1")
        ));
        assert!(!is_groq_model(
            "qwen3-max",
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1")
        ));
    }

    #[test]
    fn test_normalize_mistral_tool_id() {
        // Already valid 9-char alphanumeric
        assert_eq!(normalize_mistral_tool_id("abcdefghi"), "abcdefghi");

        // Too short - should pad with zeros
        assert_eq!(normalize_mistral_tool_id("abc"), "abc000000");
        assert_eq!(normalize_mistral_tool_id("a"), "a00000000");

        // Too long - should truncate
        assert_eq!(normalize_mistral_tool_id("abcdefghijklm"), "abcdefghi");

        // Contains non-alphanumeric - should remove them
        assert_eq!(normalize_mistral_tool_id("call_abc123"), "callabc12");
        assert_eq!(normalize_mistral_tool_id("a-b-c-d-e"), "abcde0000");

        // UUID-like IDs
        assert_eq!(
            normalize_mistral_tool_id("550e8400-e29b-41d4-a716-446655440000"),
            "550e8400e"
        );
    }

    #[test]
    fn test_classify_error_with_overflow_patterns() {
        // Test that classify_error detects overflow from message patterns
        // even without the specific error code

        // Anthropic-style message without code
        let anthropic_overflow = serde_json::json!({
            "message": "prompt is too long: 213462 tokens > 200000 maximum"
        });
        match classify_error(&anthropic_overflow) {
            ApiError::ContextWindowExceeded => {}
            _ => panic!("Expected ContextWindowExceeded"),
        }

        // OpenRouter-style message
        let openrouter_overflow = serde_json::json!({
            "message": "This endpoint's maximum context length is 128000 tokens"
        });
        match classify_error(&openrouter_overflow) {
            ApiError::ContextWindowExceeded => {}
            _ => panic!("Expected ContextWindowExceeded"),
        }

        // Cerebras/Mistral 400 with no body
        let status_overflow = serde_json::json!({
            "message": "400 status code (no body)"
        });
        match classify_error(&status_overflow) {
            ApiError::ContextWindowExceeded => {}
            _ => panic!("Expected ContextWindowExceeded"),
        }
    }
    #[tokio::test]
    async fn cache_topology_legacy_managed_requests_do_not_claim_session_authority() {
        let (mut client, request_rx) =
            managed_gateway_test_client(MANAGED_COMPLETED_SSE, &managed_receipt_headers());
        client.set_managed_inference_authorization(None);
        client.set_managed_request_lineage(Some("lineage-receipt".into()));
        assert!(client.cache_scope().unwrap().is_none());
        assert_ne!(client.cache_namespace().unwrap(), "local");
        let stream = client
            .stream(
                &[],
                &RequestConfig {
                    model: "gpt-5.6".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let events = collect_stream_events(stream).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::ProviderError { .. })),
            "{events:?}"
        );
        let request = request_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let body = captured_request_body(&request);
        assert!(body.get("cache_topology").is_none());
        assert!(body.get("managed_inference_authorization").is_none());
    }

    #[tokio::test]
    async fn cache_topology_preserves_translating_and_model_fallback_routes() {
        for (provider, candidate_model) in [
            ("google", "openai/gpt-5.6-terra"),
            ("vertex-ai", "openai/gpt-5.6-terra"),
            ("openrouter", "openai/gpt-5.6-terra"),
            ("azure-openai", "openai/gpt-5.6-terra"),
            ("openai", "different-fallback-model"),
        ] {
            let (mut client, request_rx) =
                managed_gateway_test_client(MANAGED_COMPLETED_SSE, &managed_receipt_headers());
            let mut authorization: serde_json::Value =
                serde_json::from_str(&managed_authorization_fixture("lineage-receipt")).unwrap();
            let mut fallback = authorization["claims"]["providerCandidates"][0].clone();
            fallback["provider"] = provider.into();
            fallback["model"] = candidate_model.into();
            authorization["claims"]["providerCandidates"]
                .as_array_mut()
                .unwrap()
                .push(fallback);
            client.set_managed_inference_authorization(Some(authorization.to_string()));
            let stream = client.stream(&[], &RequestConfig::default()).await.unwrap();
            let events = collect_stream_events(stream).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::ProviderError { .. })),
                "{events:?}"
            );
            let request = request_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let body = captured_request_body(&request);
            assert!(
                body.get("cache_topology").is_some(),
                "managed routes must carry topology"
            );
            assert_eq!(body["managed_inference_authorization"], authorization);
        }
    }

    #[tokio::test]
    async fn cache_topology_rejects_late_changes_before_opening_http() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::text("hello"),
        }];
        let mut config = RequestConfig::default();
        config.cache_topology = Some(
            crate::cache_topology::PreparedPrompt::prepare(
                &messages,
                &config,
                "local".into(),
                None,
            )
            .unwrap(),
        );
        let mut client =
            super::OpenAiClient::with_base_url("test-key", "http://127.0.0.1:1").unwrap();
        client
            .request_extensions
            .insert("instructions".into(), serde_json::json!("late injection"));
        let error = client
            .stream_with_producer(&messages, &config)
            .await
            .err()
            .expect("reject extension");
        assert!(error.to_string().contains("late prompt extensions"));
        client.request_extensions.clear();
        client
            .request_extensions
            .insert("session_id".into(), serde_json::json!("late affinity"));
        let error = client
            .stream_with_producer(&messages, &config)
            .await
            .err()
            .expect("reject affinity override");
        assert!(error.to_string().contains("late prompt extensions"));
        client.request_extensions.clear();
        config.system = Some("late change".into());
        let error = client
            .stream_with_producer(&messages, &config)
            .await
            .err()
            .expect("reject mutation");
        assert!(error.to_string().contains("prepared cache topology"));
    }
}
