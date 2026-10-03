//! A `dex_loop::Model` adapter over `maestro-ai`'s `UnifiedClient::stream`.
//!
//! First real port of `docs/design/maestro-on-dex-loop.md`'s cutover step 2
//! ("Build the real `Model` adapter over `ai_rs::AiClient::stream`"). Scope
//! matches the rest of this crate's "first slice" framing: one call at a
//! time, no retries beyond what `UnifiedClient::stream` already does, no
//! prompt caching, no thinking-block translation, and cost is not yet
//! attributed (`Usage::cost_micros` is always `0` -- `maestro-ai` reports
//! cost as a separate `StreamEvent::ProviderCost`, not on the `Usage` event
//! this adapter reads). None of that is a correctness bug for the local
//! `run_local_turn` consumer this backs; it is exactly what "first slice"
//! means, called out here the way the crate doc comment calls out the rest.

use std::collections::BTreeMap;
use std::sync::Arc;

use dex_loop::{
    Context, Entry, Message as LoopMessage, ModelChunk, ModelError, Outcome, Output, OutputBlock,
    ToolName, ToolSpec, Usage,
};
use futures_util::Stream;
use maestro_ai::{
    ContentBlock as AiContentBlock, ImageSource, Message as AiMessage, MessageContent,
    RequestConfig, Role, StreamEvent, Tool as AiTool, UnifiedClient,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Wraps one `maestro_ai::UnifiedClient` as a `dex_loop::Model`.
#[derive(Clone)]
pub struct AiRsModel {
    client: UnifiedClient,
    model: String,
    max_tokens: u32,
    system: Option<String>,
}

impl AiRsModel {
    #[must_use]
    pub fn new(client: UnifiedClient, model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens,
            system: None,
        }
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
        LoopMessage::User { text, .. } | LoopMessage::Summary { text } => Some(AiMessage {
            role: Role::User,
            content: MessageContent::text(text.clone()),
        }),
        LoopMessage::Assistant { text, calls, .. } => {
            if calls.is_empty() {
                return Some(AiMessage {
                    role: Role::Assistant,
                    content: MessageContent::text(text.clone()),
                });
            }
            let mut blocks = Vec::with_capacity(calls.len() + 1);
            if !text.is_empty() {
                blocks.push(AiContentBlock::Text { text: text.clone() });
            }
            for call in calls {
                blocks.push(AiContentBlock::ToolUse {
                    id: call.id.as_str().to_owned(),
                    name: call.tool.as_str().to_owned(),
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

fn to_ai_tools(tools: &[&ToolSpec]) -> Vec<AiTool> {
    tools
        .iter()
        .map(|spec| AiTool {
            name: spec.name.as_str().to_owned(),
            description: spec.label.clone(),
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
}

impl ChunkTranslator {
    fn translate(&mut self, event: StreamEvent) -> Vec<Result<ModelChunk, ModelError>> {
        match event {
            StreamEvent::TextDelta { text, .. } => vec![Ok(ModelChunk::Text(text))],
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
            StreamEvent::ContentBlockStop { index, .. } => {
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
                    name: ToolName::new(name),
                    args,
                })]
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => vec![Ok(ModelChunk::Usage(Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens: cache_read_tokens.unwrap_or_default(),
                cache_creation_input_tokens: cache_creation_tokens.unwrap_or_default(),
                // Not attributed here; see the module doc comment.
                cost_micros: 0,
            }))],
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
        let client = self.client.clone();
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
                    let mut translator = ChunkTranslator::default();
                    while let Some(event) = source.recv().await {
                        for chunk in translator.translate(event) {
                            if tx.send(chunk).is_err() {
                                return;
                            }
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
        let chunks = ChunkTranslator::default().translate(StreamEvent::Usage {
            input_tokens: 10,
            output_tokens: 3,
            cache_read_tokens: Some(20),
            cache_creation_tokens: Some(7),
        });
        assert!(matches!(&chunks[0], Ok(ModelChunk::Usage(usage))
            if usage.input_tokens == 10 && usage.output_tokens == 3
                && usage.cache_read_input_tokens == 20
                && usage.cache_creation_input_tokens == 7));
        let absent = ChunkTranslator::default().translate(StreamEvent::Usage {
            input_tokens: 10,
            output_tokens: 3,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        });
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
