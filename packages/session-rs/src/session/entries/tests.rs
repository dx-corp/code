use super::super::wire_format_generated::{
    COMPACTION_CONTEXT_ENTRY_TYPES, COMPACTION_CONTEXT_EXCLUDED_MESSAGE_ROLES,
    CONTENT_BLOCK_FIELD_ALIASES, CONTENT_BLOCK_TYPE_ALIASES, FIELD_ALIASES, STOP_REASON_ALIASES,
    canonical_content_block_type, canonical_stop_reason, content_block_field_aliases,
    field_aliases, is_compaction_context_entry_type, is_compaction_context_excluded_message_role,
};
use super::*;
use serde_json::{Map, Value, json};

#[test]
fn side_questions_and_plan_review_are_typed_entries() {
    let side = SessionEntry::SideQuestion(SideQuestionEntry {
        id: "side-1".into(),
        timestamp: "2026-07-23T12:00:00Z".into(),
        question: "Why?".into(),
        answer: "Because.".into(),
        error: None,
    });
    let value = serde_json::to_value(&side).unwrap();
    assert_eq!(value["type"], "side_question");

    let events = vec![
        PlanReviewEntry {
            timestamp: "2026-07-23T12:00:01Z".into(),
            event: PlanReviewEvent::Comment {
                id: 1,
                start_line: 2,
                end_line: 4,
                text: "Clarify this".into(),
                revision: "revision-1".into(),
                excerpt: "selected lines".into(),
            },
        },
        PlanReviewEntry {
            timestamp: "2026-07-23T12:00:02Z".into(),
            event: PlanReviewEvent::Resolve { id: 1 },
        },
        PlanReviewEntry {
            timestamp: "2026-07-23T12:00:03Z".into(),
            event: PlanReviewEvent::Reopen { id: 1 },
        },
    ];
    let comments = reconstruct_plan_review(&events);
    assert_eq!(comments.len(), 1);
    assert!(!comments[0].resolved);
    assert_eq!((comments[0].start_line, comments[0].end_line), (2, 4));
    assert_eq!(comments[0].revision, "revision-1");
    assert_eq!(comments[0].excerpt, "selected lines");
}

fn repo_test_fixture(kind: &str, name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("test")
        .join("fixtures")
        .join(kind)
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

fn repo_fixture(name: &str) -> String {
    repo_test_fixture("session-wire", name)
}

fn repo_replay_fixture(name: &str) -> String {
    repo_test_fixture("session-replay", name)
}

fn parse_fixture(name: &str) -> Vec<SessionEntry> {
    repo_fixture(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn parse_replay_fixture(name: &str) -> Vec<SessionEntry> {
    repo_replay_fixture(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn as_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => panic!("expected JSON object"),
    }
}

fn sample_wire_value(canonical: &str) -> Value {
    match canonical {
        "modelId" => json!("gpt-5.2"),
        "providerName" => json!("OpenAI"),
        "baseUrl" => json!("https://example.test"),
        "contextWindow" => json!(100_000),
        "maxTokens" => json!(4096),
        "modelMetadata" => json!({
            "provider": "openai",
            "modelId": "gpt-5.2"
        }),
        "thinkingLevel" => json!("high"),
        "systemPrompt" => json!("Persisted system"),
        "promptMetadata" => json!({
            "name": "system",
            "label": "System",
            "hash": "hash-1",
            "source": "service"
        }),
        "promptContextManifest" => json!({
            "cwd": "/tmp",
            "candidates": ["AGENTS.md"],
            "bytesRead": 12,
            "entries": [],
            "diagnostics": []
        }),
        "unifiedContextManifest" => json!({
            "protocolVersion": "maestro.unified-context-manifest.v1",
            "version": 1,
            "cwd": "/tmp",
            "entries": [],
            "diagnostics": []
        }),
        "branchedFrom" => json!("parent-session"),
        "parentSession" => json!("root-session"),
        "resumeSummary" => json!("Continue from the test"),
        "memoryExtractionHash" => json!("abc123"),
        "archivedAt" => json!("2024-01-15T10:31:00Z"),
        "archived" => json!(true),
        "attachmentId" => json!("attachment-1"),
        "extractedText" => json!("extracted text"),
        "stopReason" => json!("tool_use"),
        "toolCallId" => json!("call-1"),
        "toolName" => json!("read"),
        "isError" => json!(true),
        "firstKeptEntryId" => json!("assistant-1"),
        "firstKeptEntryIndex" => json!(1),
        "tokensBefore" => json!(1234),
        "customInstructions" => json!("keep tool context"),
        "fromId" => json!("branch-root"),
        "fromHook" => json!(true),
        "customType" => json!("hook"),
        "targetId" => json!("message-1"),
        "arguments" => json!({ "path": "README.md" }),
        "thinking" => json!("Need a file read"),
        "thinkingSignature" => json!("sig-1"),
        other => panic!("missing sample value for canonical field {other}"),
    }
}

fn insert_alias_fields(object: &mut Map<String, Value>, aliases: &[(&str, &str)]) {
    for &(alias, canonical) in aliases {
        object.insert(alias.to_string(), sample_wire_value(canonical));
    }
}

fn assert_serialized_uses_canonical_fields(value: &Value, aliases: &[(&str, &str)]) {
    for &(alias, canonical) in aliases {
        assert!(
            value.get(canonical).is_some(),
            "missing canonical field {canonical} after parsing alias {alias}"
        );
        assert!(
            value.get(alias).is_none(),
            "serialized entry kept legacy alias {alias}"
        );
    }
}

fn serialized_entry(value: Value) -> Value {
    let entry: SessionEntry = serde_json::from_value(value).unwrap();
    serde_json::to_value(entry).unwrap()
}

fn serialized_assistant_block(block: Value) -> Value {
    serialized_entry(json!({
        "type": "message",
        "timestamp": "2024-01-15T10:30:00Z",
        "message": {
            "role": "assistant",
            "content": [block],
            "timestamp": 0
        }
    }))
}

#[test]
fn parse_session_header() {
    let json = r#"{"type":"session","id":"abc123","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","model":"anthropic/claude-3","thinking_level":"medium"}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();
    match entry {
        SessionEntry::Session(header) => {
            assert_eq!(header.id, "abc123");
            assert_eq!(header.cwd, "/tmp");
        }
        _ => panic!("Expected Session entry"),
    }
}

#[test]
fn parse_user_message() {
    let json = r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{"role":"user","content":"Hello","timestamp":0}}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();
    match entry {
        SessionEntry::Message(msg) => {
            assert_eq!(msg.message.role(), "user");
        }
        _ => panic!("Expected Message entry"),
    }
}

#[test]
fn parse_assistant_message() {
    let json = r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}],"timestamp":0}}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();
    match entry {
        SessionEntry::Message(msg) => {
            assert_eq!(msg.message.role(), "assistant");
            assert_eq!(msg.message.text_content(), "Hi there!");
        }
        _ => panic!("Expected Message entry"),
    }
}

#[test]
fn parse_typescript_assistant_blocks() {
    let json = r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{"role":"assistant","api":"openai-responses","provider":"openai","model":"gpt-5.2","usage":{"input":10,"output":4,"cacheRead":2,"cacheWrite":1,"cost":{"input":0.1,"output":0.2,"cacheRead":0.01,"cacheWrite":0.02,"total":0.33}},"stopReason":"toolUse","timestamp":0,"content":[{"type":"thinking","thinking":"Need a file read","thinkingSignature":"sig-1"},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"README.md"}}]}}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();

    let SessionEntry::Message(msg) = entry else {
        panic!("Expected Message entry");
    };
    let AppMessage::Assistant {
        content,
        stop_reason,
        usage,
        ..
    } = msg.message
    else {
        panic!("Expected assistant message");
    };

    assert_eq!(stop_reason.as_deref(), Some("toolUse"));
    assert_eq!(usage.unwrap().cache_read, 2);
    match &content[0] {
        ContentBlock::Thinking { text, signature } => {
            assert_eq!(text, "Need a file read");
            assert_eq!(signature.as_deref(), Some("sig-1"));
        }
        _ => panic!("Expected thinking block"),
    }
    match &content[1] {
        ContentBlock::ToolCall { id, name, args, .. } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "read");
            assert_eq!(args["path"], "README.md");
        }
        _ => panic!("Expected tool call block"),
    }
}

#[test]
fn parse_legacy_snake_case_assistant_blocks() {
    let json = r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"thinking","text":"Need a file read","signature":"sig-1"},{"type":"tool_call","id":"call_1","name":"read","args":{"path":"README.md"}}],"timestamp":0}}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();

    let SessionEntry::Message(msg) = entry else {
        panic!("Expected Message entry");
    };
    let AppMessage::Assistant {
        content,
        stop_reason,
        ..
    } = msg.message
    else {
        panic!("Expected assistant message");
    };

    assert_eq!(stop_reason.as_deref(), Some("tool_use"));
    assert!(matches!(content[0], ContentBlock::Thinking { .. }));
    assert!(matches!(content[1], ContentBlock::ToolCall { .. }));
}

#[test]
fn parse_typescript_tool_result_message() {
    let json = r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"file contents"}],"details":{"path":"README.md"},"isError":false,"timestamp":0}}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();

    let SessionEntry::Message(msg) = entry else {
        panic!("Expected Message entry");
    };
    let AppMessage::ToolResult {
        tool_call_id,
        tool_name,
        content,
        is_error,
        ..
    } = msg.message
    else {
        panic!("Expected tool result message");
    };

    assert_eq!(tool_call_id, "call_1");
    assert_eq!(tool_name, "read");
    assert_eq!(content, "file contents");
    assert!(!is_error);
}

#[test]
fn parse_typescript_session_meta_archive_state() {
    let json = r#"{"type":"session_meta","timestamp":"2024-01-15T10:30:00Z","archived":true,"archivedAt":"2024-01-15T10:31:00Z"}"#;
    let entry: SessionEntry = serde_json::from_str(json).unwrap();

    let SessionEntry::SessionMeta(meta) = &entry else {
        panic!("Expected session meta entry");
    };
    assert_eq!(meta.archived, Some(true));
    assert_eq!(meta.archived_at.as_deref(), Some("2024-01-15T10:31:00Z"));

    let serialized = serde_json::to_value(entry).unwrap();
    assert_eq!(serialized["archived"], true);
    assert_eq!(serialized["archivedAt"], "2024-01-15T10:31:00Z");
}

#[test]
fn parse_shared_session_wire_fixtures() {
    for fixture in [
        "canonical-tool-session.jsonl",
        "legacy-rust-tool-session.jsonl",
        "receipt-tool-session.jsonl",
    ] {
        let entries = parse_fixture(fixture);
        assert!(!entries.is_empty(), "fixture {fixture} should not be empty");
    }
}

#[test]
fn tool_result_receipt_survives_session_wire_round_trip() {
    let entries = parse_fixture("receipt-tool-session.jsonl");
    let serialized = serde_json::to_value(&entries[1]).expect("serialize receipt entry");
    let receipt = &serialized["message"]["receipt"];
    assert_eq!(receipt["call_id"], "call-1");
    assert_eq!(receipt["tool_name"], "read");
    assert_eq!(receipt["source"], "remote_client");
    assert_eq!(receipt["duration_ms"], 42);
    assert_eq!(receipt["status"]["status"], "cancelled");
    assert_eq!(receipt["status"]["phase"], "running");
    assert_eq!(receipt["details"]["kind"], "mcp");
    assert_eq!(receipt["details"]["details"]["server"], "filesystem");
    assert_eq!(receipt["details"]["details"]["tool"], "read");
    assert_eq!(receipt["details"]["details"]["is_error"], true);

    let round_tripped: SessionEntry =
        serde_json::from_value(serialized.clone()).expect("deserialize receipt entry");
    assert_eq!(
        serde_json::to_value(round_tripped).expect("reserialize receipt entry")["message"]["receipt"],
        *receipt
    );
}

#[test]
fn legacy_tool_result_session_omits_missing_receipt() {
    let entries = parse_fixture("legacy-rust-tool-session.jsonl");
    let serialized = serde_json::to_value(&entries[3]).expect("serialize legacy tool result");
    assert!(serialized["message"].get("receipt").is_none());
}

#[derive(Debug, Clone)]
struct ReplayToolRequest {
    args: Value,
}

fn replay_text_content(message: &AppMessage) -> Option<String> {
    let text = message.text_content();
    if text.is_empty() { None } else { Some(text) }
}

fn replay_compact_item(
    id: String,
    item_type: &str,
    status: &str,
    visibility: &str,
    title: &str,
) -> Map<String, Value> {
    let mut item = Map::new();
    item.insert("id".to_string(), json!(id));
    item.insert("type".to_string(), json!(item_type));
    item.insert("status".to_string(), json!(status));
    item.insert("visibility".to_string(), json!(visibility));
    item.insert("source".to_string(), json!("local"));
    item.insert("title".to_string(), json!(title));
    item
}

fn replay_tool_path(request: Option<&ReplayToolRequest>) -> Option<String> {
    request
        .and_then(|request| {
            request
                .args
                .get("path")
                .or_else(|| request.args.get("file_path"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn replay_push_message_items(
    items: &mut Vec<(String, Value)>,
    session_id: &str,
    base_id: &str,
    timestamp: &str,
    message: &AppMessage,
    tool_requests: &mut std::collections::HashMap<String, ReplayToolRequest>,
) {
    match message {
        AppMessage::User { .. } => {
            let mut item = replay_compact_item(
                format!("message:{base_id}"),
                "message.user",
                "completed",
                "user",
                "User message",
            );
            item.insert("role".to_string(), json!("user"));
            if let Some(summary) = replay_text_content(message) {
                item.insert("summary".to_string(), json!(summary));
            }
            items.push((timestamp.to_string(), Value::Object(item)));
        }
        AppMessage::Assistant { content, .. } => {
            let mut item = replay_compact_item(
                format!("message:{base_id}"),
                "message.assistant",
                "completed",
                "user",
                "Assistant response",
            );
            item.insert("role".to_string(), json!("assistant"));
            if let Some(summary) = replay_text_content(message) {
                item.insert("summary".to_string(), json!(summary));
            }
            items.push((timestamp.to_string(), Value::Object(item)));

            for block in content {
                let ContentBlock::ToolCall { id, name, args, .. } = block else {
                    continue;
                };
                tool_requests.insert(id.clone(), ReplayToolRequest { args: args.clone() });
                let mut request = replay_compact_item(
                    format!("tool-requested:{base_id}:{id}"),
                    "tool.requested",
                    "running",
                    "user",
                    &format!("Requested {name}"),
                );
                request.insert("toolCallId".to_string(), json!(id));
                request.insert("toolName".to_string(), json!(name));
                items.push((timestamp.to_string(), Value::Object(request)));
            }
        }
        AppMessage::ToolResult {
            tool_call_id,
            tool_name,
            details,
            is_error,
            ..
        } => {
            let request = tool_requests.get(tool_call_id);
            if !is_error && (tool_name == "edit" || tool_name == "write") {
                let details = details.as_ref();
                let edits_applied = details
                    .and_then(|value| value.get("editsApplied"))
                    .and_then(Value::as_u64);
                let bytes_written = details
                    .and_then(|value| value.get("bytesWritten"))
                    .and_then(Value::as_u64);
                let previous_exists = details
                    .and_then(|value| value.get("previousExists"))
                    .and_then(Value::as_bool);
                let display_path = replay_tool_path(request);
                let action = if tool_name == "write" {
                    if previous_exists == Some(false) {
                        "created"
                    } else {
                        "wrote"
                    }
                } else {
                    "edited"
                };
                let mut file_change = replay_compact_item(
                    format!("file-change:{base_id}:{tool_call_id}"),
                    "file.changed",
                    "completed",
                    "user",
                    &format!("File {action}"),
                );
                file_change.insert("toolCallId".to_string(), json!(tool_call_id));
                file_change.insert("toolName".to_string(), json!(tool_name));
                let summary = [
                    display_path,
                    bytes_written.map(|bytes| format!("{bytes} bytes")),
                    edits_applied.map(|edits| format!("{edits} edits")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" | ");
                if !summary.is_empty() {
                    file_change.insert("summary".to_string(), json!(summary));
                }
                items.push((timestamp.to_string(), Value::Object(file_change)));
            }

            let status = if *is_error { "failed" } else { "completed" };
            let mut result = replay_compact_item(
                format!("tool-result:{base_id}:{tool_call_id}"),
                if *is_error {
                    "tool.failed"
                } else {
                    "tool.completed"
                },
                status,
                "user",
                &format!("{tool_name} {status}"),
            );
            result.insert("role".to_string(), json!("tool"));
            result.insert("toolCallId".to_string(), json!(tool_call_id));
            result.insert("toolName".to_string(), json!(tool_name));
            items.push((timestamp.to_string(), Value::Object(result)));
        }
    }

    let _ = session_id;
}

fn replay_normalized_timeline(entries: &[SessionEntry]) -> Value {
    let mut items: Vec<(String, Value)> = Vec::new();
    let mut session_id = String::new();
    let mut tool_requests = std::collections::HashMap::new();

    for entry in entries {
        match entry {
            SessionEntry::Session(header) => {
                session_id = header.id.clone();
                items.push((
                    header.timestamp.clone(),
                    Value::Object(replay_compact_item(
                        format!("session-started:{}", header.id),
                        "session.started",
                        "info",
                        "user",
                        "Session started",
                    )),
                ));
            }
            SessionEntry::Message(message) => {
                replay_push_message_items(
                    &mut items,
                    &session_id,
                    message.id.as_deref().unwrap_or("missing-id"),
                    &message.timestamp,
                    &message.message,
                    &mut tool_requests,
                );
            }
            SessionEntry::BranchSummary(branch) => {
                let mut item = replay_compact_item(
                    format!("branch:{}", branch.id.as_deref().unwrap_or("missing-id")),
                    "branch.created",
                    "info",
                    "admin",
                    "Branch summary created",
                );
                item.insert("summary".to_string(), json!(branch.summary));
                items.push((branch.timestamp.clone(), Value::Object(item)));
            }
            SessionEntry::CustomMessage(message) => {
                if !message.display {
                    continue;
                }
                let mut item = replay_compact_item(
                    format!(
                        "custom-message:{}",
                        message.id.as_deref().unwrap_or("missing-id")
                    ),
                    "custom.event",
                    "info",
                    "admin",
                    &message.custom_type,
                );
                let content = match &message.content {
                    MessageContent::Text(text) => Some(text.clone()),
                    MessageContent::Blocks(blocks) => {
                        let text = blocks
                            .iter()
                            .filter_map(|block| {
                                if let ContentBlock::Text { text } = block {
                                    Some(text.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        if text.is_empty() { None } else { Some(text) }
                    }
                };
                if let Some(summary) = content {
                    item.insert("summary".to_string(), json!(summary));
                }
                items.push((message.timestamp.clone(), Value::Object(item)));
            }
            SessionEntry::Compaction(compaction) => {
                let mut item = replay_compact_item(
                    format!(
                        "compaction:{}",
                        compaction.id.as_deref().unwrap_or("missing-id")
                    ),
                    "compaction.created",
                    "info",
                    "admin",
                    "Context compacted",
                );
                item.insert("summary".to_string(), json!(compaction.summary));
                items.push((compaction.timestamp.clone(), Value::Object(item)));
            }
            SessionEntry::SessionMeta(_)
            | SessionEntry::AttachmentExtract(_)
            | SessionEntry::ThinkingLevelChange(_)
            | SessionEntry::ModelChange(_)
            | SessionEntry::Custom(_)
            | SessionEntry::Label(_)
            | SessionEntry::SideQuestion(_)
            | SessionEntry::PlanReview(_) => {}
        }
    }

    items.sort_by(|(left_ts, left), (right_ts, right)| {
        left_ts.cmp(right_ts).then_with(|| {
            left["id"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["id"].as_str().unwrap_or_default())
        })
    });
    Value::Array(items.into_iter().map(|(_, value)| value).collect())
}

#[test]
fn rust_parser_matches_shared_replay_fixture_timeline() {
    let entries = parse_replay_fixture("legacy-compacted-mcp-session.jsonl");
    let expected: Value = serde_json::from_str(&repo_replay_fixture(
        "legacy-compacted-mcp-session.replay.json",
    ))
    .unwrap();

    assert_eq!(replay_normalized_timeline(&entries), expected["timeline"]);

    let SessionEntry::Session(header) = &entries[0] else {
        panic!("expected session header");
    };
    assert_eq!(header.id, "replay-legacy-mcp-1");
    assert_eq!(header.thinking_level, ThinkingLevel::High);
    assert_eq!(header.parent_session.as_deref(), Some("imported-parent-1"));
    assert!(header.unified_context_manifest.is_some());
    assert_eq!(header.tools.len(), 3);
}

#[test]
fn generated_stop_reason_aliases_match_rust_serializer() {
    for &(alias, canonical) in STOP_REASON_ALIASES {
        let serialized = serialized_entry(json!({
            "type": "message",
            "timestamp": "2024-01-15T10:30:00Z",
            "message": {
                "role": "assistant",
                "stopReason": alias,
                "content": [],
                "timestamp": 0
            }
        }));

        assert_eq!(serialized["message"]["stopReason"], canonical);
    }
}

#[test]
fn generated_content_block_aliases_match_rust_deserializer() {
    for &(alias, canonical) in CONTENT_BLOCK_TYPE_ALIASES {
        assert_eq!(canonical_content_block_type(alias), canonical);
        let serialized = serialized_assistant_block(json!({
            "type": alias,
            "id": "call-1",
            "name": "read",
            "arguments": { "path": "README.md" }
        }));

        assert_eq!(serialized["message"]["content"][0]["type"], canonical);
    }

    for &(block_type, aliases) in CONTENT_BLOCK_FIELD_ALIASES {
        assert_eq!(content_block_field_aliases(block_type), aliases);
        let mut block = match block_type {
            "toolCall" => as_object(json!({
                "type": "toolCall",
                "id": "call-1",
                "name": "read"
            })),
            "thinking" => as_object(json!({
                "type": "thinking"
            })),
            other => panic!("generated content block alias test missing {other}"),
        };
        insert_alias_fields(&mut block, aliases);

        let serialized = serialized_assistant_block(Value::Object(block));
        let serialized_block = &serialized["message"]["content"][0];
        assert_serialized_uses_canonical_fields(serialized_block, aliases);
    }
}

#[test]
fn generated_field_aliases_match_rust_deserializer() {
    for &(section, aliases) in FIELD_ALIASES {
        assert_eq!(field_aliases(section), aliases);

        match section {
            "modelMetadata" => {
                let mut metadata = as_object(json!({ "provider": "openai" }));
                insert_alias_fields(&mut metadata, aliases);
                let serialized = serialized_entry(json!({
                    "type": "session",
                    "id": "session-1",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "cwd": "/tmp",
                    "model": "openai/gpt-5.2",
                    "modelMetadata": Value::Object(metadata)
                }));
                assert_serialized_uses_canonical_fields(&serialized["modelMetadata"], aliases);
            }
            "session" => {
                let mut session = as_object(json!({
                    "type": "session",
                    "id": "session-1",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "cwd": "/tmp",
                    "model": "openai/gpt-5.2"
                }));
                insert_alias_fields(&mut session, aliases);
                let serialized = serialized_entry(Value::Object(session));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "sessionMeta" => {
                let mut entry = as_object(json!({
                    "type": "session_meta",
                    "timestamp": "2024-01-15T10:30:00Z"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "attachmentExtract" => {
                let mut entry = as_object(json!({
                    "type": "attachment_extract",
                    "timestamp": "2024-01-15T10:30:00Z"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "assistantMessage" => {
                let mut message = as_object(json!({
                    "role": "assistant",
                    "content": [],
                    "timestamp": 0
                }));
                insert_alias_fields(&mut message, aliases);
                let serialized = serialized_entry(json!({
                    "type": "message",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "message": Value::Object(message)
                }));
                assert_serialized_uses_canonical_fields(&serialized["message"], aliases);
                assert_eq!(serialized["message"]["stopReason"], "toolUse");
            }
            "toolResultMessage" => {
                let mut message = as_object(json!({
                    "role": "toolResult",
                    "content": "file contents",
                    "timestamp": 0
                }));
                insert_alias_fields(&mut message, aliases);
                let serialized = serialized_entry(json!({
                    "type": "message",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "message": Value::Object(message)
                }));
                assert_serialized_uses_canonical_fields(&serialized["message"], aliases);
            }
            "thinkingLevelChange" => {
                let mut entry = as_object(json!({
                    "type": "thinking_level_change",
                    "timestamp": "2024-01-15T10:30:00Z"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "modelChange" => {
                let mut entry = as_object(json!({
                    "type": "model_change",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "model": "openai/gpt-5.2"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "compaction" => {
                let mut entry = as_object(json!({
                    "type": "compaction",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "summary": "Kept the useful work",
                    "auto": true
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "branchSummary" => {
                let mut entry = as_object(json!({
                    "type": "branch_summary",
                    "id": "branch-summary-1",
                    "parentId": "message-1",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "summary": "Branch result"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "custom" => {
                let mut entry = as_object(json!({
                    "type": "custom",
                    "id": "custom-1",
                    "parentId": "message-1",
                    "timestamp": "2024-01-15T10:30:00Z"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "customMessage" => {
                let mut entry = as_object(json!({
                    "type": "custom_message",
                    "id": "custom-message-1",
                    "parentId": "message-1",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "content": "Hook content",
                    "display": true
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            "label" => {
                let mut entry = as_object(json!({
                    "type": "label",
                    "id": "label-1",
                    "parentId": "message-1",
                    "timestamp": "2024-01-15T10:30:00Z",
                    "label": "Interesting"
                }));
                insert_alias_fields(&mut entry, aliases);
                let serialized = serialized_entry(Value::Object(entry));
                assert_serialized_uses_canonical_fields(&serialized, aliases);
            }
            other => panic!("generated field alias test missing section {other}"),
        }
    }
}

#[test]
fn generated_compaction_context_manifest_matches_rust_helpers() {
    for entry_type in COMPACTION_CONTEXT_ENTRY_TYPES {
        assert!(is_compaction_context_entry_type(entry_type));
    }
    assert!(!is_compaction_context_entry_type("tool_result"));

    for role in COMPACTION_CONTEXT_EXCLUDED_MESSAGE_ROLES {
        assert!(is_compaction_context_excluded_message_role(role));
    }
    assert!(!is_compaction_context_excluded_message_role("assistant"));
}

#[test]
fn serialize_rust_entries_prefers_canonical_session_wire_keys() {
    let entries = parse_fixture("legacy-rust-tool-session.jsonl");

    let session = serde_json::to_value(&entries[0]).unwrap();
    assert_eq!(session["thinkingLevel"], "medium");
    assert_eq!(session["modelMetadata"]["modelId"], "gpt-5.2");
    assert_eq!(session["systemPrompt"], "Persisted system");
    assert_eq!(session["branchedFrom"], "parent-session");
    assert!(session.get("thinking_level").is_none());
    assert!(session.get("model_metadata").is_none());
    assert!(session.get("system_prompt").is_none());
    assert!(session.get("branched_from").is_none());

    let assistant = serde_json::to_value(&entries[2]).unwrap();
    let assistant_message = &assistant["message"];
    assert_eq!(assistant_message["stopReason"], "toolUse");
    assert!(assistant_message.get("stop_reason").is_none());
    assert_eq!(
        assistant_message["content"][0]["thinking"],
        "Need a file read"
    );
    assert_eq!(
        assistant_message["content"][0]["thinkingSignature"],
        "sig-1"
    );
    assert_eq!(assistant_message["content"][1]["type"], "toolCall");
    assert_eq!(
        assistant_message["content"][1]["arguments"]["path"],
        "README.md"
    );
    assert!(assistant_message["content"][1].get("args").is_none());

    let tool_result = serde_json::to_value(&entries[3]).unwrap();
    let tool_result_message = &tool_result["message"];
    assert_eq!(tool_result_message["toolCallId"], "call-1");
    assert_eq!(tool_result_message["toolName"], "read");
    assert_eq!(tool_result_message["isError"], false);
    assert!(tool_result_message.get("tool_call_id").is_none());
    assert!(tool_result_message.get("tool_name").is_none());
    assert!(tool_result_message.get("is_error").is_none());

    let model_change = serde_json::to_value(&entries[4]).unwrap();
    assert_eq!(model_change["modelMetadata"]["modelId"], "gpt-5.2");
    assert!(model_change.get("model_metadata").is_none());

    let thinking_level_change = serde_json::to_value(&entries[5]).unwrap();
    assert_eq!(thinking_level_change["thinkingLevel"], "high");
    assert!(thinking_level_change.get("thinking_level").is_none());

    let compaction = serde_json::to_value(&entries[6]).unwrap();
    assert_eq!(compaction["firstKeptEntryIndex"], 0);
    assert_eq!(compaction["tokensBefore"], 1234);
    assert_eq!(compaction["customInstructions"], "keep tool context");
    assert!(compaction.get("first_kept_entry_index").is_none());
    assert!(compaction.get("tokens_before").is_none());
    assert!(compaction.get("custom_instructions").is_none());
}

#[test]
fn thinking_level_serialize() {
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::High).unwrap(),
        "\"high\""
    );
}

#[test]
fn token_usage_total() {
    let usage = TokenUsage {
        input: 100,
        output: 50,
        ..Default::default()
    };
    assert_eq!(usage.total(), 150);
}

#[test]
fn compaction_continuation_round_trips_in_session_jsonl_shape() {
    use maestro_context::{ContinuationCommand, ContinuationRecord};

    let entry = CompactionEntry {
        id: Some("compact-1".to_string()),
        parent_id: None,
        timestamp: "2026-08-05T00:00:00Z".to_string(),
        summary: "summary".to_string(),
        first_kept_entry_id: None,
        first_kept_entry_index: Some(2),
        tokens_before: 42,
        auto: true,
        custom_instructions: None,
        continuation: Some(ContinuationRecord {
            projected_tool_images: vec![],
            file_operations: vec![maestro_context::compaction::ContinuationFileOperation {
                tool_call_id: "write-1".into(),
                path: "src/lib.rs".into(),
                kind: maestro_context::compaction::ContinuationFileOperationKind::Write,
            }],
            tool_outputs: vec![maestro_context::compaction::ToolOutputReference {
                tool_call_id: "call-1".into(),
                path: "/session-owned/output.txt".into(),
            }],
            objective: Some("Ship the workflow".to_string()),
            user_requests: vec![],
            constraints: vec!["Do not deploy".to_string()],
            decisions: vec![],
            open_questions: vec!["Which region?".to_string()],
            evidence: vec![],
            commands: vec![ContinuationCommand {
                tool_call_id: "call-1".to_string(),
                command: "cargo test".to_string(),
                outcome: Some("ok".to_string()),
                failed: false,
            }],
            next_actions: vec!["Open a draft PR".to_string()],
            verification: vec![],
            source_hash: "abc123".to_string(),
        }),
    };

    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(value["continuation"]["source_hash"], "abc123");
    assert_eq!(
        value["continuation"]["file_operations"][0]["path"],
        "src/lib.rs"
    );
    let decoded: CompactionEntry = serde_json::from_value(value).unwrap();
    assert_eq!(
        decoded.continuation.unwrap().commands[0].command,
        "cargo test"
    );
}

#[test]
fn all_entry_variants_roundtrip_against_golden_jsonl() {
    // Golden-file contract: every SessionEntry variant, in its canonical
    // on-disk JSONL form, must parse and re-serialize to the identical value.
    let expected: &[(&str, &str)] = &[
        ("session", "Session"),
        ("message", "Message"),
        ("message", "Message"),
        ("message", "Message"),
        ("message", "Message"),
        ("attachment_extract", "AttachmentExtract"),
        ("thinking_level_change", "ThinkingLevelChange"),
        ("model_change", "ModelChange"),
        ("session_meta", "SessionMeta"),
        ("compaction", "Compaction"),
        ("branch_summary", "BranchSummary"),
        ("custom", "Custom"),
        ("custom_message", "CustomMessage"),
        ("label", "Label"),
        ("side_question", "SideQuestion"),
        ("plan_review", "PlanReview"),
    ];
    let golden = repo_fixture("all-entry-variants.jsonl");
    let lines: Vec<&str> = golden
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        expected.len(),
        "golden fixture must contain one line per expected entry"
    );

    for (line, &(expected_type, expected_variant)) in lines.iter().zip(expected) {
        let golden_value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(golden_value["type"], expected_type);

        let entry: SessionEntry = serde_json::from_str(line).unwrap();
        let actual_variant = match &entry {
            SessionEntry::Session(_) => "Session",
            SessionEntry::Message(_) => "Message",
            SessionEntry::AttachmentExtract(_) => "AttachmentExtract",
            SessionEntry::ThinkingLevelChange(_) => "ThinkingLevelChange",
            SessionEntry::ModelChange(_) => "ModelChange",
            SessionEntry::SessionMeta(_) => "SessionMeta",
            SessionEntry::Compaction(_) => "Compaction",
            SessionEntry::BranchSummary(_) => "BranchSummary",
            SessionEntry::Custom(_) => "Custom",
            SessionEntry::CustomMessage(_) => "CustomMessage",
            SessionEntry::Label(_) => "Label",
            SessionEntry::SideQuestion(_) => "SideQuestion",
            SessionEntry::PlanReview(_) => "PlanReview",
        };
        assert_eq!(
            actual_variant, expected_variant,
            "type tag {expected_type} deserialized to the wrong variant"
        );

        let serialized = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            serialized, golden_value,
            "roundtrip changed the on-disk form of {expected_type}"
        );
    }
}

#[test]
fn stop_reason_alias_table_is_pinned_to_wire_contract() {
    assert_eq!(
        STOP_REASON_ALIASES,
        &[
            ("tool_use", "toolUse"),
            ("tool_calls", "toolUse"),
            ("max_tokens", "length"),
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
        ]
    );
    for &(alias, canonical) in STOP_REASON_ALIASES {
        assert_eq!(canonical_stop_reason(alias), canonical);
    }
    // Canonical and unknown reasons pass through unchanged.
    assert_eq!(canonical_stop_reason("stop"), "stop");
    assert_eq!(canonical_stop_reason("length"), "length");
    assert_eq!(
        canonical_stop_reason("unknown_future_reason"),
        "unknown_future_reason"
    );
}

#[test]
fn unknown_fields_are_tolerated_for_forward_compatibility() {
    // Entries written by a newer version may carry fields this build does not
    // know about; they must still parse, keep known fields intact, and drop
    // the unknown fields on re-serialization.
    let header: SessionEntry = serde_json::from_str(
        r#"{"type":"session","id":"s1","timestamp":"2024-01-15T10:30:00Z","cwd":"/tmp","futureField":{"x":1},"anotherNewThing":[1,2]}"#,
    )
    .unwrap();
    let SessionEntry::Session(header) = header else {
        panic!("Expected Session entry");
    };
    assert_eq!(header.id, "s1");
    let serialized = serde_json::to_value(SessionEntry::Session(header)).unwrap();
    assert!(serialized.get("futureField").is_none());
    assert!(serialized.get("anotherNewThing").is_none());

    let message: SessionEntry = serde_json::from_str(
        r#"{"type":"message","timestamp":"2024-01-15T10:30:00Z","futureEntryField":true,"message":{"role":"assistant","content":[{"type":"text","text":"hi","futureBlockField":1}],"timestamp":0,"futureMessageField":"x"}}"#,
    )
    .unwrap();
    let SessionEntry::Message(message) = message else {
        panic!("Expected Message entry");
    };
    assert_eq!(message.message.text_content(), "hi");
    let serialized = serde_json::to_value(SessionEntry::Message(message)).unwrap();
    assert!(serialized.get("futureEntryField").is_none());
    assert!(serialized["message"].get("futureMessageField").is_none());
    assert!(
        serialized["message"]["content"][0]
            .get("futureBlockField")
            .is_none()
    );
}

#[test]
fn session_history_fixture_matches_native_writer_wire_format() {
    let fixture = include_str!("../../../../session-history-rs/tests/fixtures/maestro.jsonl");
    let entries: Vec<SessionEntry> = fixture
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let values: Vec<Value> = entries
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    assert_eq!(values[0]["type"], "session");
    assert_eq!(values[2]["message"]["content"][1]["type"], "toolCall");
    assert_eq!(values[3]["message"]["role"], "toolResult");
    assert_eq!(values[3]["message"]["toolCallId"], "call-check");
    assert_eq!(values[3]["message"]["isError"], false);
    assert_eq!(values[2]["message"]["usage"]["input"], 12);
}
