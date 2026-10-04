//! A `dex_loop::Model` adapter over `maestro-ai`'s `UnifiedClient::stream`.
//!
//! The port of `docs/design/maestro-on-dex-loop.md`'s cutover step 2. One
//! call at a time, no retries beyond what `UnifiedClient::stream` already
//! does, and no prompt caching. What a request carries matches the native
//! actor's provider path:
//!
//! - Tools are declared under `dex_loop::model_tool_name` (providers reject
//!   dots) with their model-facing description, and a returned call is mapped
//!   back to its registry name.
//! - Extended thinking streams as `ModelChunk::Thinking`. Its signed blocks
//!   return as `ModelChunk::Reasoning` and are replayed on later steps, which
//!   Anthropic requires to continue a turn after a tool call.
//! - Provider cost (`StreamEvent::ProviderCost`) is charged on the step's
//!   `Usage`, which is sent once after a clean terminal, after `Reasoning`.
//! - A managed-gateway client gets one lineage per dex-loop turn
//!   (`managed_turn_lineage_id`), as the native actor gets one per prompt.
//! - Managed-gateway receipts, the governance evidence for a managed call,
//!   have no `ModelChunk`; they go to the host on a side channel
//!   (`with_receipts`) so it can show them as the native actor does.
//! - Image attachments on a user message are local files (the host's
//!   `ArtifactRef` is the path) sent as image blocks.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use base64::Engine as _;

use dex_loop::{
    Context, Entry, Message as LoopMessage, ModelChunk, ModelError, Outcome, Output, OutputBlock,
    ToolName, ToolSpec, Usage,
};
use futures_util::Stream;
use maestro_ai::{
    ContentBlock as AiContentBlock, ImageSource, ManagedGatewayReceipt, Message as AiMessage,
    MessageContent, RequestConfig, Role, StreamEvent, ThinkingConfig, Tool as AiTool,
    UnifiedClient,
};
use maestro_runtime::agent::managed_turn_lineage_id;

/// `ProviderReasoning::format` for Anthropic thinking blocks.
const ANTHROPIC_THINKING: &str = "anthropic.messages.v1";
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Wraps one `maestro_ai::UnifiedClient` as a `dex_loop::Model`.
#[derive(Clone)]
pub struct AiRsModel {
    client: UnifiedClient,
    model: String,
    max_tokens: u32,
    system: Option<String>,
    thinking_budget: Option<u32>,
    receipts: Option<tokio::sync::mpsc::UnboundedSender<ManagedGatewayReceipt>>,
    /// Distinguishes this model's turns from another process's in a
    /// managed lineage id.
    run_id: String,
}

impl AiRsModel {
    #[must_use]
    pub fn new(client: UnifiedClient, model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens,
            system: None,
            thinking_budget: None,
            receipts: None,
            run_id: format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
            ),
        }
    }

    /// Sends each managed-gateway receipt to `receipts` as it arrives.
    #[must_use]
    pub fn with_receipts(
        mut self,
        receipts: tokio::sync::mpsc::UnboundedSender<ManagedGatewayReceipt>,
    ) -> Self {
        self.receipts = Some(receipts);
        self
    }

    /// Requests extended thinking with `budget` tokens.
    #[must_use]
    pub fn with_thinking(mut self, budget: u32) -> Self {
        self.thinking_budget = Some(budget);
        self
    }

    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    fn request_config(&self, tools: Vec<AiTool>) -> RequestConfig {
        RequestConfig {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            system: self.system.clone(),
            tools: Arc::new(tools),
            thinking: self.thinking_budget.map(ThinkingConfig::enabled),
            // Temperature must be omitted while thinking.
            temperature: if self.thinking_budget.is_some() {
                None
            } else {
                Some(0.7)
            },
            ..RequestConfig::default()
        }
    }
}

fn to_ai_messages(history: &[Entry]) -> Result<Vec<AiMessage>, ModelError> {
    let mut messages = Vec::new();
    let mut images = Vec::new();
    for entry in history {
        if !matches!(&entry.message, LoopMessage::Tool { .. }) {
            flush_tool_images(&mut messages, &mut images);
        }
        if let LoopMessage::Tool {
            output: Output::Blocks(blocks),
            ..
        } = &entry.message
        {
            dex_loop::validate_codemode_blocks(blocks).map_err(|message| ModelError {
                class: dex_loop::ErrorClass::Protocol,
                message: format!("Invalid typed tool output: {message}"),
            })?;
            for block in blocks {
                if let OutputBlock::Image { mime_type, data } = block {
                    images.push(AiContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: mime_type.clone(),
                            data: data.clone(),
                            owner: None,
                        },
                    });
                }
            }
        }
        if let Some(message) = to_ai_message(&entry.message) {
            messages.push(message);
        }
    }
    flush_tool_images(&mut messages, &mut images);
    Ok(messages)
}

fn flush_tool_images(messages: &mut Vec<AiMessage>, images: &mut Vec<AiContentBlock>) {
    if !images.is_empty() {
        // Keep every peer result together before starting a new user message;
        // Chat Completions drops images placed beside a ToolResult block.
        messages.push(AiMessage {
            role: Role::User,
            content: MessageContent::Blocks(std::mem::take(images)),
        });
    }
}

fn to_ai_message(message: &LoopMessage) -> Option<AiMessage> {
    match message {
        LoopMessage::User {
            text, attachments, ..
        } => {
            let images: Vec<AiContentBlock> = attachments
                .iter()
                .filter_map(|attachment| image_block(attachment.as_str()))
                .collect();
            if images.is_empty() {
                return Some(AiMessage {
                    role: Role::User,
                    content: MessageContent::text(text.clone()),
                });
            }
            let mut blocks = vec![AiContentBlock::Text { text: text.clone() }];
            blocks.extend(images);
            Some(AiMessage {
                role: Role::User,
                content: MessageContent::Blocks(blocks),
            })
        }
        LoopMessage::Summary { text } => Some(AiMessage {
            role: Role::User,
            content: MessageContent::text(text.clone()),
        }),
        LoopMessage::Assistant {
            text,
            calls,
            reasoning,
            ..
        } => {
            let thinking = thinking_blocks(reasoning.as_ref());
            if calls.is_empty() && thinking.is_empty() {
                return Some(AiMessage {
                    role: Role::Assistant,
                    content: MessageContent::text(text.clone()),
                });
            }
            let mut blocks = thinking;
            if !text.is_empty() {
                blocks.push(AiContentBlock::Text { text: text.clone() });
            }
            for call in calls {
                blocks.push(AiContentBlock::ToolUse {
                    id: call.id.as_str().to_owned(),
                    name: dex_loop::model_tool_name(call.tool.as_str()),
                    input: call.args.clone(),
                    gemini_context: None,
                });
            }
            Some(AiMessage {
                role: Role::Assistant,
                content: MessageContent::Blocks(blocks),
            })
        }
        LoopMessage::Tool {
            call,
            outcome,
            output,
            ..
        } => {
            let content = match output {
                Output::Text(text) => text.clone(),
                // Pixels are projected separately after all sibling results.
                Output::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        OutputBlock::Text { text } => Some(text.as_str()),
                        OutputBlock::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                // `LocalTools` never produces this variant (see `tools.rs`);
                // a real out-of-line store needs its own resolver here.
                Output::Ref(_) => {
                    "[tool output stored out of line; not available to this host]".to_owned()
                }
            };
            Some(AiMessage {
                role: Role::User,
                content: MessageContent::Blocks(vec![AiContentBlock::ToolResult {
                    tool_use_id: call.as_str().to_owned(),
                    content,
                    is_error: Some(matches!(outcome, Outcome::Failed)),
                }]),
            })
        }
    }
}

/// A local image file as an image block; anything else stays a note in the
/// message text.
fn image_block(path: &str) -> Option<AiContentBlock> {
    let extension = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    let media_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    let bytes = std::fs::read(path).ok()?;
    Some(AiContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: media_type.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            owner: None,
        },
    })
}

/// The signed thinking blocks a step returned, to send back unchanged.
fn thinking_blocks(reasoning: Option<&dex_loop::ProviderReasoning>) -> Vec<AiContentBlock> {
    let Some(reasoning) = reasoning.filter(|reasoning| reasoning.format == ANTHROPIC_THINKING)
    else {
        return Vec::new();
    };
    reasoning
        .payload
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            Some(AiContentBlock::Thinking {
                thinking: block.get("thinking")?.as_str()?.to_owned(),
                signature: block
                    .get("signature")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn to_ai_tools(tools: &[&ToolSpec]) -> Vec<AiTool> {
    tools
        .iter()
        .map(|spec| AiTool {
            namespace_instructions: None,
            name: dex_loop::model_tool_name(spec.name.as_str()),
            description: if spec.description.is_empty() {
                spec.label.clone()
            } else {
                spec.description.clone()
            },
            input_schema: spec.schema.clone(),
            output_schema: None,
            schema_enforcement: Default::default(),
        })
        .collect()
}

/// Accumulates one streamed response's tool-call JSON per content-block
/// index (providers stream `InputJsonDelta` chunks between a block's
/// `ContentBlockStart` and `ContentBlockStop`), and turns each provider
/// `StreamEvent` into zero or more `ModelChunk`s.
#[derive(Default)]
struct ChunkTranslator {
    pending_tools: BTreeMap<usize, (String, String)>, // index -> (tool name, json buffer)
    /// Model-facing tool name -> registry name.
    names: HashMap<String, ToolName>,
    model: String,
    /// index -> (thinking text, signature)
    thinking: BTreeMap<usize, (String, Option<String>)>,
    usage: Option<Usage>,
    cost_micros: u64,
    failed: bool,
}

impl ChunkTranslator {
    fn new(tools: &[&ToolSpec], model: &str) -> Self {
        Self {
            names: tools
                .iter()
                .map(|spec| {
                    (
                        dex_loop::model_tool_name(spec.name.as_str()),
                        spec.name.clone(),
                    )
                })
                .collect(),
            model: model.to_owned(),
            ..Self::default()
        }
    }

    fn tool_name(&self, name: String) -> ToolName {
        self.names
            .get(&name)
            .cloned()
            .unwrap_or_else(|| ToolName::new(name))
    }

    /// What a clean terminal commits, in the engine's order: the step's
    /// reasoning, then its usage with any provider cost charged.
    fn finish(&mut self) -> Vec<Result<ModelChunk, ModelError>> {
        if self.failed {
            return Vec::new();
        }
        let mut chunks = Vec::new();
        if !self.thinking.is_empty() {
            let payload = std::mem::take(&mut self.thinking)
                .into_values()
                .map(|(thinking, signature)| {
                    serde_json::json!({ "thinking": thinking, "signature": signature })
                })
                .collect();
            chunks.push(Ok(ModelChunk::Reasoning(dex_loop::ProviderReasoning {
                format: ANTHROPIC_THINKING.to_owned(),
                model: self.model.clone(),
                payload: serde_json::Value::Array(payload),
            })));
        }
        if let Some(mut usage) = self.usage.take() {
            usage.cost_micros = usage.cost_micros.saturating_add(self.cost_micros);
            chunks.push(Ok(ModelChunk::Usage(usage)));
        }
        chunks
    }

    fn translate(&mut self, event: StreamEvent) -> Vec<Result<ModelChunk, ModelError>> {
        let chunks = self.translate_event(event);
        if chunks.iter().any(Result::is_err) {
            self.failed = true;
        }
        chunks
    }

    fn translate_event(&mut self, event: StreamEvent) -> Vec<Result<ModelChunk, ModelError>> {
        match event {
            StreamEvent::TextDelta { text, .. } => vec![Ok(ModelChunk::Text(text))],
            StreamEvent::ThinkingDelta { index, thinking } => {
                self.thinking
                    .entry(index)
                    .or_default()
                    .0
                    .push_str(&thinking);
                vec![Ok(ModelChunk::Thinking(thinking))]
            }
            StreamEvent::ThinkingSignature { index, signature } => {
                self.thinking.entry(index).or_default().1 = Some(signature);
                Vec::new()
            }
            StreamEvent::ProviderCost { cost_usd } => {
                if cost_usd.is_finite() && cost_usd > 0.0 {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let micros = (cost_usd * 1_000_000.0).round() as u64;
                    self.cost_micros = self.cost_micros.saturating_add(micros);
                }
                Vec::new()
            }
            StreamEvent::ContentBlockStart {
                index,
                block: AiContentBlock::ToolUse { name, .. },
            } => {
                self.pending_tools.insert(index, (name, String::new()));
                Vec::new()
            }
            StreamEvent::InputJsonDelta {
                index,
                partial_json,
            } => {
                if let Some((_, buffer)) = self.pending_tools.get_mut(&index) {
                    buffer.push_str(&partial_json);
                }
                Vec::new()
            }
            StreamEvent::ContentBlockStop {
                index,
                thinking_signature,
            } => {
                if let (Some(signature), Some(block)) =
                    (thinking_signature, self.thinking.get_mut(&index))
                {
                    block.1 = Some(signature);
                }
                let Some((name, buffer)) = self.pending_tools.remove(&index) else {
                    return Vec::new();
                };
                let args = if buffer.trim().is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    match serde_json::from_str(&buffer) {
                        Ok(value) => value,
                        Err(error) => {
                            return vec![Err(ModelError {
                                class: dex_loop::ErrorClass::Protocol,
                                message: format!(
                                    "provider returned invalid tool-call JSON for {name}: {error}"
                                ),
                            })];
                        }
                    }
                };
                vec![Ok(ModelChunk::ToolCall {
                    name: self.tool_name(name),
                    args,
                })]
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                // Sent by `finish`, after any reasoning, with cost charged.
                self.usage = Some(Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens: cache_read_tokens.unwrap_or_default(),
                    cache_creation_input_tokens: cache_creation_tokens.unwrap_or_default(),
                    cost_micros: 0,
                });
                Vec::new()
            }
            StreamEvent::ProviderError { kind, message } => {
                use maestro_ai::ProviderStreamErrorKind;
                let class = match kind {
                    ProviderStreamErrorKind::TransientProtocol => dex_loop::ErrorClass::Truncated,
                    ProviderStreamErrorKind::OutputTokenExhaustion
                    | ProviderStreamErrorKind::IncompleteResponse => {
                        dex_loop::ErrorClass::Incomplete
                    }
                    ProviderStreamErrorKind::ProviderDeclaredFailure => {
                        dex_loop::ErrorClass::Unknown
                    }
                };
                vec![Err(ModelError { class, message })]
            }
            StreamEvent::Error { message } => {
                vec![Err(ModelError {
                    class: dex_loop::ErrorClass::Unknown,
                    message,
                })]
            }
            _ => Vec::new(),
        }
    }
}

impl dex_loop::Model for AiRsModel {
    fn stream<'a>(
        &'a self,
        ctx: &'a Context,
        tools: &'a [&'a ToolSpec],
    ) -> impl Stream<Item = Result<ModelChunk, ModelError>> + Send + 'a {
        let messages = to_ai_messages(ctx.history());
        let config = self.request_config(to_ai_tools(tools));
        let mut translator = ChunkTranslator::new(tools, &self.model);
        let receipts = self.receipts.clone();
        let mut client = self.client.clone();
        let lineage = client
            .managed_gateway_scope()
            .filter(|_| client.is_managed_gateway())
            .map(|(organization, workspace)| {
                managed_turn_lineage_id(
                    organization,
                    workspace,
                    &ctx.thread().thread,
                    &self.run_id,
                    ctx.turn().map_or("turn", |turn| turn.as_str()),
                )
            });
        if lineage.is_some() {
            client.set_managed_request_lineage(lineage);
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let messages = match messages {
                Ok(messages) => messages,
                Err(error) => {
                    let _ = tx.send(Err(error));
                    return;
                }
            };
            match client.stream(&messages, &config).await {
                Ok(mut source) => {
                    while let Some(event) = source.recv().await {
                        if let (StreamEvent::ManagedGatewayReceipt(receipt), Some(receipts)) =
                            (&event, &receipts)
                        {
                            let _ = receipts.send(receipt.clone());
                        }
                        for chunk in translator.translate(event) {
                            if tx.send(chunk).is_err() {
                                return;
                            }
                        }
                    }
                    for chunk in translator.finish() {
                        if tx.send(chunk).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(ModelError {
                        class: dex_loop::ErrorClass::Unknown,
                        message: format!("{error:#}"),
                    }));
                }
            }
        });
        UnboundedReceiverStream::new(rx)
    }
}

#[cfg(test)]
#[path = "model_image_tests.rs"]
mod image_projection_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use dex_loop::{Model as _, PrincipalId, ThreadId};
    use futures_util::StreamExt;
    use maestro_ai::{ScriptedBlock, ScriptedClient, ScriptedResponse};

    fn thread() -> ThreadId {
        ThreadId {
            org: "org-1".into(),
            workspace: "ws-1".into(),
            thread: "thread-1".into(),
        }
    }

    #[test]
    fn failure_class_comes_from_the_provider_kind_not_display_words() {
        use maestro_ai::ProviderStreamErrorKind;
        for (kind, expected) in [
            (
                ProviderStreamErrorKind::TransientProtocol,
                dex_loop::ErrorClass::Truncated,
            ),
            (
                ProviderStreamErrorKind::OutputTokenExhaustion,
                dex_loop::ErrorClass::Incomplete,
            ),
            (
                ProviderStreamErrorKind::IncompleteResponse,
                dex_loop::ErrorClass::Incomplete,
            ),
            (
                ProviderStreamErrorKind::ProviderDeclaredFailure,
                dex_loop::ErrorClass::Unknown,
            ),
        ] {
            let errors = ChunkTranslator::default().translate(StreamEvent::ProviderError {
                kind,
                message: "budget auth 429 refusal".into(),
            });
            assert_eq!(errors[0].as_ref().unwrap_err().class, expected);
        }
        let errors = ChunkTranslator::default().translate(StreamEvent::Error {
            message: "budget auth 429 refusal".into(),
        });
        assert_eq!(
            errors[0].as_ref().unwrap_err().class,
            dex_loop::ErrorClass::Unknown
        );
    }

    #[test]
    fn retains_provider_reported_cache_usage() {
        let mut translator = ChunkTranslator::default();
        assert!(
            translator
                .translate(StreamEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                    cache_read_tokens: Some(20),
                    cache_creation_tokens: Some(7),
                })
                .is_empty()
        );
        let chunks = translator.finish();
        assert!(matches!(&chunks[0], Ok(ModelChunk::Usage(usage))
            if usage.input_tokens == 10 && usage.output_tokens == 3
                && usage.cache_read_input_tokens == 20
                && usage.cache_creation_input_tokens == 7));
        let mut translator = ChunkTranslator::default();
        translator.translate(StreamEvent::Usage {
            input_tokens: 10,
            output_tokens: 3,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        });
        let absent = translator.finish();
        assert!(matches!(&absent[0], Ok(ModelChunk::Usage(usage))
            if usage.cache_read_input_tokens == 0 && usage.cache_creation_input_tokens == 0));
    }

    #[tokio::test]
    async fn translates_text_then_tool_call_then_usage() {
        let scripted = ScriptedClient::new(
            "scripted",
            vec![ScriptedResponse {
                blocks: vec![
                    ScriptedBlock::Text("Reading the file.".to_owned()),
                    ScriptedBlock::ToolUse {
                        id: "call-1".to_owned(),
                        name: "fs.read_file".to_owned(),
                        input: serde_json::json!({"path": "a.txt"}),
                    },
                ],
                stop_reason: maestro_ai::StopReason::ToolUse,
                error: None,
            }],
        );
        let model = AiRsModel::new(UnifiedClient::Scripted(scripted), "scripted-model", 1024);

        let ctx = dex_loop::rehydrate(
            thread(),
            &[(
                dex_loop::Cursor(1),
                dex_loop::Event::UserMessage {
                    turn: dex_loop::TurnId::new("t1"),
                    message_id: None,
                    model_binding: None,
                    principal: PrincipalId::new("alice"),
                    text: "read a.txt".into(),
                    attachments: Vec::new(),
                    client_tools: Vec::new(),
                    authorized_tools: Vec::new(),
                    approval_mode: dex_loop::ApprovalMode::Headless,
                },
            )],
        );
        let spec = ToolSpec {
            description: String::new(),
            name: ToolName::new("fs.read_file"),
            label: "Read a file".into(),
            schema: serde_json::json!({"type": "object"}),
            read_only: true,
            core: true,
            governance: dex_loop::GovernanceClass::Plain,
            executor: dex_loop::ExecutorKind::InProcess,
        };
        let tools = [&spec];
        let chunks: Vec<_> = model.stream(&ctx, &tools).collect().await;
        let chunks: Vec<ModelChunk> = chunks
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("no model errors");

        // Text, then the assembled tool call, then the trailing usage chunk
        // every scripted response emits.
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert!(matches!(&chunks[0], ModelChunk::Text(text) if text == "Reading the file."));
        assert!(matches!(
            &chunks[1],
            ModelChunk::ToolCall { name, args }
                if name.as_str() == "fs.read_file" && args["path"] == "a.txt"
        ));
        assert!(matches!(&chunks[2], ModelChunk::Usage(_)));
    }

    #[test]
    fn thinking_streams_then_commits_signed_reasoning_before_costed_usage() {
        let spec = ToolSpec {
            description: "Ask".into(),
            name: ToolName::new("user.ask"),
            label: "Confirm an action".into(),
            schema: serde_json::json!({"type": "object"}),
            read_only: true,
            core: true,
            governance: dex_loop::GovernanceClass::Plain,
            executor: dex_loop::ExecutorKind::User,
        };
        let mut translator = ChunkTranslator::new(&[&spec], "claude");
        let mut chunks = Vec::new();
        for event in [
            StreamEvent::ThinkingDelta {
                index: 0,
                thinking: "consider".into(),
            },
            StreamEvent::ThinkingSignature {
                index: 0,
                signature: "sig".into(),
            },
            StreamEvent::ContentBlockStart {
                index: 1,
                block: AiContentBlock::ToolUse {
                    id: "p1".into(),
                    name: "user_ask".into(),
                    input: serde_json::json!({}),
                    gemini_context: None,
                },
            },
            StreamEvent::InputJsonDelta {
                index: 1,
                partial_json: r#"{"question":"ok?"}"#.into(),
            },
            StreamEvent::ContentBlockStop {
                index: 1,
                thinking_signature: None,
            },
            StreamEvent::Usage {
                input_tokens: 5,
                output_tokens: 2,
                cache_read_tokens: None,
                cache_creation_tokens: None,
            },
            StreamEvent::ProviderCost { cost_usd: 0.0125 },
        ] {
            chunks.extend(translator.translate(event));
        }
        chunks.extend(translator.finish());
        let chunks: Vec<ModelChunk> = chunks.into_iter().map(Result::unwrap).collect();
        assert!(matches!(&chunks[0], ModelChunk::Thinking(text) if text == "consider"));
        assert!(
            matches!(&chunks[1], ModelChunk::ToolCall { name, .. } if name.as_str() == "user.ask")
        );
        let ModelChunk::Reasoning(reasoning) = &chunks[2] else {
            panic!("reasoning after the last call: {chunks:?}");
        };
        assert!(matches!(&chunks[3], ModelChunk::Usage(usage) if usage.cost_micros == 12_500));
        let replay = thinking_blocks(Some(reasoning));
        assert!(matches!(
            replay.as_slice(),
            [AiContentBlock::Thinking { thinking, signature: Some(signature) }]
                if thinking == "consider" && signature == "sig"
        ));
    }

    #[test]
    fn tools_are_declared_under_provider_safe_names_with_their_description() {
        let spec = ToolSpec {
            description: "Ask the person".into(),
            name: ToolName::new("user.ask"),
            label: "Confirm an action".into(),
            schema: serde_json::json!({"type": "object"}),
            read_only: true,
            core: true,
            governance: dex_loop::GovernanceClass::Plain,
            executor: dex_loop::ExecutorKind::User,
        };
        let declared = to_ai_tools(&[&spec]);
        assert_eq!(declared[0].name, "user_ask");
        assert_eq!(declared[0].description, "Ask the person");
    }

    #[test]
    fn an_image_attachment_becomes_an_image_block() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let image = dir.path().join("shot.png");
        std::fs::write(&image, [0x89, b'P', b'N', b'G']).expect("write image");
        let note = dir.path().join("notes.txt");
        std::fs::write(&note, "text").expect("write note");
        assert!(matches!(
            image_block(image.to_str().expect("utf-8")),
            Some(AiContentBlock::Image { source: ImageSource::Base64 { media_type, .. } })
                if media_type == "image/png"
        ));
        assert!(image_block(note.to_str().expect("utf-8")).is_none());
    }

    #[tokio::test]
    async fn surfaces_provider_errors_as_model_errors() {
        let scripted = ScriptedClient::new(
            "scripted",
            vec![ScriptedResponse::stream_error("upstream exploded")],
        );
        let model = AiRsModel::new(UnifiedClient::Scripted(scripted), "scripted-model", 1024);
        let ctx = dex_loop::rehydrate(thread(), &[]);
        let tools: [&ToolSpec; 0] = [];
        let chunks: Vec<_> = model.stream(&ctx, &tools).collect().await;
        assert!(
            chunks.iter().any(
                |chunk| matches!(chunk, Err(error) if error.message.contains("upstream exploded"))
            ),
            "expected an error chunk, got {chunks:?}"
        );
    }
}
