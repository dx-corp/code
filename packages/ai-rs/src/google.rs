//! Google Gemini AI Client
//!
//! Implementation of the Google Gemini API for streaming AI responses.
//! Supports Gemini 2.5 Pro/Flash models with function calling.
//!
//! # API Differences from Anthropic/OpenAI
//!
//! - Uses `generateContent` endpoint with streaming
//! - Different message format (Content with Parts)
//! - Tool calls are returned as `functionCall` parts
//! - Supports `thought` field for reasoning/thinking
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_tui::ai::google::GoogleClient;
//!
//! let client = GoogleClient::from_env()?;
//! let mut rx = client.stream(&messages, &config).await?;
//! while let Some(event) = rx.recv().await {
//!     // Handle streaming events
//! }
//! ```

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

/// Google Gemini API base URL
const GOOGLE_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Google Gemini client
#[derive(Clone)]
pub struct GoogleClient {
    api_key: String,
    client: Client,
}

impl GoogleClient {
    /// Create a new Google client with the given API key
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into().trim().to_string(),
            client: Client::builder()
                .timeout(std::time::Duration::from_mins(5))
                .build()
                .expect("Failed to create HTTP client"),
        }
    }

    /// Create a Google client from `GOOGLE_API_KEY` environment variable
    pub fn from_env() -> Result<Self> {
        let api_key = env::var("GOOGLE_API_KEY")
            .or_else(|_| env::var("GEMINI_API_KEY"))
            .context("GOOGLE_API_KEY or GEMINI_API_KEY environment variable not set")?;
        Ok(Self::new(api_key))
    }

    /// Stream a request to the Gemini API
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
        let api_key = self.api_key.clone();
        let model = provider_model_name(&config.model);

        tokio::spawn(async move {
            if let Err(e) =
                stream_google_response(client, api_key, model, request, tx.clone()).await
            {
                let _ = tx.send(StreamEvent::Error {
                    message: format!("Google API error: {e}"),
                });
            }
        });

        Ok(rx)
    }

    /// Build the Gemini API request body
    /// Serialized request body, used by the provider conformance matrix.
    #[cfg(test)]
    pub(crate) fn request_body_json(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.build_request(messages, config)?)?)
    }

    fn build_request(&self, messages: &[Message], config: &RequestConfig) -> Result<GoogleRequest> {
        let messages = crate::cache_topology::messages_with_volatile_tail(messages, config);
        let messages = super::transform::google_messages_for_wire(&messages);
        let contents = messages
            .iter()
            .map(|msg| self.message_to_content(msg))
            .collect();

        let mut request = GoogleRequest {
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

            request.tools = Some(vec![GoogleTool {
                function_declarations,
            }]);
        }

        Ok(request)
    }

    /// Convert our Message type to Google Content format
    fn message_to_content(&self, msg: &Message) -> Content {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
            Role::System => "user", // Google handles system via system_instruction
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
        AiProvider::Google
    }
}

/// Stream response from Google Gemini API
async fn stream_google_response(
    client: Client,
    api_key: String,
    model: String,
    request: GoogleRequest,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<()> {
    let url =
        format!("{GOOGLE_API_BASE}/models/{model}:streamGenerateContent?alt=sse&key={api_key}");

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .context("Failed to send request to Google API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Google API error ({status}): {}",
            super::summarize_error_body(&body)
        );
    }

    // Parse SSE stream
    let mut stream = response.bytes_stream();

    let mut buffer = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read_tokens = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read chunk")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete SSE events
        while let Some(pos) = buffer.find("\n\n") {
            let event = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            // Parse SSE data
            if let Some(data) = event.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    break;
                }

                if let Ok(response) = serde_json::from_str::<GoogleResponse>(data) {
                    // Process candidates
                    if let Some(candidates) = response.candidates {
                        for candidate in candidates {
                            if let Some(content) = candidate.content {
                                for part in content.parts {
                                    match part {
                                        Part::Text { text } => {
                                            let _ =
                                                tx.send(StreamEvent::TextDelta { index: 0, text });
                                        }
                                        Part::FunctionCall { function_call } => {
                                            // Tool use - send as content block start with ToolUse
                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                index: 0,
                                                block: ContentBlock::ToolUse {
                                                    id: format!("call_{}", uuid::Uuid::new_v4()),
                                                    name: function_call.name,
                                                    input: function_call.args,
                                                    gemini_context: None,
                                                },
                                            });
                                        }
                                        _ => {}
                                    }
                                }
                            }

                            // Check finish reason
                            if let Some(reason) = candidate.finish_reason {
                                let stop_reason = match reason.as_str() {
                                    "STOP" => Some(StopReason::EndTurn),
                                    "MAX_TOKENS" => Some(StopReason::MaxTokens),
                                    "SAFETY" => Some(StopReason::EndTurn),
                                    _ => Some(StopReason::EndTurn),
                                };
                                let _ = tx.send(StreamEvent::MessageStop { stop_reason });
                            }
                        }
                    }

                    // Update usage
                    if let Some(metadata) = response.usage_metadata {
                        input_tokens = metadata.prompt_token_count.unwrap_or(input_tokens);
                        output_tokens = metadata.candidates_token_count.unwrap_or(output_tokens);
                        cache_read_tokens =
                            metadata.cached_content_token_count.or(cache_read_tokens);
                    }
                }
            }
        }
    }

    // Send final usage
    let _ = tx.send(super::google::cache_usage_event(
        input_tokens,
        output_tokens,
        cache_read_tokens,
    ));

    Ok(())
}

/// Google prompt counts include cached tokens. Keep cache absence distinct from zero.
pub(super) fn cache_usage_event(input: u64, output: u64, cached: Option<u64>) -> StreamEvent {
    StreamEvent::Usage {
        input_tokens: input.saturating_sub(cached.unwrap_or(0)),
        output_tokens: output,
        cache_read_tokens: cached,
        cache_creation_tokens: None,
    }
}

// ============================================================================
// Google API Types
// ============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleRequest {
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GoogleTool>>,
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
struct GoogleTool {
    function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct FunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleResponse {
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
    fn provider_cache_usage_distinguishes_hit_miss_and_unknown() {
        for (field, expected) in [(None, None), (Some(0), Some(0)), (Some(80), Some(80))] {
            let mut metadata = serde_json::json!({"promptTokenCount":100,"candidatesTokenCount":5});
            if let Some(value) = field {
                metadata["cachedContentTokenCount"] = value.into();
            }
            let metadata: UsageMetadata = serde_json::from_value(metadata).unwrap();
            let StreamEvent::Usage {
                input_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                ..
            } = cache_usage_event(
                metadata.prompt_token_count.unwrap(),
                5,
                metadata.cached_content_token_count,
            )
            else {
                panic!("expected usage")
            };
            assert_eq!(cache_read_tokens, expected);
            assert_eq!(input_tokens, 100 - expected.unwrap_or(0));
            assert_eq!(cache_creation_tokens, None);
        }
    }

    #[test]
    fn volatile_tail_reaches_the_provider_after_history() {
        let client = GoogleClient::new("test");
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
        let client = GoogleClient::new("test");
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

    // ========================================================================
    // Client Creation Tests
    // ========================================================================

    #[test]
    fn test_google_client_creation() {
        let client = GoogleClient::new("test-key");
        assert_eq!(client.provider(), AiProvider::Google);
    }

    #[test]
    fn test_google_client_with_empty_key() {
        let client = GoogleClient::new("");
        assert_eq!(client.provider(), AiProvider::Google);
        // Empty key is allowed at construction, will fail at API call
    }

    // ========================================================================
    // Message Conversion Tests
    // ========================================================================

    #[test]
    fn test_message_to_content_text() {
        let client = GoogleClient::new("test");
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
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Text("Hi there".to_string()),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.role, Some("model".to_string()));
    }

    #[test]
    fn test_message_to_content_system() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::System,
            content: MessageContent::Text("You are helpful".to_string()),
        };

        let content = client.message_to_content(&msg);
        // System messages are converted to user role in Google format
        assert_eq!(content.role, Some("user".to_string()));
    }

    #[test]
    fn test_message_to_content_with_blocks() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Hello".to_string(),
                },
                ContentBlock::Thinking {
                    thinking: "Let me think...".to_string(),
                    signature: None,
                },
            ]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 2);

        // First part should be plain text
        match &content.parts[0] {
            Part::Text { text } => assert_eq!(text, "Hello"),
            _ => panic!("Expected Text part"),
        }

        // Second part should be thinking wrapped in tags
        match &content.parts[1] {
            Part::Text { text } => assert!(text.contains("<thinking>")),
            _ => panic!("Expected Text part for thinking"),
        }
    }

    #[test]
    fn test_message_to_content_with_tool_use() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call_123".to_string(),
                name: "get_weather".to_string(),
                input: json!({"city": "Seattle"}),
                gemini_context: None,
            }]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);
        match &content.parts[0] {
            Part::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "get_weather");
                assert_eq!(function_call.args["city"], "Seattle");
            }
            _ => panic!("Expected FunctionCall part"),
        }
    }

    #[test]
    fn test_message_to_content_with_tool_result() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_123".to_string(),
                content: "72°F and sunny".to_string(),
                is_error: Some(false),
            }]),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);
        match &content.parts[0] {
            Part::FunctionResponse { function_response } => {
                assert_eq!(function_response.name, "call_123");
            }
            _ => panic!("Expected FunctionResponse part"),
        }
    }

    // ========================================================================
    // Request Building Tests
    // ========================================================================

    #[test]
    fn test_build_request_with_tools() {
        let client = GoogleClient::new("test");
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
        let sys = request.system_instruction.unwrap();
        assert_eq!(sys.parts.len(), 1);
    }

    #[test]
    fn test_build_request_without_tools() {
        let client = GoogleClient::new("test");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert!(request.tools.is_none());
        assert!(request.system_instruction.is_none());
    }

    #[test]
    fn test_build_request_with_multiple_messages() {
        let client = GoogleClient::new("test");
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("Hi!".to_string()),
            },
            Message {
                role: Role::User,
                content: MessageContent::Text("How are you?".to_string()),
            },
        ];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            temperature: Some(0.5),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        assert_eq!(request.contents.len(), 3);
        assert_eq!(request.contents[0].role, Some("user".to_string()));
        assert_eq!(request.contents[1].role, Some("model".to_string()));
        assert_eq!(request.contents[2].role, Some("user".to_string()));
    }

    #[test]
    fn test_build_request_generation_config() {
        let client = GoogleClient::new("test");
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
    fn test_build_request_with_multiple_tools() {
        let client = GoogleClient::new("test");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            tools: vec![
                Tool::new("tool1", "First tool").with_schema(json!({"type": "object"})),
                Tool::new("tool2", "Second tool").with_schema(json!({"type": "object"})),
                Tool::new("tool3", "Third tool").with_schema(json!({"type": "object"})),
            ]
            .into(),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        let tools = request.tools.unwrap();
        assert_eq!(tools[0].function_declarations.len(), 3);
        assert_eq!(tools[0].function_declarations[0].name, "tool1");
        assert_eq!(tools[0].function_declarations[1].name, "tool2");
        assert_eq!(tools[0].function_declarations[2].name, "tool3");
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_message_list() {
        let client = GoogleClient::new("test");
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
    fn test_message_with_empty_text() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text(String::new()),
        };

        let content = client.message_to_content(&msg);
        assert_eq!(content.parts.len(), 1);
        match &content.parts[0] {
            Part::Text { text } => assert!(text.is_empty()),
            _ => panic!("Expected Text part"),
        }
    }

    #[test]
    fn test_message_with_unicode() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text("Hello 你好 مرحبا 🌍".to_string()),
        };

        let content = client.message_to_content(&msg);
        match &content.parts[0] {
            Part::Text { text } => assert!(text.contains("你好")),
            _ => panic!("Expected Text part"),
        }
    }

    #[test]
    fn test_message_with_empty_blocks() {
        let client = GoogleClient::new("test");
        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![]),
        };

        let content = client.message_to_content(&msg);
        assert!(content.parts.is_empty());
    }

    #[test]
    fn test_tool_with_complex_schema() {
        let client = GoogleClient::new("test");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Test".to_string()),
        }];

        let complex_schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "User name"},
                "age": {"type": "integer", "minimum": 0},
                "tags": {"type": "array", "items": {"type": "string"}},
                "metadata": {
                    "type": "object",
                    "properties": {
                        "created": {"type": "string", "format": "date-time"}
                    }
                }
            },
            "required": ["name"]
        });

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 1024,
            tools: vec![
                Tool::new("complex_tool", "A complex tool").with_schema(complex_schema.clone()),
            ]
            .into(),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        let tools = request.tools.unwrap();
        assert_eq!(tools[0].function_declarations[0].parameters, complex_schema);
    }

    // ========================================================================
    // Serialization Tests
    // ========================================================================

    #[test]
    fn test_request_serialization() {
        let client = GoogleClient::new("test");
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hello".to_string()),
        }];

        let config = RequestConfig {
            model: "gemini-2.0-flash".to_string(),
            max_tokens: 100,
            temperature: Some(0.5),
            system: Some("Be helpful".to_string()),
            ..Default::default()
        };

        let request = client.build_request(&messages, &config).unwrap();
        let json_str = serde_json::to_string(&request).unwrap();

        // Verify it can be serialized to JSON
        assert!(json_str.contains("contents"));
        assert!(json_str.contains("generationConfig"));
        assert!(json_str.contains("systemInstruction"));
    }

    #[test]
    fn test_part_serialization() {
        // Test Text part
        let text_part = Part::Text {
            text: "Hello".to_string(),
        };
        let json = serde_json::to_string(&text_part).unwrap();
        assert!(json.contains("Hello"));

        // Test FunctionCall part
        let fc_part = Part::FunctionCall {
            function_call: FunctionCall {
                name: "test".to_string(),
                args: json!({"key": "value"}),
            },
        };
        let json = serde_json::to_string(&fc_part).unwrap();
        assert!(json.contains("functionCall"));
        assert!(json.contains("test"));
    }
}
