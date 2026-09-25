//! AI types for messages, tools, and responses

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

/// Message role
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// Message content - can be text or structured
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Blocks(blocks) => {
                // Return first text block
                blocks.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            }
        }
    }
}

/// Native Gemini call context, retained only for provider history replay.
/// It is never part of executable tool arguments or tool authorization.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeminiToolContext {
    pub native_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl std::fmt::Debug for GeminiToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiToolContext")
            .field("native_name", &self.native_name)
            .field("has_thought_signature", &self.thought_signature.is_some())
            .finish_non_exhaustive()
    }
}

/// Content block types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gemini_context: Option<GeminiToolContext>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        /// Cryptographic signature for replaying thinking blocks to the API.
        /// This signature is required when sending a conversation with thinking
        /// blocks back to Claude - without it, the API will reject the request.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// Image source for vision
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// Tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "ToolSchemaEnforcement::is_off")]
    pub schema_enforcement: ToolSchemaEnforcement,
}

/// Provider-neutral constrained-sampling policy for a tool's JSON schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSchemaEnforcement {
    /// Preserve the provider's unconstrained tool behavior.
    #[default]
    Off,
    /// Use strict schema enforcement when the provider and schema support it.
    Prefer,
    /// Reject request preparation unless strict schema enforcement is available.
    Require,
}

impl ToolSchemaEnforcement {
    fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

impl Tool {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            schema_enforcement: ToolSchemaEnforcement::Off,
        }
    }

    #[must_use]
    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.input_schema = schema;
        self
    }

    #[must_use]
    pub fn with_schema_enforcement(mut self, enforcement: ToolSchemaEnforcement) -> Self {
        self.schema_enforcement = enforcement;
        self
    }
}

/// Machine-readable classification for provider stream failures whose
/// handling must not depend on parsing a display string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamErrorKind {
    /// A successful HTTP response ended before the provider's required
    /// terminal protocol event.
    TransientProtocol,
    /// The provider terminated because the configured output budget was
    /// exhausted.
    OutputTokenExhaustion,
    /// The provider reported another typed incomplete-response reason.
    IncompleteResponse,
    /// The provider emitted its explicit failed terminal event.
    ProviderDeclaredFailure,
}

/// Safe identifiers returned by the managed Gateway for one inference
/// request. This deliberately excludes the signed authorization and all
/// provider request or response payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedGatewayReceipt {
    pub request_id: String,
    pub record_id: String,
    pub lineage_id: String,
    pub record_status: String,
    /// Exact request system prompt; absent on unsuccessful HTTP responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_prompt_sha256: Option<String>,
    /// Gateway v1 digest of final tools and tool choice after adapter conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tools_sha256: Option<String>,
    /// Present only alongside supported tool evidence; zero means explicit empty tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_count: Option<u32>,
}

/// Content-free observations from the existing stream policy owner.
/// `Observed` establishes coverage; absence does not mean zero failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamObservation {
    Observed,
    OpenFailed,
    IdleTimeout,
    Disconnect,
    Retry,
    Recovery,
}

/// Optional local observer. Implementations must not block the stream task.
pub type StreamObserver = Arc<dyn Fn(StreamObservation) + Send + Sync>;

/// Streaming event from the AI
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Managed Gateway evidence available before any provider content.
    ManagedGatewayReceipt(ManagedGatewayReceipt),
    /// Message started
    MessageStart { id: String, model: String },
    /// Content block started
    ContentBlockStart { index: usize, block: ContentBlock },
    /// Text delta
    TextDelta { index: usize, text: String },
    /// Thinking delta
    ThinkingDelta { index: usize, thinking: String },
    /// Tool use input delta (JSON string chunk)
    InputJsonDelta { index: usize, partial_json: String },
    /// Thinking block signature (required for replaying thinking to API)
    ThinkingSignature { index: usize, signature: String },
    /// Content block completed
    ContentBlockStop {
        index: usize,
        /// Signature for thinking blocks (captured from `signature_delta`)
        thinking_signature: Option<String>,
    },
    /// Message completed
    MessageStop {
        /// Stop reason from the API (`MaxTokens` indicates overflow)
        stop_reason: Option<StopReason>,
    },
    /// Provider-reported USD cost; absent pricing is never inferred as zero.
    ProviderCost { cost_usd: f64 },
    /// Provider-reported reasoning tokens, a subset of output tokens.
    ReasoningUsage { tokens: u64 },
    /// Usage stats
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
    },
    /// Provider protocol failure with a stable machine-readable kind.
    ProviderError {
        kind: ProviderStreamErrorKind,
        message: String,
    },
    /// Error occurred
    Error { message: String },
}

/// Request configuration
#[derive(Debug, Clone)]
pub struct RequestConfig {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub system: Option<String>,
    pub tools: Arc<Vec<Tool>>,
    pub thinking: Option<ThinkingConfig>,
    /// Enable provider prompt caching for stable system, tool, and history prefixes.
    /// When true, supported providers add their native cache markers.
    pub cache_system_prompt: bool,
    /// Opt in to new explicit cache-write boundaries. Default off, so outgoing
    /// payloads stay identical to the previous request shape.
    pub explicit_cache_boundaries: bool,
    /// Immutable preparation proof; dispatch rejects changes after preparation.
    pub cache_topology: Option<crate::cache_topology::PreparedPrompt>,
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 8192,
            temperature: None,
            system: None,
            tools: Arc::new(Vec::new()),
            thinking: None,
            cache_system_prompt: false,
            explicit_cache_boundaries: false,
            cache_topology: None,
        }
    }
}

/// Extended thinking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: String,
    pub budget_tokens: u32,
}

impl ThinkingConfig {
    #[must_use]
    pub fn enabled(budget_tokens: u32) -> Self {
        Self {
            thinking_type: "enabled".to_string(),
            budget_tokens,
        }
    }
}

/// Stop reason from the API
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
}

/// Token usage statistics
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl Usage {
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ========================================================================
    // Role Tests
    // ========================================================================

    #[test]
    fn test_role_serialization() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
    }

    #[test]
    fn test_role_deserialization() {
        assert_eq!(
            serde_json::from_str::<Role>("\"user\"").unwrap(),
            Role::User
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"assistant\"").unwrap(),
            Role::Assistant
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"system\"").unwrap(),
            Role::System
        );
    }

    // ========================================================================
    // MessageContent Tests
    // ========================================================================

    #[test]
    fn test_message_content_text() {
        let content = MessageContent::text("Hello, world!");
        assert_eq!(content.as_text(), Some("Hello, world!"));
    }

    #[test]
    fn test_message_content_blocks_with_text() {
        let content = MessageContent::Blocks(vec![ContentBlock::Text {
            text: "Block text".to_string(),
        }]);
        assert_eq!(content.as_text(), Some("Block text"));
    }

    #[test]
    fn test_message_content_blocks_no_text() {
        let content = MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "tool-1".to_string(),
            name: "read".to_string(),
            input: json!({"path": "/tmp/test"}),
            gemini_context: None,
        }]);
        assert_eq!(content.as_text(), None);
    }

    #[test]
    fn test_message_content_blocks_finds_first_text() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read".to_string(),
                input: json!({}),
                gemini_context: None,
            },
            ContentBlock::Text {
                text: "First text".to_string(),
            },
            ContentBlock::Text {
                text: "Second text".to_string(),
            },
        ]);
        assert_eq!(content.as_text(), Some("First text"));
    }

    // ========================================================================
    // Tool Tests
    // ========================================================================

    #[test]
    fn test_tool_new() {
        let tool = Tool::new("read", "Read a file");
        assert_eq!(tool.name, "read");
        assert_eq!(tool.description, "Read a file");
        assert_eq!(tool.input_schema["type"], "object");
    }

    #[test]
    fn test_tool_with_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        });
        let tool = Tool::new("read", "Read a file").with_schema(schema.clone());
        assert_eq!(tool.input_schema, schema);
    }

    #[test]
    fn tool_schema_enforcement_is_explicit_and_defaults_off() {
        let default_tool = Tool::new("read", "Read a file");
        assert_eq!(default_tool.schema_enforcement, ToolSchemaEnforcement::Off);

        let preferred =
            Tool::new("read", "Read a file").with_schema_enforcement(ToolSchemaEnforcement::Prefer);
        assert_eq!(preferred.schema_enforcement, ToolSchemaEnforcement::Prefer);
    }

    #[test]
    fn tool_schema_enforcement_off_preserves_legacy_wire_shape() {
        let serialized = serde_json::to_value(Tool::new("read", "Read a file")).unwrap();
        assert!(serialized.get("schema_enforcement").is_none());

        let preferred = serde_json::to_value(
            Tool::new("read", "Read a file").with_schema_enforcement(ToolSchemaEnforcement::Prefer),
        )
        .unwrap();
        assert_eq!(preferred["schema_enforcement"], "prefer");
    }

    // ========================================================================
    // ThinkingConfig Tests
    // ========================================================================

    #[test]
    fn test_thinking_config_enabled() {
        let config = ThinkingConfig::enabled(4096);
        assert_eq!(config.thinking_type, "enabled");
        assert_eq!(config.budget_tokens, 4096);
    }

    // ========================================================================
    // StopReason Tests
    // ========================================================================

    #[test]
    fn test_stop_reason_serialization() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::MaxTokens).unwrap(),
            "\"max_tokens\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool_use\""
        );
    }

    // ========================================================================
    // Usage Tests
    // ========================================================================

    #[test]
    fn test_usage_total_tokens() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        assert_eq!(usage.total_tokens(), 150);
    }

    #[test]
    fn test_usage_default() {
        let usage = Usage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.total_tokens(), 0);
    }

    // ========================================================================
    // RequestConfig Tests
    // ========================================================================

    #[test]
    fn test_request_config_default() {
        let config = RequestConfig::default();
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.max_tokens, 8192);
        assert!(config.temperature.is_none());
        assert!(config.system.is_none());
        assert!(config.tools.is_empty());
        assert!(config.thinking.is_none());
        assert!(!config.cache_system_prompt);
    }

    // ========================================================================
    // ContentBlock Serialization Tests
    // ========================================================================

    #[test]
    fn test_content_block_text_serialization() {
        let block = ContentBlock::Text {
            text: "Hello".to_string(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello");
    }

    #[test]
    fn test_content_block_tool_use_serialization() {
        let block = ContentBlock::ToolUse {
            id: "tool-123".to_string(),
            name: "bash".to_string(),
            input: json!({"command": "ls"}),
            gemini_context: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "tool-123");
        assert_eq!(json["name"], "bash");
        assert_eq!(json["input"]["command"], "ls");
    }

    #[test]
    fn test_content_block_thinking_serialization() {
        let block = ContentBlock::Thinking {
            thinking: "Let me think...".to_string(),
            signature: Some("sig123".to_string()),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["thinking"], "Let me think...");
        assert_eq!(json["signature"], "sig123");
    }

    #[test]
    fn test_content_block_thinking_without_signature() {
        let block = ContentBlock::Thinking {
            thinking: "Let me think...".to_string(),
            signature: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "thinking");
        assert!(json.get("signature").is_none());
    }
}
