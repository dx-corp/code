//! Append-only local session persistence for runtime tool-operation staging.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use maestro_runtime_contracts::{
    TOOL_OPERATION_CUSTOM_TYPE, ToolOperationError, ToolOperationLedger, ToolOperationRecord,
};
use serde_json::Value;

use super::{CustomEntry, SessionEntry};

pub fn append_tool_operation(
    path: &Path,
    record: &ToolOperationRecord,
) -> Result<(), ToolOperationJournalError> {
    let data = serde_json::to_value(record)?;
    let entry = SessionEntry::Custom(CustomEntry {
        id: Some(format!(
            "tool-operation:{}:{}",
            record.call_id, record.updated_at_ms
        )),
        parent_id: None,
        timestamp: record.updated_at_ms.to_string(),
        custom_type: TOOL_OPERATION_CUSTOM_TYPE.into(),
        data: Some(data),
    });
    let mut encoded = serde_json::to_vec(&entry)?;
    encoded.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&encoded)?;
    file.sync_data()?;
    Ok(())
}

pub fn load_tool_operation_ledger(
    path: &Path,
) -> Result<ToolOperationLedger, ToolOperationJournalError> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut ledger = ToolOperationLedger::default();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|error| {
            ToolOperationJournalError::MalformedLine {
                line: index + 1,
                reason: error.to_string(),
            }
        })?;
        if value.get("type").and_then(Value::as_str) != Some("custom")
            || value.get("customType").and_then(Value::as_str) != Some(TOOL_OPERATION_CUSTOM_TYPE)
        {
            continue;
        }
        let data =
            value
                .get("data")
                .cloned()
                .ok_or_else(|| ToolOperationJournalError::MalformedLine {
                    line: index + 1,
                    reason: "tool operation entry has no data".into(),
                })?;
        let record: ToolOperationRecord = serde_json::from_value(data).map_err(|error| {
            ToolOperationJournalError::MalformedLine {
                line: index + 1,
                reason: error.to_string(),
            }
        })?;
        ledger.apply(record)?;
    }
    Ok(ledger)
}

#[derive(Debug)]
pub enum ToolOperationJournalError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Operation(ToolOperationError),
    MalformedLine { line: usize, reason: String },
}

impl fmt::Display for ToolOperationJournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "tool operation journal I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "tool operation encoding failed: {error}"),
            Self::Operation(error) => write!(formatter, "tool operation journal rejected: {error}"),
            Self::MalformedLine { line, reason } => {
                write!(
                    formatter,
                    "tool operation journal line {line} is malformed: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ToolOperationJournalError {}

impl From<std::io::Error> for ToolOperationJournalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ToolOperationJournalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<ToolOperationError> for ToolOperationJournalError {
    fn from(error: ToolOperationError) -> Self {
        Self::Operation(error)
    }
}

#[cfg(test)]
mod tests {
    use maestro_runtime_contracts::{ToolOperationOutcome, ToolOperationPhase, ToolReplayPolicy};
    use serde_json::json;

    use super::*;

    fn planned(call_id: &str) -> ToolOperationRecord {
        ToolOperationRecord::planned(
            call_id,
            "read",
            json!({"path":"README.md"}),
            None,
            ToolReplayPolicy::Safe,
            10,
        )
        .unwrap()
    }

    #[test]
    fn append_and_reload_reduce_to_latest_valid_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(&path, "{\"type\":\"future_entry\",\"value\":1}\n").unwrap();
        let planned = planned("call-1");
        append_tool_operation(&path, &planned).unwrap();
        let pending = planned.effect_pending(20).unwrap();
        append_tool_operation(&path, &pending).unwrap();
        let ready = pending
            .outcome_ready(ToolOperationOutcome::new("contents", false, None), 30)
            .unwrap();
        append_tool_operation(&path, &ready).unwrap();

        let ledger = load_tool_operation_ledger(&path).unwrap();
        let restored = ledger.latest("call-1").unwrap();
        assert_eq!(restored.phase, ToolOperationPhase::OutcomeReady);
        assert_eq!(restored.outcome.as_ref().unwrap().content, "contents");
        assert_eq!(ledger.recovery_actions()[0].kind(), "materialize");
    }

    #[test]
    fn reload_fails_closed_on_regression() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let planned = planned("call-1");
        append_tool_operation(&path, &planned).unwrap();
        append_tool_operation(&path, &planned.clone().effect_pending(20).unwrap()).unwrap();
        let mut regressed = planned;
        regressed.updated_at_ms = 21;
        append_tool_operation(&path, &regressed).unwrap();

        assert!(matches!(
            load_tool_operation_ledger(&path),
            Err(ToolOperationJournalError::Operation(
                ToolOperationError::PhaseRegression { .. }
            ))
        ));
    }

    #[test]
    fn reload_fails_closed_on_malformed_operation_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"custom\",\"customType\":\"{TOOL_OPERATION_CUSTOM_TYPE}\",\"data\":{{\"callId\":\"missing-fields\"}}}}\n"
            ),
        )
        .unwrap();

        assert!(matches!(
            load_tool_operation_ledger(&path),
            Err(ToolOperationJournalError::MalformedLine { line: 1, .. })
        ));
    }
}
