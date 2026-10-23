//! Atomic multipart checkpoints in the existing append-only session journal.
use maestro_ai::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CustomEntry, SessionEntry, writer::SessionWriteError};

const PART_TYPE: &str = "runtime_context_part_v1";
const COMMIT_TYPE: &str = "runtime_context_commit_v1";
// JSON can expand a raw control byte to six bytes. Leave room for that and
// the small envelope, including the terminating newline.
const CHUNK_BYTES: usize = super::fork::MAX_SESSION_LINE_BYTES / 8;
// Bound one in-progress context independently of forged part/commit counts.
const MAX_CONTEXT_BYTES: usize = 256 * 1024 * 1024;

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(format!(
                "runtime context exceeds the {} byte checkpoint limit",
                self.limit
            )));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    checkpoint_id: String,
    index: usize,
    text: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Commit {
    checkpoint_id: String,
    parts: usize,
    bytes: usize,
    sha256: String,
}

fn entry(kind: &str, data: serde_json::Value) -> SessionEntry {
    SessionEntry::Custom(CustomEntry {
        id: Some(uuid::Uuid::new_v4().to_string()),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        custom_type: kind.into(),
        data: Some(data),
    })
}

fn serialize_context(history: &[Message], limit: usize) -> Result<String, SessionWriteError> {
    #[derive(Serialize)]
    struct Context<'a> {
        messages: &'a [Message],
    }
    let mut output = BoundedBytes {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, &Context { messages: history })
        .map_err(|error| SessionWriteError::SerializeError(error.to_string()))?;
    String::from_utf8(output.bytes)
        .map_err(|error| SessionWriteError::SerializeError(error.to_string()))
}

/// Produce bounded lines and a final commit for exact, credential-vaulted context.
/// Callers must stop on the first failed write and flush after the commit.
pub fn runtime_context_entries(
    history: &[Message],
) -> Result<impl Iterator<Item = SessionEntry>, SessionWriteError> {
    if history.is_empty() {
        return Err(SessionWriteError::SerializeError(
            "runtime context is empty".into(),
        ));
    }
    let serialized = serialize_context(history, MAX_CONTEXT_BYTES)?;
    let digest = format!("{:x}", Sha256::digest(serialized.as_bytes()));
    let checkpoint_id = uuid::Uuid::new_v4().to_string();
    let mut offset = 0;
    let mut index = 0;
    let mut committed = false;
    Ok(std::iter::from_fn(move || {
        if offset < serialized.len() {
            let mut end = offset.saturating_add(CHUNK_BYTES).min(serialized.len());
            while !serialized.is_char_boundary(end) {
                end -= 1;
            }
            let part = Part {
                checkpoint_id: checkpoint_id.clone(),
                index,
                text: serialized[offset..end].into(),
            };
            offset = end;
            index += 1;
            Some(entry(
                PART_TYPE,
                serde_json::to_value(part).expect("part fields serialize"),
            ))
        } else if !committed {
            committed = true;
            Some(entry(
                COMMIT_TYPE,
                serde_json::to_value(Commit {
                    checkpoint_id: checkpoint_id.clone(),
                    parts: index,
                    bytes: serialized.len(),
                    sha256: digest.clone(),
                })
                .expect("commit fields serialize"),
            ))
        } else {
            None
        }
    }))
}

#[derive(Default)]
pub(super) struct PendingCheckpoint {
    current: Option<(String, usize, String)>,
}

impl PendingCheckpoint {
    /// Ordinary entries invalidate an interrupted transaction. No declared count
    /// or length controls allocation: memory grows only with validated payload.
    pub(super) fn consume(&mut self, incoming: SessionEntry) -> Option<SessionEntry> {
        self.consume_with_limit(incoming, MAX_CONTEXT_BYTES)
    }

    fn consume_with_limit(&mut self, incoming: SessionEntry, limit: usize) -> Option<SessionEntry> {
        let SessionEntry::Custom(custom) = &incoming else {
            self.current = None;
            return Some(incoming);
        };
        match custom.custom_type.as_str() {
            PART_TYPE => {
                let part = custom
                    .data
                    .clone()
                    .and_then(|data| serde_json::from_value::<Part>(data).ok());
                let Some(part) = part.filter(|part| {
                    uuid::Uuid::parse_str(&part.checkpoint_id).is_ok()
                        && !part.text.is_empty()
                        && part.text.len() <= CHUNK_BYTES.min(limit)
                }) else {
                    self.current = None;
                    return None;
                };
                if part.index == 0 {
                    self.current = Some((part.checkpoint_id, 1, part.text));
                } else if let Some((id, next, text)) = &mut self.current {
                    if *id == part.checkpoint_id
                        && *next == part.index
                        && part.text.len() <= limit.saturating_sub(text.len())
                    {
                        text.push_str(&part.text);
                        *next += 1;
                    } else {
                        self.current = None;
                    }
                }
                None
            }
            COMMIT_TYPE => {
                let pending = self.current.take();
                let commit = custom
                    .data
                    .clone()
                    .and_then(|data| serde_json::from_value::<Commit>(data).ok())?;
                let (id, parts, text) = pending?;
                if id != commit.checkpoint_id
                    || parts != commit.parts
                    || text.len() != commit.bytes
                    || format!("{:x}", Sha256::digest(text.as_bytes())) != commit.sha256
                {
                    return None;
                }
                let data: serde_json::Value = serde_json::from_str(&text).ok()?;
                super::selective_summary::decode_context(Some(data.clone())).ok()?;
                // Reuse the established reducer only after the entire checkpoint
                // is validated. Earlier valid context and turn suffix survive
                // abandoned parts, forged counts, and a torn final commit.
                Some(SessionEntry::Custom(CustomEntry {
                    custom_type: super::selective_summary::RUNTIME_CONTEXT_TYPE.into(),
                    data: Some(data),
                    ..custom.clone()
                }))
            }
            _ => {
                self.current = None;
                Some(incoming)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionReader, SessionWriter, model_history, runtime_context_entry};
    use maestro_ai::{ContentBlock, ContentSource, MessageContent, Role};
    use std::io::Write;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"id\":\"test\",\"timestamp\":\"2026-10-23T00:00:00Z\",\"cwd\":\"/tmp\",\"model\":\"openai/gpt-4o\",\"thinkingLevel\":\"low\"}\n").unwrap();
        (dir, path)
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::text(text),
        }
    }

    #[test]
    fn multipart_context_roundtrips_oversized_single_message_and_typed_hooks() {
        let (_dir, path) = fixture();
        let history = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::attributed_text(
                    "🦀\n\"".repeat(1_500_000),
                    ContentSource::User,
                    None,
                ),
                ContentBlock::attributed_text(
                    "exact hook source",
                    ContentSource::Hook,
                    Some("before-request".into()),
                ),
            ]),
        }];
        assert!(runtime_context_entry(&history).is_err());
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        let mut count = 0;
        for entry in runtime_context_entries(&history).unwrap() {
            assert!(
                serde_json::to_vec(&entry).unwrap().len()
                    < super::super::fork::MAX_SESSION_LINE_BYTES
            );
            writer.write_entry(entry).unwrap();
            count += 1;
        }
        assert!(count > 2);
        writer.flush().unwrap();
        drop(writer);
        let restored = model_history(&SessionReader::read_file(&path).unwrap());
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(history).unwrap()
        );
    }

    #[test]
    fn incomplete_and_invalid_multipart_context_preserves_previous_checkpoint_and_suffix() {
        let (_dir, path) = fixture();
        let previous = vec![user("previous context")];
        let replacement = vec![user("replacement context")];
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        writer
            .write_entry(runtime_context_entry(&previous).unwrap())
            .unwrap();
        writer.write_entry(serde_json::from_value(serde_json::json!({"type":"message","timestamp":"now","message":{"role":"user","content":"new suffix"}})).unwrap()).unwrap();
        let parts = runtime_context_entries(&replacement)
            .unwrap()
            .collect::<Vec<_>>();
        // Complete parts without commit must not hide the ordinary suffix.
        for part in &parts[..parts.len() - 1] {
            writer.write_entry(part.clone()).unwrap();
        }
        writer.flush().unwrap();
        drop(writer);
        let assert_previous = || {
            let restored = model_history(&SessionReader::read_file(&path).unwrap());
            assert_eq!(restored.len(), 2);
            assert_eq!(restored[0].content.as_text(), Some("previous context"));
            assert_eq!(restored[1].content.as_text(), Some("new suffix"));
        };
        assert_previous();
        let mut writer = SessionWriter::open_existing(&path).unwrap();
        let mut bad_commit = parts.last().unwrap().clone();
        if let SessionEntry::Custom(commit) = &mut bad_commit {
            commit.data.as_mut().unwrap()["parts"] = serde_json::json!(usize::MAX);
        }
        writer.write_entry(bad_commit).unwrap();
        // Malformed custom data and out-of-order chunks are ignored safely.
        writer
            .write_entry(entry(PART_TYPE, serde_json::json!({"index":usize::MAX})))
            .unwrap();
        let mut out_of_order = parts[0].clone();
        if let SessionEntry::Custom(part) = &mut out_of_order {
            part.data.as_mut().unwrap()["index"] = serde_json::json!(usize::MAX);
        }
        writer.write_entry(out_of_order).unwrap();
        writer.write_entry(parts.last().unwrap().clone()).unwrap();
        writer.flush().unwrap();
        drop(writer);
        assert_previous();
        // A torn commit also leaves the previous checkpoint and suffix intact.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        for part in &parts[..parts.len() - 1] {
            writeln!(file, "{}", serde_json::to_string(part).unwrap()).unwrap();
        }
        write!(
            file,
            "{{\"type\":\"custom\",\"customType\":\"runtime_context_commit_v1\""
        )
        .unwrap();
        drop(file);
        assert_previous();
    }

    #[test]
    fn multipart_context_bounds_actual_bytes_and_rejects_bad_digest() {
        let history = vec![user("more than a tiny limit")];
        let error = serialize_context(&history, 8).unwrap_err().to_string();
        assert!(error.contains("8 byte checkpoint limit"), "{error}");
        let parts = runtime_context_entries(&history)
            .unwrap()
            .collect::<Vec<_>>();
        let mut pending = PendingCheckpoint::default();
        assert!(pending.consume_with_limit(parts[0].clone(), 8).is_none());
        assert!(pending.current.is_none());
        assert!(pending.consume(parts.last().unwrap().clone()).is_none());
        let id = uuid::Uuid::new_v4().to_string();
        for index in 0..2 {
            pending.consume_with_limit(
                entry(
                    PART_TYPE,
                    serde_json::json!({
                        "checkpoint_id": id, "index": index, "text": "123456"
                    }),
                ),
                10,
            );
        }
        assert!(
            pending.current.is_none(),
            "accumulated payload is bounded too"
        );
        pending.consume(parts[0].clone());
        let suffix: SessionEntry = serde_json::from_value(serde_json::json!({
            "type":"message", "timestamp":"now", "message":{"role":"user", "content":"interleaved suffix"}
        })).unwrap();
        assert!(pending.consume(suffix).is_some());
        assert!(
            pending.consume(parts.last().unwrap().clone()).is_none(),
            "commit cannot discard an interleaved turn"
        );
        pending.consume(parts[0].clone());
        let mut commit = parts.last().unwrap().clone();
        if let SessionEntry::Custom(custom) = &mut commit {
            custom.data.as_mut().unwrap()["sha256"] = "forged".into();
        }
        assert!(pending.consume(commit).is_none());
        assert!(pending.current.is_none());
    }
}
