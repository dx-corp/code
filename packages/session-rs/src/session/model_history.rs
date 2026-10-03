//! Reconstruct provider history from persisted content rather than display text.
use super::{AppMessage, ContentBlock, MessageContent, ParsedSession};
use maestro_ai::{self as ai, Message, Role};
use maestro_context::render_context_summary;

pub fn model_history(session: &ParsedSession) -> Vec<Message> {
    // Compaction indices exclude tool results and previous summaries.
    let mut history: Vec<(Message, bool)> = session
        .messages
        .iter()
        .map(|entry| {
            let (role, content, visible) = match entry {
                AppMessage::User { content, .. } => (
                    Role::User,
                    match content {
                        MessageContent::Text(text) => ai::MessageContent::text(text),
                        MessageContent::Blocks(blocks) => {
                            ai::MessageContent::Blocks(convert_blocks(blocks))
                        }
                    },
                    true,
                ),
                AppMessage::Assistant { content, .. } => (
                    Role::Assistant,
                    ai::MessageContent::Blocks(convert_blocks(content)),
                    true,
                ),
                AppMessage::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                    ..
                } => (
                    Role::User,
                    ai::MessageContent::Blocks(vec![ai::ContentBlock::ToolResult {
                        tool_use_id: tool_call_id.clone(),
                        content: content.clone(),
                        is_error: Some(*is_error),
                    }]),
                    false,
                ),
            };
            (Message { role, content }, visible)
        })
        .collect();
    if let Some((checkpoint, display_len)) = &session.selective_summary_context {
        let prefix = checkpoint.iter().cloned().map(|message| {
            let visible = super::selective_summary::context_message_visible(&message);
            (message, visible)
        });
        history.splice(..(*display_len).min(history.len()), prefix);
    }
    for compaction in &session.compactions {
        if let Some(index) = compaction.first_kept_entry_index {
            let boundary = if index == 0 {
                0
            } else {
                history
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, visible))| *visible)
                    .nth(index)
                    .map_or(history.len(), |(position, _)| position)
            };
            history.drain(..boundary);
            history.insert(
                0,
                (
                    Message {
                        role: Role::User,
                        content: ai::MessageContent::text(render_context_summary(
                            &compaction.summary,
                        )),
                    },
                    false,
                ),
            );
        }
    }
    history.into_iter().map(|(message, _)| message).collect()
}

fn convert_blocks(blocks: &[ContentBlock]) -> Vec<ai::ContentBlock> {
    blocks
        .iter()
        .filter_map(|block| {
            Some(match block {
                ContentBlock::Text { text } => ai::ContentBlock::Text { text: text.clone() },
                ContentBlock::Thinking { text, signature } => ai::ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                },
                ContentBlock::ToolCall {
                    id,
                    name,
                    args,
                    gemini_context,
                    ..
                } => ai::ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: args.clone(),
                    gemini_context: gemini_context.clone(),
                },
                ContentBlock::Image {
                    source,
                    data,
                    mime_type,
                } => ai::ContentBlock::Image {
                    source: if let Some(source) = source {
                        ai::ImageSource::Base64 {
                            media_type: source.media_type.clone(),
                            data: source.data.clone(),
                            owner: source.owner.clone(),
                        }
                    } else {
                        ai::ImageSource::Base64 {
                            media_type: mime_type.clone()?,
                            data: data.clone()?,
                            owner: None,
                        }
                    },
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn persisted_gemini_call_retains_native_replay_context() {
        let context = serde_json::json!({"native_name":"computer_read_file", "native_id":"native-1", "thought_signature":"opaque-signature"});
        let encoded = serde_json::json!({"type":"toolCall", "id":"call-1", "name":"computer.read_file",
            "arguments":{"path":"a.rs"}, "gemini_context":context});
        let block: ContentBlock = serde_json::from_value(encoded).unwrap();
        let restored = convert_blocks(&[block]);
        let ai::ContentBlock::ToolUse {
            gemini_context,
            input,
            ..
        } = &restored[0]
        else {
            panic!("tool call")
        };
        assert_eq!(serde_json::to_value(gemini_context).unwrap(), context);
        assert_eq!(input, &serde_json::json!({"path":"a.rs"}));
        let legacy: ContentBlock = serde_json::from_value(serde_json::json!({
            "type":"toolCall", "id":"old-call", "name":"read", "arguments":{}
        }))
        .unwrap();
        assert!(matches!(
            &convert_blocks(&[legacy])[0],
            ai::ContentBlock::ToolUse {
                gemini_context: None,
                ..
            }
        ));
    }

    #[test]
    fn restored_history_preserves_tool_calls_results_and_summary_framing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for entry in [
            serde_json::json!({"type":"session","id":"history-test","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp","model":"openai/gpt-4o"}),
            serde_json::json!({"type":"message","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"inspect it"}}),
            serde_json::json!({"type":"message","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"read-1","name":"read","arguments":{"path":"a.rs"}}]}}),
            serde_json::json!({"type":"message","timestamp":"2026-01-01T00:00:03Z","message":{"role":"toolResult","toolCallId":"read-1","toolName":"read","content":"original","isError":false}}),
        ] {
            writeln!(file, "{entry}").unwrap();
        }
        file.flush().unwrap();
        let session = super::super::SessionReader::read_file(&path).unwrap();
        let history = model_history(&session);
        assert_eq!(history.len(), 3);
        assert!(
            matches!(&history[1].content, ai::MessageContent::Blocks(blocks)
            if matches!(&blocks[0], ai::ContentBlock::ToolUse { id, name, input , .. }
                if id == "read-1" && name == "read" && input["path"] == "a.rs"))
        );
        assert!(
            matches!(&history[2].content, ai::MessageContent::Blocks(blocks)
            if matches!(&blocks[0], ai::ContentBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "read-1" && content == "original"))
        );
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type":"compaction","timestamp":"2026-01-01T00:00:04Z",
                "summary":"Read a.rs; keep the old API.","firstKeptEntryIndex":2,
                "tokensBefore":100, "auto":true
            })
        )
        .unwrap();
        file.flush().unwrap();
        let compacted = model_history(&super::super::SessionReader::read_file(&path).unwrap());
        assert_eq!(compacted.len(), 1);
        let summary = compacted[0].content.as_text().unwrap();
        assert!(summary.starts_with("<context_summary>"));
        assert!(summary.contains("keep the old API"));
    }
}
