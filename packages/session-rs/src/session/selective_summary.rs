//! Append-only context checkpoints for selectively summarized session forks.
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::writer::SessionWriteError;
use super::{AppMessage, CustomEntry, SessionEntry, SessionReader, SessionWriter};
use maestro_ai::{ContentBlock, ImageSource, Message, MessageContent, Role};
use maestro_context::extract_context_summary;
use maestro_runtime_contracts::TokenUsage;

pub(super) const CONTEXT_TYPE: &str = "selective_summary_context_v1";
pub(super) const USAGE_TYPE: &str = "selective_summary_usage_v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextCheckpoint {
    pub messages: Vec<Message>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SummaryUsage {
    pub model: String,
    pub usage: TokenUsage,
}

fn custom_entry(kind: &str, data: serde_json::Value) -> SessionEntry {
    SessionEntry::Custom(CustomEntry {
        id: Some(uuid::Uuid::new_v4().to_string()),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        custom_type: kind.into(),
        data: Some(data),
    })
}

/// Save the complete rewritten provider context on an existing fork, then flush.
/// The source session is never opened for writing. No image URL or credential is resolved.
pub fn append_selective_summary_checkpoint(
    path: impl AsRef<Path>,
    history: &[Message],
) -> Result<(), SessionWriteError> {
    if history.is_empty() {
        return Err(SessionWriteError::SerializeError(
            "selective summary context is empty".into(),
        ));
    }
    let data = serde_json::to_value(ContextCheckpoint {
        messages: history.to_vec(),
    })
    .map_err(|e| SessionWriteError::SerializeError(e.to_string()))?;
    let entry = custom_entry(CONTEXT_TYPE, data);
    let line =
        serde_json::to_vec(&entry).map_err(|e| SessionWriteError::SerializeError(e.to_string()))?;
    if line.len().saturating_add(1) > super::fork::MAX_SESSION_LINE_BYTES {
        return Err(SessionWriteError::SerializeError(
            "selective summary checkpoint exceeds the session line limit; summarize a larger range"
                .into(),
        ));
    }
    let mut writer = SessionWriter::open_existing(path.as_ref())?;
    let session = SessionReader::read_file(path.as_ref())
        .map_err(|e| SessionWriteError::SerializeError(e.to_string()))?;
    if session.header.parent_session.is_none() {
        return Err(SessionWriteError::SerializeError(
            "selective summary requires a forked session".into(),
        ));
    }
    writer.write_entry(entry)?;
    writer.flush()?;
    std::fs::File::open(path.as_ref())?.sync_all()?;
    Ok(())
}

/// Record measured auxiliary model usage without inventing a transcript message.
pub fn selective_summary_usage_entry(
    model: &str,
    usage: &TokenUsage,
) -> Result<SessionEntry, SessionWriteError> {
    if model.trim().is_empty()
        || usage
            .cost
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
    {
        return Err(SessionWriteError::SerializeError(
            "invalid selective summary usage".into(),
        ));
    }
    let data = serde_json::to_value(SummaryUsage {
        model: model.into(),
        usage: usage.clone(),
    })
    .map_err(|e| SessionWriteError::SerializeError(e.to_string()))?;
    Ok(custom_entry(USAGE_TYPE, data))
}

pub(super) fn decode_context(data: Option<serde_json::Value>) -> Result<ContextCheckpoint, String> {
    let data = data.ok_or("missing selective summary context")?;
    let checkpoint: ContextCheckpoint =
        serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
    if checkpoint.messages.is_empty() {
        return Err("selective summary context is empty".into());
    }
    // Reject unrecognized nested message/block fields as well as malformed known fields.
    if serde_json::to_value(&checkpoint).map_err(|e| e.to_string())? != data {
        return Err("noncanonical selective summary context".into());
    }
    Ok(checkpoint)
}

pub(super) fn decode_usage(data: Option<serde_json::Value>) -> Result<SummaryUsage, String> {
    let data = data.ok_or("missing selective summary usage")?;
    let usage: SummaryUsage = serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
    if usage.model.trim().is_empty()
        || usage
            .usage
            .cost
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
        || serde_json::to_value(&usage).map_err(|e| e.to_string())? != data
    {
        return Err("invalid selective summary usage".into());
    }
    Ok(usage)
}

pub(super) fn context_message_visible(message: &Message) -> bool {
    !(message.role == Role::User
        && matches!(&message.content, MessageContent::Blocks(blocks)
            if !blocks.is_empty()
                && blocks.iter().all(|block| matches!(block, ContentBlock::ToolResult { .. }))))
}

/// A display projection only. Exact provider blocks remain in the checkpoint.
pub(super) fn display_messages(history: &[Message]) -> Vec<AppMessage> {
    let mut result = Vec::new();
    for message in history {
        let summary = match (&message.role, &message.content) {
            (Role::User, MessageContent::Text(text)) => extract_context_summary(text),
            _ => None,
        };
        if let Some(summary) = summary {
            let mut display = vec![super::ContentBlock::Text {
                text: format!("Conversation summary\n\n{summary}"),
            }];
            flush_display(&mut result, Role::Assistant, &mut display);
            continue;
        }
        let blocks = match &message.content {
            MessageContent::Text(text) => vec![ContentBlock::Text { text: text.clone() }],
            MessageContent::Blocks(blocks) => blocks.clone(),
        };
        let mut display = Vec::new();
        let mut tool_results = Vec::new();
        for block in blocks {
            let block = match block {
                ContentBlock::Text { text } => super::ContentBlock::Text { text },
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => super::ContentBlock::Thinking {
                    text: thinking,
                    signature,
                },
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    gemini_context,
                } => super::ContentBlock::ToolCall {
                    id,
                    name,
                    args: input,
                    contract: None,
                    gemini_context,
                },
                ContentBlock::Image {
                    source:
                        ImageSource::Base64 {
                            media_type,
                            data,
                            owner,
                        },
                } => super::ContentBlock::Image {
                    source: Some(super::ImageSource {
                        source_type: "base64".into(),
                        media_type,
                        data,
                        owner,
                    }),
                    data: None,
                    mime_type: None,
                },
                ContentBlock::Image {
                    source: ImageSource::Url { url },
                } => super::ContentBlock::Text {
                    text: format!("[Image: {url}]"),
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    tool_results.push(AppMessage::ToolResult {
                        tool_call_id: tool_use_id,
                        tool_name: String::new(),
                        content,
                        details: None,
                        receipt: None,
                        is_error: is_error.unwrap_or(false),
                        timestamp: 0,
                    });
                    continue;
                }
            };
            display.push(block);
        }
        // One visible row per provider message, even when tool results split its text.
        // Tool-result rows do not count toward the automatic-compaction boundary.
        if context_message_visible(message) {
            flush_display(&mut result, message.role, &mut display);
        }
        result.extend(tool_results);
    }
    result
}

/// Match retained content to the original transcript in order, including repeated
/// messages. A newly generated summary gets its checkpoint time.
pub(super) fn restore_display_timestamps(
    projected: &mut [AppMessage],
    original: &[AppMessage],
    history: &[Message],
    checkpoint_time: u64,
) -> Result<(), serde_json::Error> {
    fn display_blocks(blocks: &[super::ContentBlock]) -> Vec<super::ContentBlock> {
        blocks
            .iter()
            .cloned()
            .map(|mut block| {
                // A display key excludes dispatch metadata that is absent from the
                // provider checkpoint. The original transcript remains untouched.
                if let super::ContentBlock::ToolCall {
                    contract,
                    gemini_context,
                    ..
                } = &mut block
                {
                    *contract = None;
                    *gemini_context = None;
                }
                block
            })
            .collect()
    }
    fn key(message: &AppMessage) -> Result<String, serde_json::Error> {
        let value = match message {
            AppMessage::User { content, .. } => {
                let blocks = match content {
                    super::MessageContent::Text(text) => {
                        vec![super::ContentBlock::Text { text: text.clone() }]
                    }
                    super::MessageContent::Blocks(blocks) => display_blocks(blocks),
                };
                serde_json::json!(["user", blocks])
            }
            AppMessage::Assistant { content, .. } => {
                serde_json::json!(["assistant", display_blocks(content)])
            }
            AppMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => serde_json::json!(["tool", tool_call_id, content, is_error]),
        };
        serde_json::to_string(&value)
    }
    let keys = original.iter().map(key).collect::<Result<Vec<_>, _>>()?;
    // Selective summaries replace one contiguous range. Match the retained
    // prefix from the start and suffix from the end so repeated text in the
    // removed range cannot steal a retained turn's timestamp.
    let summary_row = history
        .iter()
        .rposition(|message| {
            message.role == Role::User
                && matches!(&message.content,
            MessageContent::Text(text) if extract_context_summary(text).is_some())
        })
        .map(|index| display_messages(&history[..index]).len())
        .unwrap_or(projected.len());
    let mut begin = 0;
    let mut end = original.len();
    for message in projected.iter_mut().take(summary_row) {
        let wanted = key(message)?;
        let found = keys[begin..end]
            .iter()
            .position(|candidate| candidate == &wanted)
            .map(|offset| begin + offset);
        let time = found.map_or(checkpoint_time, |index| {
            begin = index + 1;
            original[index].timestamp()
        });
        set_display_timestamp(message, time);
    }
    for (index, message) in projected.iter_mut().enumerate().rev() {
        if index <= summary_row {
            break;
        }
        let wanted = key(message)?;
        let found = keys[begin..end]
            .iter()
            .rposition(|candidate| candidate == &wanted)
            .map(|offset| begin + offset);
        let time = found.map_or(checkpoint_time, |index| {
            end = index;
            original[index].timestamp()
        });
        set_display_timestamp(message, time);
    }
    if let Some(message) = projected.get_mut(summary_row) {
        set_display_timestamp(message, checkpoint_time);
    }
    Ok(())
}

fn set_display_timestamp(message: &mut AppMessage, time: u64) {
    match message {
        AppMessage::User { timestamp, .. }
        | AppMessage::Assistant { timestamp, .. }
        | AppMessage::ToolResult { timestamp, .. } => *timestamp = time,
    }
}

fn flush_display(result: &mut Vec<AppMessage>, role: Role, blocks: &mut Vec<super::ContentBlock>) {
    let content = std::mem::take(blocks);
    result.push(if role == Role::Assistant {
        AppMessage::Assistant {
            content,
            api: None,
            provider: None,
            model: None,
            usage: None,
            stop_reason: None,
            timestamp: 0,
        }
    } else {
        AppMessage::User {
            content: super::MessageContent::Blocks(content),
            attachments: None,
            timestamp: 0,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_context::render_context_summary;
    use maestro_runtime_contracts::ToolCallContract;
    use std::io::Write;

    #[test]
    fn script_image_checkpoint_retains_projection_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let fork = super::super::fork_session_file(&path).unwrap();
        let history: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user", "content":[{"type":"image", "source":{"type":"base64","media_type":"image/png","data":"pixel","owner":{"call_id":"script-owner","index":0}}}]}
        ])).unwrap();
        append_selective_summary_checkpoint(&fork.path, &history).unwrap();
        let restored = SessionReader::read_file(&fork.path).unwrap();
        assert_eq!(
            serde_json::to_value(super::super::model_history(&restored)).unwrap(),
            serde_json::to_value(&history).unwrap()
        );
    }

    #[test]
    fn selective_summary_oversized_checkpoint_leaves_fork_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let fork = super::super::fork_session_file(&path).unwrap();
        let before = std::fs::read(&fork.path).unwrap();
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::text("x".repeat(super::super::fork::MAX_SESSION_LINE_BYTES)),
        }];
        let error = append_selective_summary_checkpoint(&fork.path, &history).unwrap_err();
        assert!(error.to_string().contains("session line limit"));
        assert_eq!(std::fs::read(&fork.path).unwrap(), before);
        super::super::fork_session_file(&fork.path).unwrap();
    }

    #[test]
    fn selective_summary_retained_timestamps_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        for time in [500_u64, 1000, 2000] {
            writer.write_entry(serde_json::from_value(serde_json::json!({
                "type":"message", "timestamp":"2026-01-01T00:00:04Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"retained"}],"timestamp":time}
            })).unwrap()).unwrap();
        }
        writer.flush().unwrap();
        drop(writer);
        let fork = super::super::fork_session_file(&path).unwrap();
        let history: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user", "content": render_context_summary("summary")},
            {"role":"assistant", "content":"retained"},
            {"role":"assistant", "content":"retained"}
        ]))
        .unwrap();
        append_selective_summary_checkpoint(&fork.path, &history).unwrap();
        for _ in 0..2 {
            let session = SessionReader::read_file(&fork.path).unwrap();
            assert!(session.messages[0].timestamp() > 2000);
            assert_eq!(session.messages[1].timestamp(), 1000);
            assert_eq!(session.messages[2].timestamp(), 2000);
            assert_eq!(
                serde_json::to_value(super::super::model_history(&session)).unwrap(),
                serde_json::to_value(&history).unwrap()
            );
        }
    }

    #[test]
    fn selective_summary_tool_timestamps_ignore_only_display_absent_contract_metadata() {
        let history: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user", "content": render_context_summary("summary")},
            {"role":"assistant", "content":[{"type":"tool_use","id":"read-1","name":"read","input":{"path":"file"}}]},
            {"role":"user", "content":[{"type":"tool_result","tool_use_id":"read-1","content":"result","is_error":false}]}
        ])).unwrap();
        let mut original = display_messages(&history[1..]);
        for message in &mut original {
            set_display_timestamp(message, 1234);
        }
        let AppMessage::Assistant { content, .. } = &mut original[0] else {
            panic!("assistant")
        };
        let super::super::ContentBlock::ToolCall { contract, .. } = &mut content[0] else {
            panic!("tool")
        };
        *contract = Some(ToolCallContract::record("read-1", "read", None));
        let before = serde_json::to_value(&original).unwrap();
        let mut projected = display_messages(&history);
        restore_display_timestamps(&mut projected, &original, &history, 9999).unwrap();
        assert_eq!(
            projected
                .iter()
                .map(AppMessage::timestamp)
                .collect::<Vec<_>>(),
            vec![9999, 1234, 1234]
        );
        assert_eq!(serde_json::to_value(&original).unwrap(), before);
    }

    fn source(path: &Path) {
        let mut file = std::fs::File::create(path).unwrap();
        for entry in [
            serde_json::json!({"type":"session","id":"source","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp","model":"openai/gpt-4o"}),
            serde_json::json!({"type":"message","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"old conversation"}}),
            serde_json::json!({"type":"compaction","timestamp":"2026-01-01T00:00:02Z","summary":"old summary","firstKeptEntryIndex":1,"tokensBefore":100,"auto":true}),
            serde_json::json!({"type":"custom","timestamp":"2026-01-01T00:00:03Z","id":"old-note","customType":"subagent_lifecycle_applied","data":{"content":"old lifecycle","agentNote":"do not replay"}}),
        ] {
            writeln!(file, "{entry}").unwrap();
        }
    }

    fn checkpoint_history() -> Vec<Message> {
        serde_json::from_value(serde_json::json!([
            {"role":"system","content":"governed context"},
            {"role":"user","content":"selected summary"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"private reasoning","signature":"signature"},
                {"type":"tool_use","id":"read-1","name":"read","input":{"credential":"handle://opaque"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"read-1","content":"kept output","is_error":false},
                {"type":"image","source":{"type":"url","url":"https://example.invalid/image.png"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}
            ]}
        ])).unwrap()
    }

    #[test]
    fn selective_summary_checkpoint_roundtrip_future_append_and_original_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("source.jsonl");
        source(&original);
        let bytes = std::fs::read(&original).unwrap();
        let fork = super::super::fork_session_file(&original).unwrap();
        let history = checkpoint_history();
        append_selective_summary_checkpoint(&fork.path, &history).unwrap();
        let restored = SessionReader::read_file(&fork.path).unwrap();
        assert_eq!(
            serde_json::to_value(super::super::model_history(&restored)).unwrap(),
            serde_json::to_value(&history).unwrap()
        );
        assert!(restored.compactions.is_empty());
        assert!(restored.pending_lifecycle_agent_notes.is_empty());
        assert!(restored.lifecycle_notifications.is_empty());
        assert!(
            !restored
                .messages
                .iter()
                .any(|m| m.text_content().contains("old conversation"))
        );
        assert_eq!(std::fs::read(&original).unwrap(), bytes);
        let mut writer = SessionWriter::open_existing(&fork.path).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"message","timestamp":"2026-01-01T00:00:05Z","message":{"role":"user","content":"future prompt"}})).unwrap()).unwrap();
        writer.flush().unwrap();
        let restored = SessionReader::read_file(&fork.path).unwrap();
        let resumed = super::super::model_history(&restored);
        assert_eq!(resumed.len(), history.len() + 1);
        assert_eq!(
            serde_json::to_value(&resumed[..history.len()]).unwrap(),
            serde_json::to_value(&history).unwrap()
        );
        assert_eq!(
            resumed.last().unwrap().content.as_text(),
            Some("future prompt")
        );
        assert_eq!(std::fs::read(&original).unwrap(), bytes);
    }

    #[test]
    fn selective_summary_checkpoint_rejects_invalid_data_and_nonfork_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let bytes = std::fs::read(&path).unwrap();
        assert!(append_selective_summary_checkpoint(&path, &checkpoint_history()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        for data in [
            serde_json::Value::Null,
            serde_json::json!({"messages":[]}),
            serde_json::json!({"messages":[{"role":"bogus","content":"bad"}]}),
            serde_json::json!({"messages":[{"role":"user","content":"x","unknown":true}]}),
        ] {
            source(&path);
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::to_string(&custom_entry(CONTEXT_TYPE, data)).unwrap()
            )
            .unwrap();
            assert!(SessionReader::read_file(&path).is_err());
        }
    }

    #[test]
    fn selective_summary_usage_persists_without_transcript_or_fabricated_cost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        for cost in [None, Some(0.25)] {
            writer
                .write_entry(
                    selective_summary_usage_entry(
                        "openai/gpt-4o",
                        &TokenUsage {
                            input_tokens: 12,
                            output_tokens: 3,
                            cache_read_tokens: 4,
                            cache_write_tokens: 5,
                            cost,
                        },
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        writer.flush().unwrap();
        let restored = SessionReader::read_file(&path).unwrap();
        assert_eq!(restored.messages.len(), 1);
        assert_eq!(restored.usage_entries.len(), 2);
        assert!(restored.usage_entries[0].usage.cost.is_none());
        assert!((restored.usage_entries[1].usage.total_cost() - 0.25).abs() < f64::EPSILON);
        assert_eq!(restored.usage_entries[0].usage.cache_read, 4);
        assert_eq!(restored.stats.total_input_tokens, 24);
        assert_eq!(restored.stats.total_output_tokens, 6);
        assert!((restored.stats.total_cost - 0.25).abs() < f64::EPSILON);
    }
    #[test]
    fn selective_summary_checkpoint_followed_by_auto_compaction_preserves_provider_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let fork = super::super::fork_session_file(&path).unwrap();
        let history: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user","content":"selected summary"},
            {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"read","input":{}}]},
            {"role":"user","content":[
                {"type":"text","text":"before result"},
                {"type":"tool_result","tool_use_id":"call","content":"output"},
                {"type":"text","text":"after result"}
            ]},
            {"role":"user","content":[]},
            {"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example.invalid/retained.png"}}]},
            {"role":"assistant","content":"retained answer"}
        ])).unwrap();
        append_selective_summary_checkpoint(&fork.path, &history).unwrap();
        let checkpoint = SessionReader::read_file(&fork.path).unwrap();
        assert_eq!(
            checkpoint
                .messages
                .iter()
                .filter(|message| !matches!(message, AppMessage::ToolResult { .. }))
                .count(),
            6
        );
        let mut writer = SessionWriter::open_existing(&fork.path).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({
            "type":"compaction","timestamp":"2026-01-01T00:00:07Z","summary":"new automatic summary",
            "firstKeptEntryIndex":4,"tokensBefore":200,"auto":true
        })).unwrap()).unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({
            "type":"message","timestamp":"2026-01-01T00:00:08Z","message":{"role":"user","content":"future question"}
        })).unwrap()).unwrap();
        writer.flush().unwrap();
        let restored = SessionReader::read_file(&fork.path).unwrap();
        assert_eq!(restored.compactions.len(), 1);
        let resumed = super::super::model_history(&restored);
        assert_eq!(resumed.len(), 4);
        assert!(
            resumed[0]
                .content
                .as_text()
                .unwrap()
                .contains("new automatic summary")
        );
        assert_eq!(
            serde_json::to_value(&resumed[1..3]).unwrap(),
            serde_json::to_value(&history[4..]).unwrap()
        );
        assert_eq!(resumed[3].content.as_text(), Some("future question"));
    }

    #[test]
    fn selective_summary_repeated_checkpoint_supersedes_compaction_and_retains_new_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let fork = super::super::fork_session_file(&path).unwrap();
        append_selective_summary_checkpoint(&fork.path, &checkpoint_history()).unwrap();
        {
            let mut writer = SessionWriter::open_existing(&fork.path).unwrap();
            writer.write_entry(serde_json::from_value(serde_json::json!({
                "type":"compaction","timestamp":"2026-01-01T00:00:07Z","summary":"intermediate summary",
                "firstKeptEntryIndex":2,"tokensBefore":200,"auto":true
            })).unwrap()).unwrap();
            writer.write_entry(serde_json::from_value(serde_json::json!({
                "type":"message","timestamp":"2026-01-01T00:00:08Z","message":{"role":"user","content":"later retained turn"}
            })).unwrap()).unwrap();
            writer.flush().unwrap();
        }
        let second: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user","content":"replacement selected summary"},
            {"role":"user","content":"later retained turn"}
        ]))
        .unwrap();
        // Another session test may fork a child while its writer lock is
        // being released. The child inherits the lock fd until exec, so the
        // next open can observe a short-lived `Locked` result even though no
        // live writer owns this session. Retry only in this concurrent test,
        // keeping production writers fail-fast for genuine contention.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match append_selective_summary_checkpoint(&fork.path, &second) {
                Err(SessionWriteError::Locked(_)) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                result => {
                    result.unwrap();
                    break;
                }
            }
        }
        let mut writer = super::super::writer::open_existing_after_release(&fork.path);
        writer.write_entry(serde_json::from_value(serde_json::json!({
            "type":"message","timestamp":"2026-01-01T00:00:09Z","message":{"role":"user","content":"new final question"}
        })).unwrap()).unwrap();
        writer.flush().unwrap();
        let restored = SessionReader::read_file(&fork.path).unwrap();
        assert!(restored.compactions.is_empty());
        let resumed = super::super::model_history(&restored);
        assert_eq!(resumed.len(), 3);
        assert_eq!(
            serde_json::to_value(&resumed[..2]).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert_eq!(resumed[2].content.as_text(), Some("new final question"));
        assert_eq!(restored.messages.len(), 3);
    }
    #[test]
    fn selective_summary_display_hides_exact_envelope_without_changing_provider_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        source(&path);
        let fork = super::super::fork_session_file(&path).unwrap();
        let summary = "Kept the existing API.\nNext: verify the caller.";
        let framed = render_context_summary(summary);
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::Text(framed.clone()),
        }];
        append_selective_summary_checkpoint(&fork.path, &history).unwrap();
        let restored = SessionReader::read_file(&fork.path).unwrap();
        assert!(matches!(restored.messages[0], AppMessage::Assistant { .. }));
        assert_eq!(
            restored.messages[0].text_content(),
            format!("Conversation summary\n\n{summary}")
        );
        assert!(
            !restored.messages[0]
                .text_content()
                .contains("<context_summary>")
        );
        assert!(
            !restored.messages[0]
                .text_content()
                .contains("machine-generated summary")
        );
        assert_eq!(
            super::super::model_history(&restored)[0].content.as_text(),
            Some(framed.as_str())
        );
        for lookalike in [
            "<context_summary>\nuser text\n</context_summary>".to_owned(),
            format!("user prefix {framed}"),
            format!("{framed} user suffix"),
            framed.replace("machine-generated summary", "my own summary"),
            framed.trim_end_matches('.').to_owned(),
        ] {
            assert!(extract_context_summary(&lookalike).is_none());
            let projected = display_messages(&[Message {
                role: Role::User,
                content: MessageContent::Text(lookalike.clone()),
            }]);
            assert!(matches!(projected[0], AppMessage::User { .. }));
            assert_eq!(projected[0].text_content(), lookalike);
        }
    }
}
