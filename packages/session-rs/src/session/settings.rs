//! Session-owned preferences. These are requests, never grants of authority.
use super::{CustomEntry, SessionEntry, ThinkingLevel};
use serde::{Deserialize, Serialize};

pub const SESSION_SETTINGS_CUSTOM_TYPE: &str = "session_settings_v1";

/// The approval preference is revalidated against the current host on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionApprovalMode {
    Yolo,
    Selective,
    Safe,
}

/// Whether follow-up or steering input may queue while a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionQueueMode {
    All,
    One,
}

/// Versioned choices owned by a conversation, independent of global defaults.
/// Credentials, sandbox grants, and managed policy deliberately stay with the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSettings {
    pub version: u32,
    pub model: String,
    pub thinking_level: ThinkingLevel,
    pub approval_mode: SessionApprovalMode,
    pub steering_mode: SessionQueueMode,
    pub follow_up_mode: SessionQueueMode,
}

impl SessionSettings {
    pub fn into_entry(self) -> Result<SessionEntry, serde_json::Error> {
        Ok(SessionEntry::Custom(CustomEntry {
            id: Some(uuid::Uuid::new_v4().to_string()),
            parent_id: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            custom_type: SESSION_SETTINGS_CUSTOM_TYPE.into(),
            data: Some(serde_json::to_value(self)?),
        }))
    }
}

pub(super) fn decode_settings(data: Option<serde_json::Value>) -> Result<SessionSettings, String> {
    let settings: SessionSettings = serde_json::from_value(
        data.ok_or_else(|| "Session settings checkpoint is missing data".to_owned())?,
    )
    .map_err(|error| format!("Invalid session settings checkpoint: {error}"))?;
    if settings.version != 1 || settings.model.trim().is_empty() {
        return Err("Unsupported session settings version or empty model".into());
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionReader;
    use std::io::Write;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"id\":\"settings\",\"timestamp\":\"2026-01-01T00:00:00Z\",",
                "\"cwd\":\"/tmp\",\"model\":\"original\",\"thinking_level\":\"low\"}\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn settings() -> SessionSettings {
        SessionSettings {
            version: 1,
            model: "checkpoint-model".into(),
            thinking_level: ThinkingLevel::High,
            approval_mode: SessionApprovalMode::Safe,
            steering_mode: SessionQueueMode::One,
            follow_up_mode: SessionQueueMode::All,
        }
    }

    fn append(path: &std::path::Path, entry: serde_json::Value) {
        writeln!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{entry}"
        )
        .unwrap();
    }

    #[test]
    fn checkpoint_and_legacy_changes_obey_file_order() {
        let (_dir, path) = fixture();
        append(
            &path,
            serde_json::json!({"type":"model_change", "timestamp":"now", "model":"before-checkpoint"}),
        );
        append(
            &path,
            serde_json::to_value(settings().into_entry().unwrap()).unwrap(),
        );
        let saved = SessionReader::read_file(&path).unwrap();
        assert_eq!(saved.settings, Some(settings()));
        assert_eq!(saved.header.model, "checkpoint-model");
        assert_eq!(saved.header.thinking_level, ThinkingLevel::High);
        assert_eq!(
            SessionReader::read_header(&path).unwrap().0.model,
            "checkpoint-model"
        );
        append(
            &path,
            serde_json::json!({"type":"model_change", "timestamp":"now", "model":"after-checkpoint"}),
        );
        append(
            &path,
            serde_json::json!({"type":"thinking_level_change", "timestamp":"now", "thinking_level":"medium"}),
        );
        let saved = SessionReader::read_file(&path).unwrap();
        assert_eq!(saved.header.model, "after-checkpoint");
        assert_eq!(saved.header.thinking_level, ThinkingLevel::Medium);
        let preferences = saved.settings.unwrap();
        assert_eq!(preferences.model, "after-checkpoint");
        assert_eq!(preferences.thinking_level, ThinkingLevel::Medium);
        assert_eq!(preferences.approval_mode, SessionApprovalMode::Safe);
        let header = SessionReader::read_header(&path).unwrap().0;
        assert_eq!(header.model, "after-checkpoint");
        assert_eq!(header.thinking_level, ThinkingLevel::Medium);
    }

    #[test]
    fn legacy_sessions_have_no_permission_grant() {
        let (_dir, path) = fixture();
        let saved = SessionReader::read_file(&path).unwrap();
        assert!(saved.settings.is_none());
        assert_eq!(saved.header.model, "original");
    }

    #[test]
    fn invalid_recognized_checkpoint_fails_closed() {
        let (_dir, path) = fixture();
        let mut checkpoint = settings();
        checkpoint.version = 2;
        append(
            &path,
            serde_json::to_value(checkpoint.into_entry().unwrap()).unwrap(),
        );
        assert!(SessionReader::read_file(&path).is_err());
        assert!(SessionReader::read_header(&path).is_err());
    }

    #[test]
    fn fork_preserves_session_settings() {
        let (_dir, path) = fixture();
        append(
            &path,
            serde_json::to_value(settings().into_entry().unwrap()).unwrap(),
        );
        let fork = crate::session::fork_session_file(&path).unwrap();
        assert_eq!(
            SessionReader::read_file(fork.path).unwrap().settings,
            Some(settings())
        );
    }
}
