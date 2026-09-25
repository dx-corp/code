//! Amazon Bedrock Converse streaming client.
//!
//! The AWS SDK owns SigV4 signing and the default credential provider chain;
//! this module only translates Maestro's provider-neutral request/events to
//! Bedrock's Converse API.  Credentials are never synthesized: static
//! environment credentials are passed through when complete, while profile,
//! SSO, web-identity, and container sources are resolved by the SDK at request
//! time.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use aws_credential_types::{Credentials, provider::ProvideCredentials};
use aws_sdk_bedrockruntime as bedrock;
use aws_smithy_types::{Blob, Document, Number};
use base64::Engine;
use serde_json::Value;
use tokio::sync::mpsc;

use super::client::{AiClient, AiProvider, provider_model_name};
use super::types::{ContentBlock, ImageSource, Message, MessageContent, RequestConfig, Role};
use super::types::{StopReason, StreamEvent};

const DEFAULT_REGION: &str = "us-east-1";
const AWS_ACCESS_KEY_ID: &str = "AWS_ACCESS_KEY_ID";
const AWS_SECRET_ACCESS_KEY: &str = "AWS_SECRET_ACCESS_KEY";

/// Configuration error returned when no supported AWS credential source is
/// configured for Bedrock.
const CREDENTIALS_ERROR: &str = "Bedrock requires AWS credentials. Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY together, or configure AWS_PROFILE, AWS_CONFIG_FILE/AWS_SHARED_CREDENTIALS_FILE, AWS_WEB_IDENTITY_TOKEN_FILE, or AWS_CONTAINER_CREDENTIALS_RELATIVE_URI/AWS_CONTAINER_CREDENTIALS_FULL_URI.";

#[derive(Clone)]
struct StaticCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

/// Native Bedrock Converse client.
#[derive(Clone)]
pub struct BedrockClient {
    region: String,
    use_sdk_region: bool,
    endpoint_url: Option<String>,
    static_credentials: Option<StaticCredentials>,
}

impl BedrockClient {
    /// Build a client from the process environment.
    pub fn from_env() -> Result<Self> {
        let env = std::env::vars().collect::<HashMap<_, _>>();
        let endpoint = env.get("AWS_BEDROCK_ENDPOINT").map(String::as_str);
        Self::from_runtime_env(&env, endpoint)
    }

    /// Build a client from the resolved provider environment.
    pub fn from_runtime_env(
        env: &HashMap<String, String>,
        endpoint_url: Option<&str>,
    ) -> Result<Self> {
        let access_key_id = resolved_env_value(env, AWS_ACCESS_KEY_ID)?;
        let secret_access_key = resolved_env_value(env, AWS_SECRET_ACCESS_KEY)?;
        if access_key_id.is_some() != secret_access_key.is_some() {
            bail!(
                "Bedrock AWS credentials are incomplete: set {AWS_ACCESS_KEY_ID} and {AWS_SECRET_ACCESS_KEY} together"
            );
        }

        let static_credentials = match access_key_id.zip(secret_access_key) {
            Some((access, secret)) => Some(StaticCredentials {
                access_key_id: access,
                secret_access_key: secret,
                session_token: resolved_env_value(env, "AWS_SESSION_TOKEN")?,
            }),
            None => None,
        };

        let configured_region = first_non_empty(env, &["AWS_REGION", "AWS_DEFAULT_REGION"]);
        let use_sdk_region = configured_region.is_none()
            && (has_deferred_credential_source(env) || has_profile_file_source(env));
        if static_credentials.is_none()
            && !has_deferred_credential_source(env)
            && !has_profile_file_source(env)
        {
            bail!("{CREDENTIALS_ERROR}");
        }

        let region = configured_region.unwrap_or_else(|| DEFAULT_REGION.to_string());
        let endpoint_url = endpoint_url
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        Ok(Self {
            region,
            use_sdk_region,
            endpoint_url,
            static_credentials,
        })
    }

    /// AWS region used for endpoint routing and SigV4 signing.
    #[must_use]
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Optional endpoint override (primarily useful for local/mock runtimes).
    #[must_use]
    pub fn endpoint_url(&self) -> Option<&str> {
        self.endpoint_url.as_deref()
    }

    async fn sdk_client(&self) -> Result<bedrock::Client> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if !self.use_sdk_region {
            loader = loader.region(aws_types_region(&self.region));
        }
        if let Some(endpoint_url) = &self.endpoint_url {
            loader = loader.endpoint_url(endpoint_url.clone());
        }
        if let Some(credentials) = &self.static_credentials {
            loader = loader.credentials_provider(Credentials::new(
                credentials.access_key_id.clone(),
                credentials.secret_access_key.clone(),
                credentials.session_token.clone(),
                None,
                "maestro-bedrock-runtime",
            ));
        }
        let config = loader.load().await;
        let provider = config
            .credentials_provider()
            .context("Bedrock AWS credential provider is unavailable")?;
        provider.provide_credentials().await.map_err(|error| {
            anyhow::anyhow!("{CREDENTIALS_ERROR} AWS resolution failed: {error}")
        })?;
        Ok(bedrock::Client::new(&config))
    }

    async fn stream_impl(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let cache_enabled = config.cache_system_prompt
            && crate::supports_explicit_prompt_caching(AiProvider::Bedrock, &config.model);
        let request_messages = build_request_messages(messages, config)?;
        let model = provider_model_name(&config.model);
        let client = self.sdk_client().await?;
        let mut request = client
            .converse_stream()
            .model_id(model.clone())
            .set_messages((!request_messages.is_empty()).then_some(request_messages));

        if let Some(system) = &config.system {
            request = request.set_system(Some(build_system_content(system, cache_enabled)?));
        }

        let mut inference =
            bedrock::types::InferenceConfiguration::builder().max_tokens(config.max_tokens as i32);
        if let Some(temperature) = config.temperature {
            inference = inference.temperature(temperature);
        }
        request = request.inference_config(inference.build());

        if !config.tools.is_empty() {
            request = request.tool_config(build_tool_configuration(&config.tools, cache_enabled)?);
        }

        let response = request
            .send()
            .await
            .context("Bedrock ConverseStream request failed")?;
        let mut event_stream = response.stream;
        let (tx, rx) = mpsc::unbounded_channel();
        let model = config.model.clone();
        tokio::spawn(async move {
            let message_id = format!("bedrock-{}", uuid::Uuid::new_v4());
            if tx
                .send(StreamEvent::MessageStart {
                    id: message_id,
                    model,
                })
                .is_err()
            {
                return;
            }
            let mut thinking_signatures = HashMap::new();
            loop {
                match event_stream.recv().await {
                    Ok(Some(event)) => {
                        if let Err(error) = send_event(&tx, event, &mut thinking_signatures) {
                            let _ = tx.send(StreamEvent::Error {
                                message: format!(
                                    "Bedrock ConverseStream event mapping failed: {error}"
                                ),
                            });
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = tx.send(StreamEvent::Error {
                            message: format!("Bedrock ConverseStream failed: {error}"),
                        });
                        return;
                    }
                }
            }
        });

        Ok(rx)
    }
}

impl AiClient for BedrockClient {
    fn provider(&self) -> AiProvider {
        AiProvider::Bedrock
    }

    async fn stream(
        &self,
        messages: &[Message],
        config: &RequestConfig,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        self.stream_impl(messages, config).await
    }
}

fn non_empty(env: &HashMap<String, String>, name: &str) -> Option<String> {
    env.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn resolved_env_value(env: &HashMap<String, String>, name: &str) -> Result<Option<String>> {
    env.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| super::op_secret::resolve_credential(name, value))
        .transpose()
}

fn first_non_empty(env: &HashMap<String, String>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| non_empty(env, name))
}

fn has_deferred_credential_source(env: &HashMap<String, String>) -> bool {
    [
        "AWS_PROFILE",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ]
    .iter()
    .any(|name| non_empty(env, name).is_some())
}

fn has_profile_file_source(env: &HashMap<String, String>) -> bool {
    ["AWS_SHARED_CREDENTIALS_FILE", "AWS_CONFIG_FILE"]
        .iter()
        .filter_map(|name| non_empty(env, name))
        .any(|path| std::path::Path::new(&path).is_file())
        || ["HOME", "USERPROFILE"]
            .iter()
            .filter_map(|name| non_empty(env, name))
            .map(|home| std::path::Path::new(&home).join(".aws"))
            .any(|aws_dir| {
                aws_dir.join("credentials").is_file() || aws_dir.join("config").is_file()
            })
}

fn aws_types_region(region: &str) -> aws_types::region::Region {
    aws_types::region::Region::new(region.to_string())
}

fn cache_point_block() -> Result<bedrock::types::CachePointBlock> {
    bedrock::types::CachePointBlock::builder()
        .r#type(bedrock::types::CachePointType::Default)
        .build()
        .context("invalid Bedrock cache point")
}

fn build_system_content(
    system: &str,
    cache_enabled: bool,
) -> Result<Vec<bedrock::types::SystemContentBlock>> {
    let mut blocks = vec![bedrock::types::SystemContentBlock::Text(system.to_owned())];
    if cache_enabled {
        blocks.push(bedrock::types::SystemContentBlock::CachePoint(
            cache_point_block()?,
        ));
    }
    Ok(blocks)
}

fn build_tool_configuration(
    tools: &[crate::Tool],
    cache_enabled: bool,
) -> Result<bedrock::types::ToolConfiguration> {
    let mut tool_config = bedrock::types::ToolConfiguration::builder();
    for tool in tools {
        let input_schema =
            bedrock::types::ToolInputSchema::Json(document_from_json(&tool.input_schema));
        let specification = bedrock::types::ToolSpecification::builder()
            .name(tool.name.clone())
            .description(tool.description.clone())
            .input_schema(input_schema)
            .build()
            .context("invalid Bedrock tool specification")?;
        tool_config = tool_config.tools(bedrock::types::Tool::ToolSpec(specification));
    }
    if cache_enabled {
        tool_config = tool_config.tools(bedrock::types::Tool::CachePoint(cache_point_block()?));
    }
    tool_config
        .build()
        .context("invalid Bedrock tool configuration")
}

fn build_request_messages(
    messages: &[Message],
    config: &RequestConfig,
) -> Result<Vec<bedrock::types::Message>> {
    let mut built = build_messages(messages)?;
    if config.cache_system_prompt
        && crate::supports_explicit_prompt_caching(AiProvider::Bedrock, &config.model)
        && config.cache_topology.is_some()
    {
        let newest = built.len().checked_sub(1);
        let previous = config
            .cache_topology
            .as_ref()
            .and_then(crate::cache_topology::PreparedPrompt::previous_checkpoint)
            .filter(|index| *index < messages.len())
            .and_then(|index| {
                messages[..=index]
                    .iter()
                    .filter(|message| message.role != Role::System)
                    .count()
                    .checked_sub(1)
            })
            .filter(|index| Some(*index) < newest);
        for index in previous.into_iter().chain(newest) {
            built[index]
                .content
                .push(bedrock::types::ContentBlock::CachePoint(
                    cache_point_block()?
                ));
        }
    }
    if let Some(tail) = config
        .cache_topology
        .as_ref()
        .and_then(crate::cache_topology::PreparedPrompt::volatile_tail)
    {
        built.push(build_message(
            &Message {
                role: Role::User,
                content: MessageContent::text(tail),
            },
            bedrock::types::ConversationRole::User,
        )?);
    }
    Ok(built)
}

fn build_messages(messages: &[Message]) -> Result<Vec<bedrock::types::Message>> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => bedrock::types::ConversationRole::User,
                Role::Assistant => bedrock::types::ConversationRole::Assistant,
                // System content is sent through RequestConfig.system. Ignore
                // system entries here rather than emitting an invalid role.
                Role::System => return None,
            };
            Some(build_message(message, role))
        })
        .collect()
}

fn build_message(
    message: &Message,
    role: bedrock::types::ConversationRole,
) -> Result<bedrock::types::Message> {
    let blocks = match &message.content {
        MessageContent::Text(text) => vec![bedrock::types::ContentBlock::Text(text.clone())],
        MessageContent::Blocks(blocks) => {
            let mut converted = Vec::with_capacity(blocks.len());
            for block in blocks {
                if let Some(result) = build_content_block(block) {
                    converted.push(result?);
                }
            }
            converted
        }
    };
    bedrock::types::Message::builder()
        .role(role)
        .set_content(Some(blocks))
        .build()
        .context("invalid Bedrock message")
}

fn build_content_block(block: &ContentBlock) -> Option<Result<bedrock::types::ContentBlock>> {
    Some(match block {
        ContentBlock::Text { text } => Ok(bedrock::types::ContentBlock::Text(text.clone())),
        ContentBlock::Image { source } => build_image_block(source),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => bedrock::types::ToolUseBlock::builder()
            .tool_use_id(id.clone())
            .name(name.clone())
            .input(document_from_json(input))
            .build()
            .map(bedrock::types::ContentBlock::ToolUse)
            .context("invalid Bedrock tool-use block"),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => bedrock::types::ToolResultBlock::builder()
            .tool_use_id(tool_use_id.clone())
            .content(bedrock::types::ToolResultContentBlock::Text(
                content.clone(),
            ))
            .status(if is_error.unwrap_or(false) {
                bedrock::types::ToolResultStatus::Error
            } else {
                bedrock::types::ToolResultStatus::Success
            })
            .build()
            .map(bedrock::types::ContentBlock::ToolResult)
            .context("invalid Bedrock tool-result block"),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            let mut reasoning =
                bedrock::types::ReasoningTextBlock::builder().text(thinking.clone());
            if let Some(signature) = signature {
                reasoning = reasoning.signature(signature.clone());
            }
            reasoning
                .build()
                .map(|block| {
                    bedrock::types::ContentBlock::ReasoningContent(
                        bedrock::types::ReasoningContentBlock::ReasoningText(block),
                    )
                })
                .context("invalid Bedrock reasoning block")
        }
    })
}

fn build_image_block(source: &ImageSource) -> Result<bedrock::types::ContentBlock> {
    let ImageSource::Base64 { media_type, data } = source else {
        bail!("Bedrock Converse requires base64 image sources");
    };
    let format = media_type
        .strip_prefix("image/")
        .unwrap_or(media_type)
        .to_ascii_lowercase();
    let format = match format.as_str() {
        "png" => bedrock::types::ImageFormat::Png,
        "jpeg" | "jpg" => bedrock::types::ImageFormat::Jpeg,
        "gif" => bedrock::types::ImageFormat::Gif,
        "webp" => bedrock::types::ImageFormat::Webp,
        _ => bail!("Bedrock does not support image format `{media_type}`"),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("invalid base64 image for Bedrock")?;
    bedrock::types::ImageBlock::builder()
        .format(format)
        .source(bedrock::types::ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .map(bedrock::types::ContentBlock::Image)
        .context("invalid Bedrock image block")
}

fn send_event(
    tx: &mpsc::UnboundedSender<StreamEvent>,
    event: bedrock::types::ConverseStreamOutput,
    thinking_signatures: &mut HashMap<usize, String>,
) -> Result<()> {
    use bedrock::types::{ContentBlockDelta, ContentBlockStart, ConverseStreamOutput};

    match event {
        ConverseStreamOutput::ContentBlockStart(event) => {
            let index = event.content_block_index.max(0) as usize;
            let block = match event.start {
                Some(ContentBlockStart::ToolUse(tool)) => ContentBlock::ToolUse {
                    id: tool.tool_use_id().to_string(),
                    name: tool.name().to_string(),
                    input: Value::Object(serde_json::Map::new()),
                    gemini_context: None,
                },
                Some(ContentBlockStart::Image(_)) => {
                    bail!("unsupported Bedrock image content-block start at index {index}")
                }
                Some(ContentBlockStart::ToolResult(_)) => {
                    bail!("unsupported Bedrock tool-result content-block start at index {index}")
                }
                // Text blocks have no explicit `start` payload in the Bedrock
                // event stream; the first text delta carries their content.
                None => ContentBlock::Text {
                    text: String::new(),
                },
                _ => bail!("unsupported Bedrock content-block start at index {index}"),
            };
            tx.send(StreamEvent::ContentBlockStart { index, block })?;
        }
        ConverseStreamOutput::ContentBlockDelta(event) => {
            let index = event.content_block_index.max(0) as usize;
            if let Some(delta) = event.delta {
                match delta {
                    ContentBlockDelta::Text(text) => {
                        tx.send(StreamEvent::TextDelta { index, text })?;
                    }
                    ContentBlockDelta::ToolUse(tool) => {
                        tx.send(StreamEvent::InputJsonDelta {
                            index,
                            partial_json: tool.input().to_string(),
                        })?;
                    }
                    ContentBlockDelta::ReasoningContent(reasoning) => match reasoning {
                        bedrock::types::ReasoningContentBlockDelta::Text(text) => {
                            tx.send(StreamEvent::ThinkingDelta {
                                index,
                                thinking: text,
                            })?;
                        }
                        bedrock::types::ReasoningContentBlockDelta::Signature(signature) => {
                            thinking_signatures.insert(index, signature.clone());
                            tx.send(StreamEvent::ThinkingSignature { index, signature })?;
                        }
                        bedrock::types::ReasoningContentBlockDelta::RedactedContent(_) => {
                            bail!("unsupported Bedrock redacted reasoning content at index {index}")
                        }
                        _ => {
                            bail!("unsupported Bedrock reasoning content at index {index}")
                        }
                    },
                    ContentBlockDelta::Image(_) => {
                        bail!("unsupported Bedrock image content-block delta at index {index}")
                    }
                    ContentBlockDelta::ToolResult(_) => {
                        bail!(
                            "unsupported Bedrock tool-result content-block delta at index {index}"
                        )
                    }
                    ContentBlockDelta::Citation(_) => {
                        bail!("unsupported Bedrock citation content-block delta at index {index}")
                    }
                    _ => bail!("unsupported Bedrock content-block delta at index {index}"),
                }
            }
        }
        ConverseStreamOutput::ContentBlockStop(event) => {
            let index = event.content_block_index.max(0) as usize;
            tx.send(StreamEvent::ContentBlockStop {
                index,
                thinking_signature: thinking_signatures.remove(&index),
            })?;
        }
        ConverseStreamOutput::MessageStop(event) => {
            tx.send(StreamEvent::MessageStop {
                stop_reason: map_stop_reason(event.stop_reason),
            })?;
        }
        ConverseStreamOutput::Metadata(event) => {
            if let Some(usage) = event.usage {
                tx.send(StreamEvent::Usage {
                    input_tokens: usage.input_tokens.max(0) as u64,
                    output_tokens: usage.output_tokens.max(0) as u64,
                    cache_read_tokens: usage
                        .cache_read_input_tokens
                        .map(|value| value.max(0) as u64),
                    cache_creation_tokens: usage
                        .cache_write_input_tokens
                        .map(|value| value.max(0) as u64),
                })?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn map_stop_reason(reason: bedrock::types::StopReason) -> Option<StopReason> {
    match reason.as_str() {
        "max_tokens" | "model_context_window_exceeded" => Some(StopReason::MaxTokens),
        "tool_use" | "malformed_tool_use" => Some(StopReason::ToolUse),
        "stop_sequence" => Some(StopReason::StopSequence),
        "end_turn" | "content_filtered" | "guardrail_intervened" | "malformed_model_output" => {
            Some(StopReason::EndTurn)
        }
        _ => None,
    }
}

fn document_from_json(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(values.iter().map(document_from_json).collect()),
        Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), document_from_json(value)))
                .collect(),
        ),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                Document::Number(Number::PosInt(value))
            } else if let Some(value) = number.as_i64() {
                Document::Number(Number::NegInt(value))
            } else {
                Document::Number(Number::Float(number.as_f64().unwrap_or_default()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_env() -> HashMap<String, String> {
        HashMap::from([
            (AWS_ACCESS_KEY_ID.to_string(), "access".to_string()),
            (AWS_SECRET_ACCESS_KEY.to_string(), "secret".to_string()),
            ("AWS_REGION".to_string(), "eu-west-1".to_string()),
        ])
    }

    #[test]
    fn static_runtime_configuration_is_preserved() {
        let client =
            BedrockClient::from_runtime_env(&static_env(), Some("http://localhost:4566")).unwrap();
        assert_eq!(client.region(), "eu-west-1");
        assert_eq!(client.endpoint_url(), Some("http://localhost:4566"));
    }

    #[test]
    fn deferred_runtime_configuration_accepts_profile_chain() {
        let env = HashMap::from([("AWS_PROFILE".to_string(), "default".to_string())]);
        let client = BedrockClient::from_runtime_env(&env, None).unwrap();
        assert_eq!(client.region(), DEFAULT_REGION);
    }

    #[test]
    fn standalone_sso_session_marker_does_not_claim_a_credential_source() {
        let env = HashMap::from([(
            "AWS_SSO_SESSION_NAME".to_string(),
            "default-sso".to_string(),
        )]);
        let error = match BedrockClient::from_runtime_env(&env, None) {
            Ok(_) => panic!("AWS_SSO_SESSION_NAME alone is not an aws-config source"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("AWS_PROFILE"));
        assert!(!error.contains("AWS_SSO_SESSION_NAME"));
    }

    #[test]
    fn profile_file_configuration_accepts_default_chain() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let env = HashMap::from([(
            "AWS_SHARED_CREDENTIALS_FILE".to_string(),
            file.path().display().to_string(),
        )]);
        let client = BedrockClient::from_runtime_env(&env, None).unwrap();
        assert_eq!(client.region(), DEFAULT_REGION);
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::text(text),
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::text(text),
        }
    }

    fn cache_point_positions(messages: &[bedrock::types::Message]) -> Vec<usize> {
        messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message
                    .content()
                    .iter()
                    .any(bedrock::types::ContentBlock::is_cache_point)
            })
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn cached_request_walks_history_before_volatile_tail() {
        let base = RequestConfig {
            model: "bedrock/anthropic.claude-sonnet-4-5-20250929-v1:0".into(),
            cache_system_prompt: true,
            ..RequestConfig::default()
        };
        let first_history = vec![user("a"), assistant("b")];
        let first = crate::cache_topology::PreparedPrompt::prepare(
            &first_history,
            &base,
            "session".into(),
            None,
        )
        .unwrap();
        let history = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let prepared = crate::cache_topology::PreparedPrompt::prepare(
            &history,
            &base,
            "session".into(),
            Some(first.topology()),
        )
        .unwrap()
        .with_volatile_tail(Some("clock: now".into()));
        let config = RequestConfig {
            cache_topology: Some(prepared),
            ..base
        };

        let messages = build_request_messages(&history, &config).unwrap();

        assert_eq!(cache_point_positions(&messages), vec![1, 3]);
        let mut sealed = config.cache_topology.clone().unwrap();
        sealed
            .finalize_boundary(
                Some("bedrock"),
                &config.model,
                true,
                false,
                false,
                history.len(),
            )
            .unwrap();
        let off = RequestConfig {
            cache_topology: Some(sealed),
            explicit_cache_boundaries: false,
            ..config.clone()
        };
        let off_messages = build_request_messages(&history, &off).unwrap();
        assert_eq!(
            format!("{off_messages:?}"),
            format!("{messages:?}"),
            "explicit boundaries stay off, so the Bedrock payload matches the legacy payload"
        );
        assert_eq!(messages.len(), 5);
        assert_eq!(
            messages[4].content(),
            &[bedrock::types::ContentBlock::Text("clock: now".into())]
        );
    }

    #[test]
    fn cached_request_marks_system_and_tool_prefixes() {
        let system = build_system_content("stable instructions", true).unwrap();
        assert!(matches!(
            system.as_slice(),
            [
                bedrock::types::SystemContentBlock::Text(text),
                bedrock::types::SystemContentBlock::CachePoint(_)
            ] if text == "stable instructions"
        ));

        let tools = vec![
            crate::Tool::new("read", "Read a file"),
            crate::Tool::new("grep", "Search files"),
        ];
        let tool_config = build_tool_configuration(&tools, true).unwrap();
        assert_eq!(tool_config.tools().len(), 3);
        assert!(matches!(
            tool_config.tools().last(),
            Some(bedrock::types::Tool::CachePoint(_))
        ));
    }

    #[test]
    fn uncached_request_emits_no_cache_points() {
        let config = RequestConfig::default();
        let messages = build_request_messages(&[user("hello")], &config).unwrap();
        assert!(cache_point_positions(&messages).is_empty());

        let system = build_system_content("instructions", false).unwrap();
        assert_eq!(system.len(), 1);
        assert!(matches!(
            system.as_slice(),
            [bedrock::types::SystemContentBlock::Text(text)] if text == "instructions"
        ));

        let tool_config =
            build_tool_configuration(&[crate::Tool::new("read", "Read a file")], false).unwrap();
        assert_eq!(tool_config.tools().len(), 1);
        assert!(matches!(
            tool_config.tools().first(),
            Some(bedrock::types::Tool::ToolSpec(_))
        ));
    }

    #[test]
    fn unsupported_model_ignores_explicit_cache_flag() {
        let history = vec![user("hello")];
        let mut config = RequestConfig {
            model: "bedrock/amazon.nova-pro-v1:0".into(),
            cache_system_prompt: true,
            ..RequestConfig::default()
        };
        config.cache_topology = Some(
            crate::cache_topology::PreparedPrompt::prepare(
                &history,
                &config,
                "session".into(),
                None,
            )
            .unwrap(),
        );

        let messages = build_request_messages(&history, &config).unwrap();

        assert!(cache_point_positions(&messages).is_empty());
    }

    #[tokio::test]
    async fn converse_stream_uses_sigv4_and_resolved_model_endpoint() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).unwrap_or_default();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers.lines().find_map(|line| {
                        line.strip_prefix("content-length:")
                            .or_else(|| line.strip_prefix("Content-Length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    });
                    if content_length.is_some_and(|length| request.len() >= header_end + 4 + length)
                    {
                        break;
                    }
                }
            }
            let body = br#"{"message":"mock rejection"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
            socket.write_all(body).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });

        let env = static_env();
        let client =
            BedrockClient::from_runtime_env(&env, Some(&format!("http://{}", address))).unwrap();
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::text("hello"),
        }];
        let config = RequestConfig {
            model: "bedrock/amazon.nova-lite-v1:0".to_string(),
            ..RequestConfig::default()
        };
        let error = match client.stream(&messages, &config).await {
            Ok(_) => panic!("mock Bedrock rejection must surface as an error"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Bedrock ConverseStream request failed"));

        let request = server.join().unwrap();
        assert!(
            request.starts_with("POST /model/amazon.nova-lite-v1%3A0/converse-stream"),
            "captured request: {request}"
        );
        let request_lower = request.to_ascii_lowercase();
        assert!(request_lower.contains("authorization: aws4-hmac-sha256"));
        assert!(request.contains("\"messages\":[{\"role\":\"user\""));
    }

    #[test]
    fn missing_runtime_configuration_fails_closed() {
        let error = match BedrockClient::from_runtime_env(&HashMap::new(), None) {
            Ok(_) => panic!("missing AWS configuration must fail closed"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("AWS_ACCESS_KEY_ID"));
        assert!(error.contains("AWS_PROFILE"));
    }

    #[test]
    fn partial_static_credentials_fail_closed() {
        let env = HashMap::from([(AWS_ACCESS_KEY_ID.to_string(), "access".to_string())]);
        let error = match BedrockClient::from_runtime_env(&env, None) {
            Ok(_) => panic!("partial AWS credentials must fail closed"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(AWS_SECRET_ACCESS_KEY));
    }

    #[test]
    fn tool_schema_document_conversion_preserves_nested_json() {
        let value = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        let document = document_from_json(&value);
        assert!(document.as_object().is_some());
        assert!(document.as_object().unwrap().contains_key("properties"));
    }

    #[test]
    fn reasoning_signature_is_attached_to_content_block_stop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut thinking_signatures = HashMap::new();
        let delta = bedrock::types::ContentBlockDeltaEvent::builder()
            .content_block_index(2)
            .delta(bedrock::types::ContentBlockDelta::ReasoningContent(
                bedrock::types::ReasoningContentBlockDelta::Signature("sig-123".to_string()),
            ))
            .build()
            .unwrap();
        send_event(
            &tx,
            bedrock::types::ConverseStreamOutput::ContentBlockDelta(delta),
            &mut thinking_signatures,
        )
        .unwrap();
        let stop = bedrock::types::ContentBlockStopEvent::builder()
            .content_block_index(2)
            .build()
            .unwrap();
        send_event(
            &tx,
            bedrock::types::ConverseStreamOutput::ContentBlockStop(stop),
            &mut thinking_signatures,
        )
        .unwrap();

        assert!(matches!(
            rx.try_recv().unwrap(),
            StreamEvent::ThinkingSignature { index: 2, signature } if signature == "sig-123"
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            StreamEvent::ContentBlockStop {
                index: 2,
                thinking_signature: Some(signature)
            } if signature == "sig-123"
        ));
        assert!(thinking_signatures.is_empty());
    }

    #[test]
    fn unsupported_output_block_fails_closed() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut thinking_signatures = HashMap::new();
        let start = bedrock::types::ContentBlockStartEvent::builder()
            .content_block_index(0)
            .start(bedrock::types::ContentBlockStart::Image(
                bedrock::types::ImageBlockStart::builder()
                    .format(bedrock::types::ImageFormat::Png)
                    .build()
                    .unwrap(),
            ))
            .build()
            .unwrap();
        let error = send_event(
            &tx,
            bedrock::types::ConverseStreamOutput::ContentBlockStart(start),
            &mut thinking_signatures,
        )
        .expect_err("unsupported output blocks must not be silently discarded")
        .to_string();
        assert!(error.contains("unsupported Bedrock image content-block start"));
    }

    #[test]
    fn text_content_block_start_without_payload_is_preserved() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut thinking_signatures = HashMap::new();
        let start = bedrock::types::ContentBlockStartEvent::builder()
            .content_block_index(1)
            .build()
            .unwrap();
        send_event(
            &tx,
            bedrock::types::ConverseStreamOutput::ContentBlockStart(start),
            &mut thinking_signatures,
        )
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlock::Text { text }
            } if text.is_empty()
        ));
    }
}
