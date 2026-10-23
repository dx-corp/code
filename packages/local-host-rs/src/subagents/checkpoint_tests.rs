use super::*;

#[test]
fn local_assistant_content_cannot_complete_child_checkpoint() {
    let root = tempfile::tempdir().expect("temp root");
    let session_id = uuid::Uuid::new_v4().to_string();
    let vault = CredentialVault::new();
    let mut recorder = SessionRecorder::with_id(root.path(), &session_id).unwrap();
    let local = FromAgent::LocalAssistantContent {
        response_id: "response".to_owned(),
        content: vec![crate::ai::ContentBlock::Text {
            text: "partial answer".to_owned(),
        }],
    };
    let mut seen = persist_child_event(&mut recorder, &local, &session_id, &vault, 1).unwrap();
    assert!(!terminal_checkpoint_ready(true, seen));
    assert!(recorder.replay().semantic_conversation.is_none());
    let snapshot = FromAgent::ConversationSnapshot {
        local_messages: Vec::new(),
        system_prompt_parts: Vec::new(),
        protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL.to_owned(),
        messages: vec![crate::ai::Message {
            role: crate::ai::Role::Assistant,
            content: crate::ai::MessageContent::text("complete answer"),
        }],
        processed_queue_ids: vec![42],
    };
    seen |= persist_child_event(&mut recorder, &snapshot, &session_id, &vault, 1).unwrap();
    assert!(terminal_checkpoint_ready(true, seen));
    recorder.flush_checkpoint().unwrap();
    let resumed = SessionRecorder::resume(root.path(), &session_id).unwrap();
    assert!(resumed.replay().semantic_conversation.is_some());
    assert!(resumed.semantic_processed_queue_ids().contains(&42));
}

#[test]
fn preterminal_child_snapshot_avoids_legacy_terminal_wait() {
    assert!(!terminal_checkpoint_ready(false, true));
    assert!(terminal_checkpoint_ready(true, true));
    assert!(!terminal_checkpoint_ready(true, false));
}
