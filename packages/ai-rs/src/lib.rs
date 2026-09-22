//! AI Client Module
//!
//! This module provides a unified abstraction for communicating with multiple AI providers
//! (Anthropic's Claude API and OpenAI's GPT API). It handles streaming responses, SSE parsing,
//! and provides a common interface regardless of the underlying provider.
//!
//! # Architecture
//!
//! The module uses a trait-based design for polymorphism:
//!
//! - `AiClient` trait: Defines the common interface that all AI providers must implement
//! - `AnthropicClient`: Implementation for Anthropic's Claude API
//! - `OpenAiClient`: Implementation for OpenAI's GPT API (supports both Chat Completions and Responses APIs)
//! - `UnifiedClient`: Enum-based wrapper that can hold either provider
//!
//! # Rust Concepts Used
//!
//! ## Trait Objects and Dynamic Dispatch
//!
//! The `AiClient` trait allows us to write code that works with any AI provider:
//!
//! ```rust,ignore
//! pub trait AiClient: Send + Sync {
//!     async fn stream(&self, messages: &[Message], config: &RequestConfig)
//!         -> Result<mpsc::UnboundedReceiver<StreamEvent>>;
//! }
//! ```
//!
//! This uses dynamic dispatch when you use `dyn AiClient`, allowing runtime polymorphism.
//! However, we primarily use the `UnifiedClient` enum for zero-cost static dispatch.
//!
//! ## Async Streaming with Tokio
//!
//! All API communication is async using tokio:
//!
//! - `async fn`: Functions that return futures and can be awaited
//! - `mpsc::UnboundedReceiver`: Channel for streaming events from spawned tasks
//! - `tokio::spawn`: Spawn background tasks to handle SSE streams
//!
//! ## Serde for JSON Serialization
//!
//! The `serde` crate handles JSON serialization/deserialization:
//!
//! - `#[derive(Serialize, Deserialize)]`: Auto-generate JSON conversion code
//! - `#[serde(tag = "type")]`: Tagged enums for JSON variant discrimination
//! - `#[serde(rename_all = "snake_case")]`: Field name conversion
//!
//! ## Error Handling
//!
//! Uses `anyhow::Result` for ergonomic error propagation:
//!
//! - `?` operator: Propagate errors up the call stack
//! - `.context("message")`: Add context to errors for better debugging
//!
//! # Example Usage
//!
//! ```rust,ignore
//! use maestro_tui::ai::{create_client_for_model, Message, RequestConfig};
//!
//! // Create a client based on model name
//! let client = create_client_for_model("claude-opus-4-5")?;
//!
//! // Build request
//! let messages = vec![Message::user("Hello!")];
//! let config = RequestConfig::new("claude-opus-4-5", 4096);
//!
//! // Stream response
//! let mut rx = client.stream(&messages, &config).await?;
//! while let Some(event) = rx.recv().await {
//!     match event {
//!         StreamEvent::TextDelta { text, .. } => print!("{}", text),
//!         StreamEvent::MessageStop => break,
//!         _ => {}
//!     }
//! }
//! ```

mod anthropic;
pub mod app_message;
#[cfg(feature = "bedrock")]
mod bedrock;
pub mod cache_topology;
mod client;
pub(crate) mod constrained_sampling;
mod error;
mod google;
mod kimi;
pub mod managed_authorization;
mod model_capabilities;
pub use model_capabilities::{
    ASTRA_CONTEXT_TOKENS, ASTRA_OUTPUT_TOKENS, AnthropicRequestCapabilities, AnthropicThinkingMode,
    OpenAiRequestCapabilities, OpenAiWireProtocol, anthropic_request_capabilities,
    openai_request_capabilities, supports_explicit_prompt_caching,
};
pub mod op_secret;
#[path = "openai_wrapper.rs"]
mod openai;
#[path = "openai.rs"]
mod openai_base;
#[cfg(test)]
mod provider_matrix;
mod providers;
pub mod sanitize;
mod scripted;
mod transform;
mod types;
mod vertex;

pub use anthropic::AnthropicClient;
pub use app_message::{AppMessage, BashExecution, from_api_messages, transform_to_api_messages};
#[cfg(feature = "bedrock")]
pub use bedrock::BedrockClient;
pub use client::{
    AiClient, AiProvider, DEFAULT_STREAM_IDLE_TIMEOUT, DEFAULT_STREAM_MAX_RETRIES, UnifiedClient,
    canonical_managed_credential_name, canonical_managed_environment, create_client,
    create_client_for_model, provider_model_name,
};
pub use error::summarize_error_body;
pub use google::GoogleClient;
pub use openai::OpenAiClient;
pub use openai_base::is_context_overflow_message;
pub use providers::{ProviderDescriptor, ProviderProtocol, ProviderRegistry, ResolvedProvider};
pub use sanitize::{sanitize_control_chars, sanitize_for_api, sanitize_surrogates};
pub use scripted::{ScriptedBlock, ScriptedClient, ScriptedResponse};
pub use transform::{transform_messages, transform_messages_full};
pub use types::*;
pub use vertex::VertexAiClient;
