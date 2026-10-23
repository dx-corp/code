//! Append-only updates to existing session checkpoints.
use super::writer::SessionWriteError;
use super::{CustomEntry, SessionEntry};
use serde::{Deserialize, Serialize};

pub(super) const SYSTEM_PARTS_TYPE: &str = "system_prompt_parts_v1";

#[derive(Serialize, Deserialize)]
pub(super) struct SystemPromptParts {
    pub version: u32,
    pub system_prompt_parts: Vec<maestro_ai::ContentBlock>,
}

fn entry(kind: &str, data: impl Serialize) -> Result<SessionEntry, SessionWriteError> {
    Ok(SessionEntry::Custom(CustomEntry {
        id: Some(uuid::Uuid::new_v4().to_string()),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        custom_type: kind.into(),
        data: Some(
            serde_json::to_value(data)
                .map_err(|error| SessionWriteError::SerializeError(error.to_string()))?,
        ),
    }))
}

/// Record descriptive request provenance. No replay path installs this as policy.
pub fn system_prompt_parts_entry(
    parts: &[maestro_ai::ContentBlock],
) -> Result<SessionEntry, SessionWriteError> {
    validate_parts(parts).map_err(SessionWriteError::SerializeError)?;
    entry(
        SYSTEM_PARTS_TYPE,
        SystemPromptParts {
            version: 1,
            system_prompt_parts: parts.to_vec(),
        },
    )
}

fn validate_parts(parts: &[maestro_ai::ContentBlock]) -> Result<(), String> {
    if parts.iter().any(|part| {
        !matches!(
            part,
            maestro_ai::ContentBlock::Text { .. } | maestro_ai::ContentBlock::AttributedText { .. }
        )
    }) {
        return Err("System prompt provenance must contain only text parts".into());
    }
    Ok(())
}

pub(super) fn decode_parts(
    data: Option<serde_json::Value>,
) -> Result<Vec<maestro_ai::ContentBlock>, String> {
    let checkpoint: SystemPromptParts =
        serde_json::from_value(data.ok_or("Missing system prompt provenance")?)
            .map_err(|error| format!("Invalid system prompt provenance: {error}"))?;
    if checkpoint.version != 1 {
        return Err("Unsupported system prompt provenance version".into());
    }
    validate_parts(&checkpoint.system_prompt_parts)?;
    Ok(checkpoint.system_prompt_parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionReader, SessionWriter};

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.jsonl");
        std::fs::write(&path, concat!(
            "{\"type\":\"session\",\"id\":\"checkpoint\",\"timestamp\":\"2026-01-01T00:00:00Z\",",
            "\"cwd\":\"/tmp\",\"model\":\"openai/gpt-5.5\"}\n"
        )).unwrap();
        (dir, path)
    }

    #[test]
    fn typed_request_provenance_roundtrips_without_becoming_model_history() {
        let (_dir, path) = fixture();
        let parts = vec![
            maestro_ai::ContentBlock::attributed_text(
                "user preference",
                maestro_ai::ContentSource::User,
                None,
            ),
            maestro_ai::ContentBlock::attributed_text(
                "repository guidance",
                maestro_ai::ContentSource::Repository,
                Some("AGENTS.md".into()),
            ),
            maestro_ai::ContentBlock::attributed_text(
                "hook observation",
                maestro_ai::ContentSource::Hook,
                Some("before-request".into()),
            ),
        ];
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        writer
            .write_entry(system_prompt_parts_entry(&parts).unwrap())
            .unwrap();
        writer.flush().unwrap();
        let saved = SessionReader::read_file(&path).unwrap();
        assert_eq!(
            serde_json::to_value(saved.system_prompt_parts).unwrap(),
            serde_json::to_value(parts).unwrap()
        );
        assert!(saved.messages.is_empty());
        assert!(saved.header.system_prompt.is_none());
        assert!(
            crate::session::model_history(&SessionReader::read_file(&path).unwrap()).is_empty()
        );
    }

    #[test]
    fn runtime_context_roundtrips_typed_history_and_preserves_new_suffix() {
        let (_dir, path) = fixture();
        use maestro_ai::{ContentBlock, ContentSource, Message, MessageContent, Role};
        let history = vec![
            Message {
                role: Role::User,
                content: MessageContent::attributed_text(
                    maestro_context::render_context_summary("semantic summary"),
                    ContentSource::Harness,
                    Some("compaction".into()),
                ),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::attributed_text("user request", ContentSource::User, None),
                    ContentBlock::attributed_text(
                        "repository guidance",
                        ContentSource::Repository,
                        Some("AGENTS.md".into()),
                    ),
                    ContentBlock::attributed_text(
                        "hook observation",
                        ContentSource::Hook,
                        Some("before-request".into()),
                    ),
                ]),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "read-1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"a.rs"}),
                    gemini_context: None,
                }]),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "read-1".into(),
                        content: "full output retained".into(),
                        is_error: Some(false),
                    },
                    ContentBlock::attributed_text(
                        "tool hook",
                        ContentSource::Hook,
                        Some("after-tool".into()),
                    ),
                ]),
            },
        ];
        assert!(!crate::session::selective_summary::context_message_visible(
            &history[3]
        ));
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"compaction","timestamp":"now","summary":"fast summary","firstKeptEntryIndex":2,"tokensBefore":1000,"auto":true,"continuation":{"source_hash":"source-1"}})).unwrap()).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"message","timestamp":"now","message":{"role":"toolResult","toolCallId":"read-1","toolName":"read","content":"full output retained","details":{"image_preview":{"path":"typed.png"}},"isError":false}})).unwrap()).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"custom","id":"pending-note","timestamp":"now","customType":"subagent_lifecycle_applied","data":{"content":"child complete","agentNote":"deliver to runner"}})).unwrap()).unwrap();
        writer
            .write_entry(crate::session::runtime_context_entry(&history).unwrap())
            .unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"message","timestamp":"now","message":{"role":"user","content":"newer request"}})).unwrap()).unwrap();
        writer.flush().unwrap();
        drop(writer);
        let saved = SessionReader::read_file(&path).unwrap();
        assert_eq!(
            saved.compactions[0]
                .continuation
                .as_ref()
                .unwrap()
                .source_hash,
            "source-1"
        );
        assert!(saved.messages.iter().any(|message| matches!(message, crate::session::AppMessage::ToolResult {details: Some(details),..} if details["image_preview"]["path"] == "typed.png")));
        assert_eq!(saved.pending_lifecycle_agent_notes.len(), 1);
        assert_eq!(
            saved.pending_lifecycle_agent_notes[0].content,
            "deliver to runner"
        );
        assert_eq!(saved.lifecycle_notifications.len(), 1);
        let restored = crate::session::model_history(&saved);
        assert_eq!(restored.len(), history.len() + 1);
        assert_eq!(
            serde_json::to_value(&restored[..history.len()]).unwrap(),
            serde_json::to_value(history).unwrap()
        );
        assert_eq!(
            restored.last().unwrap().content.as_text(),
            Some("newer request")
        );
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"compaction","timestamp":"later","summary":"next summary","firstKeptEntryIndex":2,"tokensBefore":1000,"auto":true})).unwrap()).unwrap();
        writer.flush().unwrap();
        drop(writer);
        let compacted = SessionReader::read_file(&path).unwrap();
        let restored = crate::session::model_history(&compacted);
        assert_eq!(
            restored.len(),
            2,
            "tool result plus hook must not shift the next compaction boundary"
        );
        assert_eq!(restored[1].content.as_text(), Some("newer request"));
    }
}
