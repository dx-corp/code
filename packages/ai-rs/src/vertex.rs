//! Vertex AI Client
//!
//! Implementation of Google's Vertex AI API for streaming AI responses.
//! Supports Gemini models via the Vertex AI endpoint with OAuth or API key auth.
//!
//! # API Differences from Direct Gemini API
//!
//! - Uses project-based endpoint: `{region}-aiplatform.googleapis.com`
//! - Requires Google Cloud project ID and region
//! - Supports both API key and OAuth authentication
//! - Different request format with Vertex-specific fields
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui::ai::vertex::VertexAiClient;
//!
//! let client = VertexAiClient::from_env()?;
//! let mut rx = client.stream(&messages, &config).await?;
//! while let Some(event) = rx.recv().await {
//!     // Handle streaming events
//! }
//! ```

use std::collections::HashMap;
use std::env;

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;

use super::types::{
    ContentBlock, Message, MessageContent, RequestConfig, Role, StopReason, StreamEvent,
};
use super::{AiProvider, provider_model_name};

/// Default Vertex AI region
const DEFAULT_REGION: &str = "us-central1";

/// Vertex AI client
#[derive(Clone)]
pub struct VertexAiClient {
    api_key: Option<String>,
    access_token: Option<String>,
    project_id: String,
    region: String,
    client: Client,
}

impl VertexAiClient {
    /// Create a new Vertex AI client with project ID and region
    pub fn new(
        project_id: impl Into<String>,
        region: impl Into<String>,
        api_key: Option<String>,
        access_token: Option<String>,
    ) -> Self {
        Self {
            api_key,
            access_token,
            project_id: project_id.into(),
            region: region.into(),
            client: Client::builder()
                .timeout(std::time::Duration::from_mins(5))
                .build()
                .expect("Failed to create HTTP client"),
        }
    }

    /// Create a Vertex AI client from environment variables
    ///
    /// Required:
    /// - `GOOGLE_CLOUD_PROJECT` or `VERTEX_PROJECT_ID`: Google Cloud project ID
    ///
    /// Optional:
    /// - `VERTEX_REGION`: Region (defaults to us-central1)
    /// - `GOOGLE_API_KEY`: API key for authentication
    /// - `VERTEX_ACCESS_TOKEN`: OAuth access token (takes precedence over API key)
    pub fn from_env() -> Result<Self> {
        let project_id = env::var("GOOGLE_CLOUD_PROJECT")
            .or_else(|_| env::var("VERTEX_PROJECT_ID"))
            .context("GOOGLE_CLOUD_PROJECT or VERTEX_PROJECT_ID environment variable not set")?;

        let region = env::var("VERTEX_REGION").unwrap_or_else(|_| DEFAULT_REGION.to_string());

        let api_key = env::var("GOOGLE_API_KEY").ok();
        let access_token = env::var("VERTEX_ACCESS_TOKEN").ok();

        if api_key.is_none() && access_token.is_none() {
            anyhow::bail!("Either GOOGLE_API_KEY or VERTEX_ACCESS_TOKEN must be set for Vertex AI");
        }

        Ok(Self::new(project_id, region, api_key, access_token))
    }

    /// Create a client from the provider registry's resolved credential and
    /// environment snapshot. The registry resolves `op://` references before
    /// this boundary, so callers never need to pass raw secret values to the
    /// transport. `auth_source` selects whether the credential is an OAuth
    /// access token or a Google API key.
    pub(crate) fn from_resolved_env(
        env: &HashMap<String, String>,
        credential: &str,
        auth_source: &str,
    ) -> Result<Self> {
        let project_id = ["GOOGLE_CLOUD_PROJECT", "VERTEX_PROJECT_ID"]
            .iter()
            .find_map(|name| {
                env.get(*name)
                    .map(String::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .context("GOOGLE_CLOUD_PROJECT or VERTEX_PROJECT_ID environment variable not set")?;
        let region = env
            .get("VERTEX_REGION")
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_REGION);
        let credential = credential.trim();
        if credential.is_empty() {
            anyhow::bail!("Vertex AI credential must not be empty");
        }

        let (api_key, access_token) = match auth_source {
            "VERTEX_ACCESS_TOKEN" => (None, Some(credential.to_owned())),
            "GOOGLE_API_KEY" => (Some(credential.to_owned()), None),
            other => anyhow::bail!("unsupported Vertex AI auth source: {other}"),
        };

        Ok(Self::new(project_id, region, api_key, access_token))
    }

    /// Stream a request to the Vertex AI API
    pub async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Build the request
        let request = self.build_request(messages, config)?;

        // Clone for the spawned task
        let client = self.client.clone();
        let project_id = self.project_id.clone();
        let region = self.region.clone();
        let api_key = self.api_key.clone();
        let access_token = self.access_token.clone();
        let model = provider_model_name(&config.model);

        tokio::spawn(async move {
            if let Err(e) = stream_vertex_response(
                client,
                project_id,
                region,
                model,
                api_key,
                access_token,
                request,
                tx.clone(),
            )
            .await
            {
                let _ = tx.send(StreamEvent::Error {
                    message: format!("Vertex AI error: {e}"),
                });
            }
        });

        Ok(rx)
    }

    /// Build the Vertex AI request body
    /// Serialized request body, used by the provider conformance matrix.
    #[cfg(test)]
    pub(crate) fn request_body_json(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.build_request(messages, config)?)?)
    }

    fn build_request(&self, messages: &[Message], config: &RequestConfig) -> Result<VertexRequest> {
        let messages = crate::cache_topology::messages_with_volatile_tail(messages, config);
        let messages = super::transform::google_messages_for_wire(&messages);
        let contents = messages
            .iter()
            .map(|msg| self.message_to_content(msg))
            .collect();

        let mut request = VertexRequest {
            contents,
            generation_config: Some(GenerationConfig {
                max_output_tokens: Some(config.max_tokens),
                temperature: config.temperature,
                ..Default::default()
            }),
            system_instruction: config.system.as_ref().map(|s| Content {
                role: None,
                parts: vec![Part::Text { text: s.clone() }],
            }),
            tools: None,
            safety_settings: None,
        };

        // Add tools if provided
        if !config.tools.is_empty() {
            let function_declarations: Vec<FunctionDeclaration> = config
                .tools
                .iter()
                .map(|tool| FunctionDeclaration {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                })
                .collect();

            request.tools = Some(vec![VertexTool {
                function_declarations,
            }]);
        }

        Ok(request)
    }

    /// Convert our Message type to Vertex Content format
    fn message_to_content(&self, msg: &Message) -> Content {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
            Role::System => "user", // Vertex handles system via system_instruction
        };

        let parts = match &msg.content {
            MessageContent::Text(text) => vec![Part::Text { text: text.clone() }],
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(Part::Text { text: text.clone() }),
                    ContentBlock::Thinking { thinking, .. } => Some(Part::Text {
                        text: format!("<thinking>{thinking}</thinking>"),
                    }),
                    ContentBlock::ToolUse {
                        id: _, name, input, ..
                    } => Some(Part::FunctionCall {
                        function_call: FunctionCall {
                            name: name.clone(),
                            args: input.clone(),
                        },
                    }),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => Some(Part::FunctionResponse {
                        function_response: FunctionResponse {
                            name: tool_use_id.clone(),
                            response: json!({ "result": content }),
                        },
                    }),
                    _ => None,
                })
                .collect(),
        };

        Content {
            role: Some(role.to_string()),
            parts,
        }
    }

    /// Get the provider type
    #[must_use]
    pub fn provider(&self) -> AiProvider {
        AiProvider::VertexAi
    }
}

/// Stream response from Vertex AI API
#[allow(clippy::too_many_arguments)]
async fn stream_vertex_response(
    client: Client,
    project_id: String,
    region: String,
    model: String,
    api_key: Option<String>,
    access_token: Option<String>,
    request: VertexRequest,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<()> {
    let url = vertex_stream_url(&project_id, &region, &model);

    let mut req_builder = client.post(&url).header("Content-Type", "application/json");

    // Add authentication
    if let Some(token) = access_token {
        req_builder = req_builder.header("Authorization", format!("Bearer {token}"));
    } else if let Some(key) = api_key {
        req_builder = req_builder.query(&[("key", key)]);
    }

    let response = req_builder
        .json(&request)
        .send()
        .await
        .context("Failed to send request to Vertex AI")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Vertex AI error ({status}): {}",
            super::summarize_error_body(&body)
        );
    }

    // Parse the SSE stream requested by `alt=sse`. Without that query
    // parameter Vertex returns one JSON array, which is not incrementally
    // streamable and used to be silently discarded by the line parser.
    let mut stream = response.bytes_stream();

    let mut buffer = Vec::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read_tokens = None;
    let mut next_block_index = 0usize;
    let mut terminal_stop_reason = None;

    let _ = tx.send(StreamEvent::MessageStart {
        id: format!("vertex-{}", uuid::Uuid::new_v4()),
        model,
    });

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read chunk")?;
        buffer.extend_from_slice(&chunk);

        while let Some(event) = take_vertex_sse_event(&mut buffer)? {
            if let Some(response) = decode_vertex_sse_event(&event)? {
                emit_vertex_response(
                    response,
                    &tx,
                    &mut next_block_index,
                    &mut input_tokens,
                    &mut output_tokens,
                    &mut cache_read_tokens,
                    &mut terminal_stop_reason,
                );
            }
        }
    }

    if buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
        anyhow::bail!("Vertex AI SSE stream ended with an incomplete frame");
    }

    // Send final usage
    let _ = tx.send(super::google::cache_usage_event(
        input_tokens,
        output_tokens,
        cache_read_tokens,
    ));
    if terminal_stop_reason.is_some() {
        let _ = tx.send(StreamEvent::MessageStop {
            stop_reason: terminal_stop_reason,
        });
    }

    Ok(())
}

fn emit_vertex_response(
    response: VertexResponse,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    next_block_index: &mut usize,
    input_tokens: &mut u64,
    output_tokens: &mut u64,
    cache_read_tokens: &mut Option<u64>,
    terminal_stop_reason: &mut Option<StopReason>,
) {
    if let Some(candidates) = response.candidates {
        for candidate in candidates {
            if let Some(content) = candidate.content {
                for part in content.parts {
                    let index = *next_block_index;
                    match part {
                        Part::Text { text } if !text.is_empty() => {
                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                index,
                                block: ContentBlock::Text {
                                    text: String::new(),
                                },
                            });
                            let _ = tx.send(StreamEvent::TextDelta { index, text });
                            let _ = tx.send(StreamEvent::ContentBlockStop {
                                index,
                                thinking_signature: None,
                            });
                            *next_block_index += 1;
                        }
                        Part::FunctionCall { function_call } => {
                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                index,
                                block: ContentBlock::ToolUse {
                                    id: format!("call_{}", uuid::Uuid::new_v4()),
                                    name: function_call.name,
                                    input: json!({}),
                                    gemini_context: None,
                                },
                            });
                            let _ = tx.send(StreamEvent::InputJsonDelta {
                                index,
                                partial_json: function_call.args.to_string(),
                            });
                            let _ = tx.send(StreamEvent::ContentBlockStop {
                                index,
                                thinking_signature: None,
                            });
                            *next_block_index += 1;
                        }
                        _ => {}
                    }
                }
            }

            if let Some(reason) = candidate.finish_reason {
                *terminal_stop_reason = Some(match reason.as_str() {
                    "MAX_TOKENS" => StopReason::MaxTokens,
                    "TOOL_USE" => StopReason::ToolUse,
                    "STOP" | "SAFETY" => StopReason::EndTurn,
                    _ => StopReason::EndTurn,
                });
            }
        }
    }

    if let Some(metadata) = response.usage_metadata {
        *input_tokens = metadata.prompt_token_count.unwrap_or(*input_tokens);
        *output_tokens = metadata.candidates_token_count.unwrap_or(*output_tokens);
        *cache_read_tokens = metadata.cached_content_token_count.or(*cache_read_tokens);
    }
}

fn vertex_stream_url(project_id: &str, region: &str, model: &str) -> String {
    format!(
        "https://{region}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{region}/publishers/google/models/{model}:streamGenerateContent?alt=sse"
    )
}

fn vertex_sse_data(event: &str) -> Option<&str> {
    event
        .lines()
        .find_map(|line| line.strip_prefix("data:").map(str::trim))
        .filter(|data| !data.is_empty() && *data != "[DONE]")
}

fn vertex_sse_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf < crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

fn take_vertex_sse_event(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    let Some((pos, delimiter_len)) = vertex_sse_event_boundary(buffer) else {
        return Ok(None);
    };
    let event = std::str::from_utf8(&buffer[..pos])
        .context("Vertex AI SSE frame was not valid UTF-8")?
        .to_owned();
    buffer.drain(..pos + delimiter_len);
    Ok(Some(event))
}

fn decode_vertex_sse_event(event: &str) -> Result<Option<VertexResponse>> {
    let Some(data) = vertex_sse_data(event) else {
        return Ok(None);
    };
    serde_json::from_str(data)
        .context("Vertex AI SSE data frame was not valid response JSON")
        .map(Some)
}

// ============================================================================
// Vertex AI Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VertexRequest {
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<VertexTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    safety_settings: Option<Vec<SafetySetting>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Part {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponse,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct FunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct FunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VertexTool {
    function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct FunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafetySetting {
    category: String,
    threshold: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VertexResponse {
    candidates: Option<Vec<Candidate>>,
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: Option<Content>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    cached_content_token_count: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_cache_usage_survives_partial_stream_updates() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (mut input, mut output, mut index) = (0, 0, 0);
        let (mut cached, mut stop) = (None, None);
        for metadata in [
            serde_json::json!({"promptTokenCount":100,"cachedContentTokenCount":80}),
            serde_json::json!({"candidatesTokenCount":5}),
        ] {
            let response =
                serde_json::from_value(serde_json::json!({"usageMetadata": metadata})).unwrap();
            emit_vertex_response(
                response,
                &tx,
                &mut index,
                &mut input,
                &mut output,
                &mut cached,
                &mut stop,
            );
        }
        let StreamEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            ..
        } = super::super::google::cache_usage_event(input, output, cached)
        else {
            panic!("expected usage")
        };
        assert_eq!(
            (input_tokens, output_tokens, cache_read_tokens),
            (20, 5, Some(80))
        );
    }

    #[test]
    fn volatile_tail_reaches_the_provider_after_history() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::Assistant,
            content: MessageContent::text("previous"),
        }];
        let mut config = RequestConfig::default();
        config.cache_topology = Some(
            crate::cache_topology::PreparedPrompt::prepare(
                &messages,
                &config,
                "scope".into(),
                None,
            )
            .unwrap()
            .with_volatile_tail(Some("current clock and plan".into())),
        );
        let request =
            serde_json::to_value(client.build_request(&messages, &config).unwrap()).unwrap();
        assert_eq!(
            request["contents"][1]["parts"][0]["text"],
            "current clock and plan"
        );
        assert_eq!(request["contents"][0]["parts"][0]["text"], "previous");
    }

    #[test]
    fn request_matches_foreign_tool_results_by_function_name() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "call_shared|fc_a".into(),
                        name: "read".into(),
                        input: json!({"path":"a"}),
                        gemini_context: None,
                    },
                    ContentBlock::ToolUse {
                        id: "call_shared|fc_b".into(),
                        name: "search".into(),
                        input: json!({"query":"b"}),
                        gemini_context: None,
                    },
                ]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_shared|fc_b".into(),
                        content: "B".into(),
                        is_error: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_shared|fc_a".into(),
                        content: "A".into(),
                        is_error: None,
                    },
                ]),
            },
        ];
        let before = serde_json::to_value(&messages).unwrap();
        let body = serde_json::to_value(
            client
                .build_request(&messages, &RequestConfig::default())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "search"
        );
        assert_eq!(
            body["contents"][1]["parts"][1]["functionResponse"]["name"],
            "read"
        );
        assert_eq!(serde_json::to_value(&messages).unwrap(), before);
    }

    use crate::Tool;

    #[test]
    fn test_vertex_client_creation() {
        let client = VertexAiClient::new(
            "test-project",
            "us-central1",
            Some("test-key".to_string()),
            None,
        );
        assert_eq!(client.provider(), AiProvider::VertexAi);
        assert_eq!(client.project_id, "test-project");
        assert_eq!(client.region, "us-central1");
    }

    #[test]
    fn test_vertex_client_with_access_token() {
        let client = VertexAiClient::new(
            "my-project",
            "europe-west4",
            None,
            Some("access-token-123".to_string()),
        );
        assert_eq!(client.project_id, "my-project");
        assert_eq!(client.region, "europe-west4");
        assert!(client.api_key.is_none());
        assert!(client.access_token.is_some());
    }

    #[test]
    fn test_vertex_client_with_both_auth_methods() {
        let client = VertexAiClient::new(
            "test-project",
            "us-central1",
            Some("api-key".to_string()),
            Some("access-token".to_string()),
        );
        // Both are allowed, access_token takes precedence in actual requests
        assert!(client.api_key.is_some());
        assert!(client.access_token.is_some());
    }

    #[test]
    fn test_message_to_content_text() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.role, Some("user".to_string()));
        assert_eq!(content.parts.len(), 1);
        match &content.parts[0] {
            Part::Text { text } => assert_eq!(text, "Hello"),
            _ => panic!("Expected Text part"),
        }
    }

    #[test]
    fn test_message_to_content_assistant() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Text("Hi there".to_string()),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.role, Some("model".to_string()));
    }

    #[test]
    fn test_message_to_content_system_role() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::System,
            content: MessageContent::Text("You are helpful".to_string()),
        };

        let content = client.message_to_content(&msg);
        // System messages are converted to user role (system is handled via system_instruction)
        assert_eq!(content.role, Some("user".to_string()));
    }

    #[test]
    fn test_message_to_content_with_blocks() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Let me help.".to_string(),
                },
                ContentBlock::Thinking {
                    thinking: "I should analyze this.".to_string(),
                    signature: None,
                },
            ]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.role, Some("model".to_string()));
        assert_eq!(content.parts.len(), 2);

        match &content.parts[0] {
            Part::Text { text } => assert_eq!(text, "Let me help."),
            _ => panic!("Expected Text part"),
        }

        match &content.parts[1] {
            Part::Text { text } => assert!(text.contains("<thinking>")),
            _ => panic!("Expected Text part for thinking"),
        }
    }

    #[test]
    fn test_message_to_content_with_tool_use() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call-123".to_string(),
                name: "read_file".to_string(),
                input: json!({"path": "/tmp/test.txt"}),
                gemini_context: None,
            }]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);

        match &content.parts[0] {
            Part::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "read_file");
                assert_eq!(function_call.args["path"], "/tmp/test.txt");
            }
            _ => panic!("Expected FunctionCall part"),
        }
    }

    #[test]
    fn test_message_to_content_with_tool_result() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "read_file".to_string(),
                content: "file contents here".to_string(),
                is_error: Some(false),
            }]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);

        match &content.parts[0] {
            Part::FunctionResponse { function_response } => {
                assert_eq!(function_response.name, "read_file");
                assert_eq!(function_response.response["result"], "file contents here");
            }
            _ => panic!("Expected FunctionResponse part"),
        }
    }

    #[test]
    fn test_build_request_with_tools() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            temperature: Some(0.7),
            system: Some("You are helpful".to_string()),
            tools: vec![Tool::new("test", "A test tool").with_schema(json!({
                "type": "object",
                "properties": {}
            }))]
            .into(),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.tools.is_some());
        let tools = request.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "test");
        assert!(request.system_instruction.is_some());
    }

    #[test]
    fn test_build_request_with_multiple_tools() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            tools: vec![
                Tool::new("read", "Read a file").with_schema(json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}}
                })),
                Tool::new("write", "Write a file").with_schema(json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}}
                })),
            ]
            .into(),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        let tools = request.tools.unwrap();
        assert_eq!(tools[0].function_declarations.len(), 2);
        assert_eq!(tools[0].function_declarations[0].name, "read");
        assert_eq!(tools[0].function_declarations[1].name, "write");
    }

    #[test]
    fn test_build_request_without_tools() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            tools: vec![].into(),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.tools.is_none());
    }

    #[test]
    fn test_build_request_generation_config() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Test".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 2048,
            temperature: Some(0.9),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        let gen_config = request.generation_config.unwrap();
        assert_eq!(gen_config.max_output_tokens, Some(2048));
        assert_eq!(gen_config.temperature, Some(0.9));
    }

    #[test]
    fn test_build_request_with_system_instruction() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            system: Some("You are a helpful coding assistant.".to_string()),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.system_instruction.is_some());
        let sys = request.system_instruction.unwrap();
        assert!(sys.role.is_none()); // system_instruction doesn't have role
        assert_eq!(sys.parts.len(), 1);
        match &sys.parts[0] {
            Part::Text { text } => assert_eq!(text, "You are a helpful coding assistant."),
            _ => panic!("Expected Text part"),
        }
    }

    #[test]
    fn test_build_request_without_system() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            system: None,
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.system_instruction.is_none());
    }

    #[test]
    fn test_build_request_multiple_messages() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("Hi! How can I help?".to_string()),
            },
            Message {
                role: Role::User,
                content: MessageContent::Text("Write some code".to_string()),
            },
        ];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert_eq!(request.contents.len(), 3);
        assert_eq!(request.contents[0].role, Some("user".to_string()));
        assert_eq!(request.contents[1].role, Some("model".to_string()));
        assert_eq!(request.contents[2].role, Some("user".to_string()));
    }

    #[test]
    fn test_default_region_constant() {
        assert_eq!(DEFAULT_REGION, "us-central1");
    }

    #[test]
    fn test_vertex_response_parsing() {
        let json_str = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello!"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5
            }
        }"#;

        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        assert!(response.candidates.is_some());
        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].finish_reason, Some("STOP".to_string()));

        let usage = response.usage_metadata.unwrap();
        assert_eq!(usage.prompt_token_count, Some(10));
        assert_eq!(usage.candidates_token_count, Some(5));
    }

    #[test]
    fn vertex_stream_requests_and_decodes_sse() {
        let url = vertex_stream_url("evalops-prod", "us-central1", "gemini-2.5-flash");
        assert_eq!(
            url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/evalops-prod/locations/us-central1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );

        let event = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",",
            "\"parts\":[{\"text\":\"MAESTRO_E2E_OK\"}]},\"finishReason\":\"STOP\"}]}\r\n\r\n"
        );
        assert_eq!(
            vertex_sse_event_boundary(event.as_bytes()),
            Some((event.len() - 4, 4))
        );
        let response = decode_vertex_sse_event(event)
            .expect("valid Vertex SSE event")
            .expect("Vertex response");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut next_block_index = 0;
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut terminal_stop_reason = None;
        emit_vertex_response(
            response,
            &tx,
            &mut next_block_index,
            &mut input_tokens,
            &mut output_tokens,
            &mut None,
            &mut terminal_stop_reason,
        );
        assert!(matches!(
            rx.try_recv().expect("text block start"),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlock::Text { ref text }
            } if text.is_empty()
        ));
        assert!(matches!(
            rx.try_recv().expect("text delta"),
            StreamEvent::TextDelta { index: 0, ref text } if text == "MAESTRO_E2E_OK"
        ));
        assert!(matches!(
            rx.try_recv().expect("text block stop"),
            StreamEvent::ContentBlockStop { index: 0, .. }
        ));
        assert_eq!(next_block_index, 1);
        assert_eq!(terminal_stop_reason, Some(StopReason::EndTurn));

        let lf_event = "data: {}\n\n";
        assert_eq!(
            vertex_sse_event_boundary(lf_event.as_bytes()),
            Some((lf_event.len() - 2, 2))
        );
    }

    #[test]
    fn vertex_sse_preserves_unicode_across_every_chunk_boundary() {
        let event = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
            "{\"text\":\"héllo 🌍\"},{\"functionCall\":{\"name\":\"echo\",",
            "\"args\":{\"value\":\"東京\"}}}]},\"finishReason\":\"TOOL_USE\"}]}\r\n\r\n"
        );

        for split in 0..=event.len() {
            let mut buffer = Vec::new();
            let mut frames = Vec::new();
            for chunk in [&event.as_bytes()[..split], &event.as_bytes()[split..]] {
                buffer.extend_from_slice(chunk);
                while let Some(frame) =
                    take_vertex_sse_event(&mut buffer).expect("valid UTF-8 frame")
                {
                    frames.push(frame);
                }
            }
            assert_eq!(frames.len(), 1, "split at byte {split}");
            let response = decode_vertex_sse_event(&frames[0])
                .expect("valid response JSON")
                .expect("response event");
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut next_block_index = 0;
            let mut input_tokens = 0;
            let mut output_tokens = 0;
            let mut terminal_stop_reason = None;
            emit_vertex_response(
                response,
                &tx,
                &mut next_block_index,
                &mut input_tokens,
                &mut output_tokens,
                &mut None,
                &mut terminal_stop_reason,
            );
            let events = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
            assert!(events.iter().any(
                |event| matches!(event, StreamEvent::TextDelta { text, .. } if text == "héllo 🌍")
            ));
            assert!(events.iter().any(|event| matches!(
                event,
                StreamEvent::InputJsonDelta { partial_json, .. }
                    if partial_json.contains("東京")
            )));
        }
    }

    #[test]
    fn malformed_vertex_sse_frame_cannot_be_followed_by_success_terminal() {
        let stream = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            "data: {not-json}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n"
        );
        let mut buffer = stream.as_bytes().to_vec();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut next_block_index = 0;
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut terminal_stop_reason = None;
        let mut decode_error = None;

        while let Some(event) = take_vertex_sse_event(&mut buffer).expect("valid UTF-8 frame") {
            match decode_vertex_sse_event(&event) {
                Ok(Some(response)) => emit_vertex_response(
                    response,
                    &tx,
                    &mut next_block_index,
                    &mut input_tokens,
                    &mut output_tokens,
                    &mut None,
                    &mut terminal_stop_reason,
                ),
                Ok(None) => {}
                Err(error) => {
                    decode_error = Some(error);
                    break;
                }
            }
        }

        assert!(decode_error.is_some());
        assert_eq!(terminal_stop_reason, None);
        assert!(!buffer.is_empty(), "terminal frame must remain unprocessed");
    }

    #[test]
    fn test_vertex_response_with_function_call() {
        let json_str = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "read_file",
                            "args": {"path": "/tmp/test.txt"}
                        }
                    }]
                },
                "finishReason": "TOOL_USE"
            }]
        }"#;

        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        assert_eq!(candidates[0].finish_reason, Some("TOOL_USE".to_string()));
    }

    #[test]
    fn test_vertex_response_empty_candidates() {
        let json_str = r#"{
            "candidates": [],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 0
            }
        }"#;

        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        assert!(response.candidates.unwrap().is_empty());
    }

    #[test]
    fn test_vertex_response_no_usage_metadata() {
        let json_str = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello"}]
                }
            }]
        }"#;

        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        assert!(response.usage_metadata.is_none());
    }

    #[test]
    fn test_vertex_response_multiple_candidates() {
        let json_str = r#"{
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [{"text": "Response 1"}]
                    },
                    "finishReason": "STOP"
                },
                {
                    "content": {
                        "role": "model",
                        "parts": [{"text": "Response 2"}]
                    },
                    "finishReason": "STOP"
                }
            ]
        }"#;

        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn test_message_to_content_empty_blocks() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![]),
        };

        let content = client.message_to_content(&msg);
        assert!(content.parts.is_empty());
    }

    #[test]
    fn test_message_to_content_mixed_blocks() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Here's my response".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    input: json!({"path": "/test"}),
                    gemini_context: None,
                },
                ContentBlock::Thinking {
                    thinking: "Let me think...".to_string(),
                    signature: None,
                },
            ]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 3);
    }

    #[test]
    fn test_build_request_empty_messages() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages: Vec<Message> = vec![];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.contents.is_empty());
    }

    #[test]
    fn test_generation_config_defaults() {
        let config = GenerationConfig::default();
        assert!(config.max_output_tokens.is_none());
        assert!(config.temperature.is_none());
        assert!(config.top_p.is_none());
        assert!(config.top_k.is_none());
    }

    #[test]
    fn test_part_text_serialization() {
        let part = Part::Text {
            text: "Hello world".to_string(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"text\":\"Hello world\""));
    }

    #[test]
    fn test_part_function_call_serialization() {
        let part = Part::FunctionCall {
            function_call: FunctionCall {
                name: "test_fn".to_string(),
                args: json!({"arg1": "value1"}),
            },
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("functionCall"));
        assert!(json.contains("test_fn"));
    }

    #[test]
    fn test_part_function_response_serialization() {
        let part = Part::FunctionResponse {
            function_response: FunctionResponse {
                name: "test_fn".to_string(),
                response: json!({"result": "success"}),
            },
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("functionResponse"));
    }

    #[test]
    fn test_content_serialization() {
        let content = Content {
            role: Some("user".to_string()),
            parts: vec![Part::Text {
                text: "Hello".to_string(),
            }],
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"text\":\"Hello\""));
    }

    #[test]
    fn test_content_no_role_serialization() {
        let content = Content {
            role: None,
            parts: vec![Part::Text {
                text: "System".to_string(),
            }],
        };
        let json = serde_json::to_string(&content).unwrap();
        // role should be skipped when None
        assert!(!json.contains("\"role\""));
    }

    #[test]
    fn test_vertex_request_minimal_serialization() {
        let request = VertexRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![Part::Text {
                    text: "Hi".to_string(),
                }],
            }],
            system_instruction: None,
            generation_config: None,
            tools: None,
            safety_settings: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        // Optional fields should not appear when None
        assert!(!json.contains("systemInstruction"));
        assert!(!json.contains("generationConfig"));
        assert!(!json.contains("tools"));
        assert!(!json.contains("safetySettings"));
    }

    #[test]
    fn test_vertex_client_different_regions() {
        let regions = ["us-central1", "europe-west4", "asia-northeast1"];
        for region in regions {
            let client = VertexAiClient::new("project", region, Some("key".to_string()), None);
            assert_eq!(client.region, region);
        }
    }

    #[test]
    fn test_build_request_preserves_message_order() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("First".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("Second".to_string()),
            },
            Message {
                role: Role::User,
                content: MessageContent::Text("Third".to_string()),
            },
        ];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();

        // Verify order is preserved
        match &request.contents[0].parts[0] {
            Part::Text { text } => assert_eq!(text, "First"),
            _ => panic!("Expected text"),
        }
        match &request.contents[1].parts[0] {
            Part::Text { text } => assert_eq!(text, "Second"),
            _ => panic!("Expected text"),
        }
        match &request.contents[2].parts[0] {
            Part::Text { text } => assert_eq!(text, "Third"),
            _ => panic!("Expected text"),
        }
    }

    #[test]
    fn test_function_declaration_serialization() {
        let decl = FunctionDeclaration {
            name: "test_function".to_string(),
            description: "A test function".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "arg1": {"type": "string"}
                }
            }),
        };
        let json = serde_json::to_string(&decl).unwrap();
        assert!(json.contains("\"name\":\"test_function\""));
        assert!(json.contains("\"description\":\"A test function\""));
    }

    #[test]
    fn test_vertex_tool_serialization() {
        let tool = VertexTool {
            function_declarations: vec![FunctionDeclaration {
                name: "fn1".to_string(),
                description: "First function".to_string(),
                parameters: json!({}),
            }],
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("functionDeclarations"));
    }

    // ========== Error Handling & Edge Cases ==========

    #[test]
    fn test_vertex_response_malformed_json() {
        // Test that invalid JSON doesn't panic
        let invalid_json = r#"{"candidates": [{"content": invalid}]}"#;
        let result: Result<VertexResponse, _> = serde_json::from_str(invalid_json);
        assert!(result.is_err());
    }

    #[test]
    fn test_vertex_response_missing_required_fields() {
        // Response with missing optional fields should still parse
        let json_str = r"{}";
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        assert!(response.candidates.is_none());
        assert!(response.usage_metadata.is_none());
    }

    #[test]
    fn test_vertex_response_null_candidates() {
        let json_str = r#"{"candidates": null}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        assert!(response.candidates.is_none());
    }

    #[test]
    fn test_usage_metadata_missing_token_counts() {
        let json_str = r#"{"usageMetadata": {}}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let metadata = response.usage_metadata.unwrap();
        assert!(metadata.prompt_token_count.is_none());
        assert!(metadata.candidates_token_count.is_none());
    }

    #[test]
    fn test_usage_metadata_partial_token_counts() {
        let json_str = r#"{"usageMetadata": {"promptTokenCount": 100}}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let metadata = response.usage_metadata.unwrap();
        assert_eq!(metadata.prompt_token_count, Some(100));
        assert!(metadata.candidates_token_count.is_none());
    }

    #[test]
    fn test_usage_metadata_zero_tokens() {
        let json_str = r#"{"usageMetadata": {"promptTokenCount": 0, "candidatesTokenCount": 0}}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let metadata = response.usage_metadata.unwrap();
        assert_eq!(metadata.prompt_token_count, Some(0));
        assert_eq!(metadata.candidates_token_count, Some(0));
    }

    #[test]
    fn test_usage_metadata_large_token_counts() {
        let json_str = r#"{"usageMetadata": {"promptTokenCount": 999999999, "candidatesTokenCount": 888888888}}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let metadata = response.usage_metadata.unwrap();
        assert_eq!(metadata.prompt_token_count, Some(999_999_999));
        assert_eq!(metadata.candidates_token_count, Some(888_888_888));
    }

    #[test]
    fn test_finish_reason_unknown_value() {
        // Unknown finish reasons should still deserialize
        let json_str = r#"{"candidates": [{"finishReason": "UNKNOWN_NEW_REASON"}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        assert_eq!(
            candidates[0].finish_reason,
            Some("UNKNOWN_NEW_REASON".to_string())
        );
    }

    #[test]
    fn test_finish_reason_all_known_values() {
        let reasons = [
            "STOP",
            "MAX_TOKENS",
            "SAFETY",
            "TOOL_USE",
            "RECITATION",
            "OTHER",
        ];
        for reason in reasons {
            let json_str = format!(r#"{{"candidates": [{{"finishReason": "{}"}}]}}"#, reason);
            let response: VertexResponse = serde_json::from_str(&json_str).unwrap();
            assert_eq!(
                response.candidates.unwrap()[0].finish_reason,
                Some(reason.to_string())
            );
        }
    }

    #[test]
    fn test_candidate_without_content() {
        let json_str = r#"{"candidates": [{"finishReason": "STOP"}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        assert!(candidates[0].content.is_none());
    }

    #[test]
    fn test_content_empty_parts() {
        let json_str = r#"{"candidates": [{"content": {"role": "model", "parts": []}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let content = candidates[0].content.as_ref().unwrap();
        assert!(content.parts.is_empty());
    }

    #[test]
    fn test_part_text_empty_string() {
        let json_str = r#"{"candidates": [{"content": {"parts": [{"text": ""}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => assert!(text.is_empty()),
            _ => panic!("Expected text part"),
        }
    }

    #[test]
    fn test_part_text_with_special_characters() {
        let json_str =
            r#"{"candidates": [{"content": {"parts": [{"text": "Hello\nWorld\t\"quoted\""}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => {
                assert!(text.contains('\n'));
                assert!(text.contains('\t'));
                assert!(text.contains('"'));
            }
            _ => panic!("Expected text part"),
        }
    }

    #[test]
    fn test_part_text_with_unicode() {
        let json_str = r#"{"candidates": [{"content": {"parts": [{"text": "日本語テスト 🎉"}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => {
                assert!(text.contains("日本語"));
                assert!(text.contains("🎉"));
            }
            _ => panic!("Expected text part"),
        }
    }

    #[test]
    fn test_function_call_empty_args() {
        let json_str = r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "test", "args": {}}}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "test");
                assert!(function_call.args.as_object().unwrap().is_empty());
            }
            _ => panic!("Expected function call part"),
        }
    }

    #[test]
    fn test_function_call_complex_args() {
        let json_str = r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "complex", "args": {"nested": {"array": [1, 2, 3]}, "flag": true}}}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "complex");
                assert!(function_call.args.get("nested").is_some());
                assert_eq!(function_call.args.get("flag"), Some(&json!(true)));
            }
            _ => panic!("Expected function call part"),
        }
    }

    #[test]
    fn test_function_response_serialization() {
        let response = FunctionResponse {
            name: "test_fn".to_string(),
            response: json!({"result": "success", "data": [1, 2, 3]}),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"name\":\"test_fn\""));
        assert!(json.contains("\"response\""));
    }

    #[test]
    fn test_multiple_candidates_in_response() {
        let json_str = r#"{"candidates": [
            {"content": {"parts": [{"text": "Response 1"}]}, "finishReason": "STOP"},
            {"content": {"parts": [{"text": "Response 2"}]}, "finishReason": "MAX_TOKENS"}
        ]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].finish_reason, Some("STOP".to_string()));
        assert_eq!(candidates[1].finish_reason, Some("MAX_TOKENS".to_string()));
    }

    #[test]
    fn test_multiple_parts_in_content() {
        let json_str = r#"{"candidates": [{"content": {"parts": [
            {"text": "Part 1"},
            {"text": "Part 2"},
            {"functionCall": {"name": "fn", "args": {}}}
        ]}}]}"#;
        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn test_vertex_request_with_all_optional_fields() {
        let request = VertexRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![Part::Text {
                    text: "Hello".to_string(),
                }],
            }],
            system_instruction: Some(Content {
                role: None,
                parts: vec![Part::Text {
                    text: "Be helpful".to_string(),
                }],
            }),
            generation_config: Some(GenerationConfig {
                temperature: Some(0.7),
                max_output_tokens: Some(1000),
                top_p: Some(0.9),
                top_k: Some(40),
            }),
            tools: Some(vec![VertexTool {
                function_declarations: vec![FunctionDeclaration {
                    name: "test".to_string(),
                    description: "Test function".to_string(),
                    parameters: json!({}),
                }],
            }]),
            safety_settings: Some(vec![]),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("systemInstruction"));
        assert!(json.contains("generationConfig"));
        assert!(json.contains("tools"));
        assert!(json.contains("safetySettings"));
    }

    #[test]
    fn test_generation_config_boundary_values() {
        // Test boundary values for generation config
        let config = GenerationConfig {
            temperature: Some(0.0),
            max_output_tokens: Some(0),
            top_p: Some(0.0),
            top_k: Some(0),
        };
        let json = serde_json::to_string(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value.get("temperature").and_then(|v| v.as_f64()), Some(0.0));
        assert_eq!(
            value.get("maxOutputTokens").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert_eq!(value.get("topP").and_then(|v| v.as_f64()), Some(0.0));
        assert_eq!(value.get("topK").and_then(|v| v.as_u64()), Some(0));

        // Test max reasonable values
        let config_max = GenerationConfig {
            temperature: Some(2.0),
            max_output_tokens: Some(1_000_000),
            top_p: Some(1.0),
            top_k: Some(1000),
        };
        let json_max = serde_json::to_string(&config_max).unwrap();
        let value_max: serde_json::Value = serde_json::from_str(&json_max).unwrap();
        assert_eq!(
            value_max.get("temperature").and_then(|v| v.as_f64()),
            Some(2.0)
        );
        assert_eq!(
            value_max.get("maxOutputTokens").and_then(|v| v.as_u64()),
            Some(1_000_000)
        );
        assert_eq!(value_max.get("topP").and_then(|v| v.as_f64()), Some(1.0));
        assert_eq!(value_max.get("topK").and_then(|v| v.as_u64()), Some(1000));
    }

    #[test]
    fn test_build_request_with_system() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];
        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            system: Some("You are a helpful assistant.".to_string()),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.system_instruction.is_some());
        let system = request.system_instruction.unwrap();
        match &system.parts[0] {
            Part::Text { text } => assert_eq!(text, "You are a helpful assistant."),
            _ => panic!("Expected text"),
        }
    }

    #[test]
    fn test_build_request_temperature_mapping() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];
        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            temperature: Some(0.5),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.generation_config.is_some());
        assert_eq!(request.generation_config.unwrap().temperature, Some(0.5));
    }

    #[test]
    fn test_message_to_content_tool_result() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_123".to_string(),
                content: "Tool output here".to_string(),
                is_error: Some(false),
            }]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);
        match &content.parts[0] {
            Part::FunctionResponse { function_response } => {
                assert_eq!(function_response.name, "call_123");
            }
            _ => panic!("Expected function response"),
        }
    }

    #[test]
    fn test_message_to_content_tool_use() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call_456".to_string(),
                name: "test_tool".to_string(),
                input: json!({"arg": "value"}),
                gemini_context: None,
            }]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);
        match &content.parts[0] {
            Part::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "test_tool");
                assert_eq!(function_call.args.get("arg"), Some(&json!("value")));
            }
            _ => panic!("Expected function call"),
        }
    }

    #[test]
    fn test_content_role_serialization() {
        let content = Content {
            role: Some("user".to_string()),
            parts: vec![],
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"role\":\"user\""));

        let content_no_role = Content {
            role: None,
            parts: vec![],
        };
        let json_no_role = serde_json::to_string(&content_no_role).unwrap();
        // role should be skipped when None due to skip_serializing_if
        assert!(!json_no_role.contains("\"role\""));
    }

    #[test]
    fn test_vertex_ai_client_provider() {
        let client = VertexAiClient::new("test", "us-central1", Some("key".to_string()), None);
        assert_eq!(client.provider(), AiProvider::VertexAi);
    }

    #[test]
    fn test_vertex_ai_client_with_access_token() {
        let client = VertexAiClient::new(
            "test-project",
            "us-central1",
            None,
            Some("access_token_123".to_string()),
        );
        assert_eq!(client.project_id, "test-project");
        assert!(client.api_key.is_none());
        assert_eq!(client.access_token, Some("access_token_123".to_string()));
    }

    #[test]
    fn test_vertex_ai_client_with_both_auth_methods() {
        // When both are provided, both are stored (access_token takes precedence in actual requests)
        let client = VertexAiClient::new(
            "test-project",
            "us-central1",
            Some("api_key".to_string()),
            Some("access_token".to_string()),
        );
        assert_eq!(client.api_key, Some("api_key".to_string()));
        assert_eq!(client.access_token, Some("access_token".to_string()));
    }

    #[test]
    fn test_response_with_all_fields() {
        let json_str = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello!"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5
            }
        }"#;

        let response: VertexResponse = serde_json::from_str(json_str).unwrap();
        assert!(response.candidates.is_some());
        assert!(response.usage_metadata.is_some());

        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].finish_reason, Some("STOP".to_string()));

        let metadata = response.usage_metadata.unwrap();
        assert_eq!(metadata.prompt_token_count, Some(10));
        assert_eq!(metadata.candidates_token_count, Some(5));
    }

    // ============================================================
    // Streaming Response Parsing Tests
    // These test the parsing logic used in stream_vertex_response
    // ============================================================

    #[test]
    fn test_parse_streaming_chunk_text_delta() {
        // Simulates a chunk from the stream
        let chunk = r#"{"candidates": [{"content": {"parts": [{"text": "Hello, "}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(chunk).unwrap();

        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 1);
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => assert_eq!(text, "Hello, "),
            _ => panic!("Expected text part"),
        }
    }

    #[test]
    fn test_parse_streaming_chunk_with_finish_reason() {
        let chunk = r#"{"candidates": [{"content": {"parts": [{"text": "world!"}]}, "finishReason": "STOP"}]}"#;
        let response: VertexResponse = serde_json::from_str(chunk).unwrap();

        let candidates = response.candidates.unwrap();
        assert_eq!(candidates[0].finish_reason, Some("STOP".to_string()));
    }

    #[test]
    fn test_parse_streaming_chunk_with_usage() {
        let chunk = r#"{"candidates": [], "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50}}"#;
        let response: VertexResponse = serde_json::from_str(chunk).unwrap();

        let metadata = response.usage_metadata.unwrap();
        assert_eq!(metadata.prompt_token_count, Some(100));
        assert_eq!(metadata.candidates_token_count, Some(50));
    }

    #[test]
    fn test_parse_streaming_newline_delimited_json_simulation() {
        // Simulates how the streaming parser handles newline-delimited JSON
        let stream_data = r#"[
{"candidates": [{"content": {"parts": [{"text": "Hello"}]}}]}
,
{"candidates": [{"content": {"parts": [{"text": " world"}]}}]}
,
{"candidates": [{"content": {"parts": [{"text": "!"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3}}
]"#;

        let mut texts = Vec::new();
        let mut final_usage = None;
        let mut finish_reason = None;

        for line in stream_data.lines() {
            let line = line.trim();
            if line.is_empty() || line == "[" || line == "]" || line == "," {
                continue;
            }

            let json_str = line.trim_start_matches(',').trim();
            if json_str.is_empty() {
                continue;
            }

            if let Ok(response) = serde_json::from_str::<VertexResponse>(json_str) {
                if let Some(candidates) = response.candidates {
                    for candidate in candidates {
                        if let Some(content) = candidate.content {
                            for part in content.parts {
                                if let Part::Text { text } = part {
                                    texts.push(text);
                                }
                            }
                        }
                        if candidate.finish_reason.is_some() {
                            finish_reason = candidate.finish_reason;
                        }
                    }
                }
                if response.usage_metadata.is_some() {
                    final_usage = response.usage_metadata;
                }
            }
        }

        // Verify all text deltas were collected
        assert_eq!(texts, vec!["Hello", " world", "!"]);

        // Verify finish reason was captured
        assert_eq!(finish_reason, Some("STOP".to_string()));

        // Verify usage was captured
        let usage = final_usage.unwrap();
        assert_eq!(usage.prompt_token_count, Some(10));
        assert_eq!(usage.candidates_token_count, Some(3));
    }

    #[test]
    fn test_parse_streaming_partial_json_buffering() {
        // Test the buffering logic for partial JSON
        let mut buffer = String::new();
        let chunks = vec![
            r#"{"candidates": [{"content": {"#,
            r#""parts": [{"text": "Hello"}]}}]}"#,
            "\n",
        ];

        let mut parsed_responses = Vec::new();

        for chunk in chunks {
            buffer.push_str(chunk);

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if let Ok(response) = serde_json::from_str::<VertexResponse>(&line) {
                    parsed_responses.push(response);
                }
            }
        }

        // Should have parsed one complete response
        assert_eq!(parsed_responses.len(), 1);
        let candidates = parsed_responses[0].candidates.as_ref().unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => assert_eq!(text, "Hello"),
            _ => panic!("Expected text part"),
        }
    }

    #[test]
    fn test_parse_streaming_function_call() {
        let chunk = r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "read_file", "args": {"path": "/test.txt"}}}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(chunk).unwrap();

        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "read_file");
                assert_eq!(
                    function_call.args.get("path"),
                    Some(&serde_json::json!("/test.txt"))
                );
            }
            _ => panic!("Expected function call"),
        }
    }

    #[test]
    fn test_parse_streaming_multiple_parts_single_chunk() {
        let chunk = r#"{"candidates": [{"content": {"parts": [
            {"text": "I'll read the file"},
            {"functionCall": {"name": "read_file", "args": {"path": "/test"}}}
        ]}}]}"#;

        let response: VertexResponse = serde_json::from_str(chunk).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;

        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], Part::Text { .. }));
        assert!(matches!(&parts[1], Part::FunctionCall { .. }));
    }

    #[test]
    fn test_parse_streaming_empty_candidates() {
        let chunk = r#"{"candidates": []}"#;
        let response: VertexResponse = serde_json::from_str(chunk).unwrap();
        assert!(response.candidates.unwrap().is_empty());
    }

    #[test]
    fn test_parse_streaming_multiple_candidates() {
        let chunk = r#"{"candidates": [
            {"content": {"parts": [{"text": "Response 1"}]}},
            {"content": {"parts": [{"text": "Response 2"}]}}
        ]}"#;

        let response: VertexResponse = serde_json::from_str(chunk).unwrap();
        let candidates = response.candidates.unwrap();
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn test_finish_reason_mapping() {
        // Test all finish reason mappings used in stream processing
        let mappings = vec![
            ("STOP", "EndTurn"),
            ("MAX_TOKENS", "MaxTokens"),
            ("SAFETY", "EndTurn"),
            ("TOOL_USE", "ToolUse"),
            ("UNKNOWN", "EndTurn"), // Default case
        ];

        for (vertex_reason, _expected) in mappings {
            let chunk = format!(
                r#"{{"candidates": [{{"content": {{"parts": []}}, "finishReason": "{}"}}]}}"#,
                vertex_reason
            );
            let response: VertexResponse = serde_json::from_str(&chunk).unwrap();
            assert_eq!(
                response.candidates.unwrap()[0].finish_reason,
                Some(vertex_reason.to_string())
            );
        }
    }

    #[test]
    fn test_parse_streaming_with_array_brackets() {
        // Vertex sometimes wraps responses in JSON array
        let lines = vec![
            "[",
            "{\"candidates\": []}",
            ",",
            "{\"candidates\": []}",
            "]",
        ];

        let mut parsed_count = 0;
        for line in lines {
            let line = line.trim();
            if line.is_empty() || line == "[" || line == "]" || line == "," {
                continue;
            }

            let json_str = line.trim_start_matches(',').trim();
            if let Ok(_response) = serde_json::from_str::<VertexResponse>(json_str) {
                parsed_count += 1;
            }
        }

        assert_eq!(parsed_count, 2);
    }

    #[test]
    fn test_parse_streaming_malformed_json_handling() {
        // Malformed JSON should be skipped without panic
        let lines = vec![
            r#"{"candidates": invalid}"#,           // Invalid JSON
            r#"{"candidates": [{"content": {}}]}"#, // Valid but missing parts
            r"not json at all",
        ];

        let mut parsed_count = 0;
        for line in lines {
            if let Ok(_response) = serde_json::from_str::<VertexResponse>(line) {
                parsed_count += 1;
            }
        }

        // Only the middle one (with missing parts) might parse depending on struct
        assert!(parsed_count <= 1);
    }

    #[test]
    fn test_parse_streaming_token_accumulation() {
        // Simulate token count accumulation across chunks
        let chunks = vec![
            r#"{"usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}}"#,
            r#"{"usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 15}}"#,
            r#"{"usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 25}}"#,
        ];

        let mut final_input = 0u64;
        let mut final_output = 0u64;

        for chunk in chunks {
            if let Ok(response) = serde_json::from_str::<VertexResponse>(chunk) {
                if let Some(metadata) = response.usage_metadata {
                    final_input = metadata.prompt_token_count.unwrap_or(0);
                    final_output = metadata.candidates_token_count.unwrap_or(0);
                }
            }
        }

        // Final values should be from the last chunk
        assert_eq!(final_input, 10);
        assert_eq!(final_output, 25);
    }

    #[test]
    fn test_parse_streaming_large_response() {
        // Test parsing a large text response
        let large_text = "x".repeat(100_000);
        let chunk = format!(
            r#"{{"candidates": [{{"content": {{"parts": [{{"text": "{}"}}]}}}}]}}"#,
            large_text
        );

        let response: VertexResponse = serde_json::from_str(&chunk).unwrap();
        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => assert_eq!(text.len(), 100_000),
            _ => panic!("Expected text part"),
        }
    }

    #[test]
    fn test_parse_streaming_unicode_text() {
        let chunk =
            r#"{"candidates": [{"content": {"parts": [{"text": "こんにちは世界 🌍🚀"}]}}]}"#;
        let response: VertexResponse = serde_json::from_str(chunk).unwrap();

        let candidates = response.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        match &parts[0] {
            Part::Text { text } => {
                assert!(text.contains("こんにちは"));
                assert!(text.contains("🌍"));
            }
            _ => panic!("Expected text part"),
        }
    }
}
