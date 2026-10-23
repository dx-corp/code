use super::*;

#[test]
fn transcript_images_resume_retains_typed_call_anchors_and_outcomes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    let mut entries = vec![serde_json::json!({
        "type":"session", "version":2, "id":"image-restore", "timestamp":"2026-10-23T00:00:00Z", "cwd":"/tmp", "model":"gpt-4o", "thinkingLevel":"medium"
    })];
    let calls: Vec<_> = ["viewed", "failed", "cancelled", "pending"].into_iter().map(|id| serde_json::json!({"type":"toolCall", "id":id, "name":"read_image", "arguments":{"file_path":"/missing/admitted.png"}})).collect();
    entries.push(
        serde_json::json!({"type":"message", "timestamp":"2026-10-23T00:00:01Z", "message":{
            "role":"assistant", "timestamp":1, "content": calls
        }}),
    );
    for (id, is_error) in [("viewed", false), ("failed", true), ("cancelled", true)] {
        let mut message = serde_json::json!({"role":"toolResult", "timestamp":2, "toolCallId":id,"toolName":"read_image","content":format!("result {id}"),"isError":is_error});
        if id == "cancelled" {
            message["receipt"] = serde_json::json!({"call_id":id,"tool_name":"read_image","source":"native","status":{"status":"cancelled","phase":"running"},"duration_ms":17,"details":{"kind":"none"}});
        }
        entries.push(serde_json::json!({"type":"message","timestamp":"2026-10-23T00:00:02Z","message":message}));
    }
    let contents = entries
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, contents).unwrap();
    let session = crate::session::SessionReader::read_file(path).unwrap();
    let mut state = AppState::new();
    restore_visible_session_messages(&mut state, &session);
    let calls = &state.messages[0].tool_calls;
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].call_id, "viewed");
    assert_eq!(calls[0].status, crate::state::ToolCallStatus::Completed);
    assert_eq!(calls[0].output, "result viewed");
    assert_eq!(calls[1].status, crate::state::ToolCallStatus::Failed);
    assert_eq!(calls[2].status, crate::state::ToolCallStatus::Cancelled);
    assert_eq!(calls[2].duration_ms, Some(17));
    assert_eq!(calls[3].status, crate::state::ToolCallStatus::Pending);
    assert!(calls[3].output.is_empty());
    assert!(calls.iter().all(|call| call.parent_call_id.is_none()));
}
