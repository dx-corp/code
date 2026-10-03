use super::{
    clear_pending_tool_response_maps, managed_gateway_receipt_status,
    native_chat_acknowledges_peer_messages, native_chat_terminal_status,
};
use maestro_local_host::agent::FromAgent;

#[tokio::test]
async fn failed_chat_writes_stop_the_embedding_before_removing_attachments() {
    use maestro_local_host::ai::{ScriptedBlock, ScriptedResponse, StopReason};
    use maestro_local_host::embedding::test_kit::ScriptedEmbeddingBuilder;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    for websocket in [false, true] {
        let workspace = tempfile::tempdir().expect("workspace");
        let attachment_dir = workspace.path().join("attachments");
        std::fs::create_dir(&attachment_dir).expect("attachment directory");
        let attachment = attachment_dir.join("request.txt");
        std::fs::write(&attachment, "request context").expect("attachment");
        let session = ScriptedEmbeddingBuilder::new(vec![ScriptedResponse {
            blocks: vec![ScriptedBlock::Pending],
            stop_reason: StopReason::EndTurn,
            error: None,
        }])
        .working_directory(workspace.path())
        .start()
        .expect("embedding");
        let (agent, mut events) = session.into_parts();
        agent.prompt("Wait for the client.").await.expect("prompt");
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if matches!(event, FromAgent::ResponseStart { .. }) {
                    return;
                }
            }
            panic!("embedding stopped before the request began");
        })
        .await
        .expect("pending model request");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let mut writer = TcpStream::connect(listener.local_addr().expect("address"))
            .await
            .expect("writer");
        let (_reader, _) = listener.accept().await.expect("reader");
        writer.shutdown().await.expect("close write half");
        let message = serde_json::json!({ "type": "done" });
        let outcome = if websocket {
            super::send_ws_json(&mut writer, &message).await
        } else {
            super::send_sse(&mut writer, &message).await
        };
        assert!(
            outcome.is_err(),
            "the closed transport must reject the write"
        );
        let original_error = outcome.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            super::finish_embedded_agent(
                agent,
                super::PreparedAttachments {
                    paths: vec![attachment.to_string_lossy().into_owned()],
                    temp_dir: Some(attachment_dir.clone()),
                },
                outcome,
            ),
        )
        .await
        .expect("cleanup cancels the pending model");
        assert_eq!(result, original_error);
        assert!(!attachment_dir.exists());
        tokio::time::timeout(Duration::from_secs(5), async {
            while events.recv().await.is_some() {}
        })
        .await
        .expect("the actor and event relay must be closed after cleanup");
    }
}

#[tokio::test]
async fn pending_response_cleanup_removes_only_this_turns_client_and_approval_entries() {
    use std::collections::{HashMap, HashSet};
    use tokio::sync::{Mutex, mpsc};

    let client_call = "client-tool-call".to_string();
    let approval_call = "approval-call".to_string();
    let unrelated_call = "other-turn-call".to_string();
    let (client_sender, _client_receiver) =
        mpsc::unbounded_channel::<maestro_local_host::agent::ToolResponseMessage>();
    let (approval_sender, _approval_receiver) =
        mpsc::unbounded_channel::<maestro_local_host::agent::ToolResponseMessage>();
    let (unrelated_sender, _unrelated_receiver) =
        mpsc::unbounded_channel::<maestro_local_host::agent::ToolResponseMessage>();
    let pending_tool_responses = Mutex::new(HashMap::from([
        (client_call.clone(), client_sender),
        (approval_call.clone(), approval_sender),
        (unrelated_call.clone(), unrelated_sender),
    ]));
    let pending_tool_response_sessions = Mutex::new(HashMap::from([
        (
            client_call.clone(),
            super::PendingToolResponseOwner::Session("session-client".to_string()),
        ),
        (
            approval_call.clone(),
            super::PendingToolResponseOwner::Session("session-approval".to_string()),
        ),
        (
            unrelated_call.clone(),
            super::PendingToolResponseOwner::Session("session-other".to_string()),
        ),
    ]));
    let completed_client_tool_results = Mutex::new(HashMap::from([
        (client_call.clone(), true),
        (approval_call.clone(), false),
        (unrelated_call.clone(), true),
    ]));
    let this_turn = HashSet::from([client_call.clone(), approval_call.clone()]);

    clear_pending_tool_response_maps(
        &pending_tool_responses,
        &pending_tool_response_sessions,
        &completed_client_tool_results,
        &this_turn,
    )
    .await;

    for call_id in [&client_call, &approval_call] {
        assert!(
            !pending_tool_responses.lock().await.contains_key(call_id),
            "pending sender for {call_id} must be removed",
        );
        assert!(
            !pending_tool_response_sessions
                .lock()
                .await
                .contains_key(call_id),
            "pending owner for {call_id} must be removed",
        );
        assert!(
            !completed_client_tool_results
                .lock()
                .await
                .contains_key(call_id),
            "completed result for {call_id} must be removed",
        );
    }
    assert!(
        pending_tool_responses
            .lock()
            .await
            .contains_key(&unrelated_call),
        "another active turn's sender must remain",
    );
    assert!(
        pending_tool_response_sessions
            .lock()
            .await
            .contains_key(&unrelated_call),
        "another active turn's owner must remain",
    );
    assert!(
        completed_client_tool_results
            .lock()
            .await
            .contains_key(&unrelated_call),
        "another active turn's completed result must remain",
    );
}

#[test]
fn managed_gateway_receipt_status_contains_safe_camel_case_fields() {
    let status = managed_gateway_receipt_status(
        "request-1".to_string(),
        "record-1".to_string(),
        "lineage-1".to_string(),
        "planned".to_string(),
    );

    assert_eq!(status["type"], "status");
    assert_eq!(status["status"], "managed_gateway_receipt");
    assert_eq!(status["details"]["requestId"], "request-1");
    assert_eq!(status["details"]["recordId"], "record-1");
    assert_eq!(status["details"]["lineageId"], "lineage-1");
    assert_eq!(status["details"]["recordStatus"], "planned");
    assert!(
        !status
            .to_string()
            .contains("managed_inference_authorization")
    );
}

#[test]
fn native_chat_requires_explicit_turn_terminal() {
    assert!(
        native_chat_terminal_status(&FromAgent::ResponseEnd {
            response_id: "done".to_string(),
            usage: None,
        })
        .is_none()
    );
    assert!(matches!(
        native_chat_terminal_status(&FromAgent::TurnCompleted {
            response_id: "done".to_string(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        }),
        Some(Ok(()))
    ));
    assert!(matches!(
        native_chat_terminal_status(&FromAgent::ProviderError {
            kind: maestro_local_host::ai::ProviderStreamErrorKind::TransientProtocol,
            message: "unexpected eof".to_string(),
        }),
        Some(Err(message)) if message.contains("unexpected eof")
    ));
}

#[test]
fn native_chat_failures_leave_peer_messages_pending_for_redelivery() {
    assert!(!native_chat_acknowledges_peer_messages(
        &FromAgent::ResponseEnd {
            response_id: "partial".to_string(),
            usage: None,
        }
    ));
    assert!(!native_chat_acknowledges_peer_messages(
        &FromAgent::ProviderError {
            kind: maestro_local_host::ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
            message: "authentication failed".to_string(),
        }
    ));
    assert!(!native_chat_acknowledges_peer_messages(
        &FromAgent::TurnInterrupted {
            response_id: "interrupted".to_string(),
            reason: "stream dropped".to_string(),
        }
    ));
}

#[test]
fn native_chat_successful_terminal_acknowledges_peer_messages() {
    assert!(native_chat_acknowledges_peer_messages(
        &FromAgent::TurnCompleted {
            response_id: "completed".to_string(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        }
    ));
}

#[tokio::test]
async fn codemode_progress_uses_existing_status_wire_on_sse_and_websocket() {
    use maestro_runtime::ExecutionStatus;
    use maestro_runtime::agent::protocol::CodeModeChildProgress;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    let children = [
        Some(ExecutionStatus::Succeeded),
        None,
        Some(ExecutionStatus::Denied),
    ]
    .into_iter()
    .map(|status| CodeModeChildProgress {
        call_id: "private-child-id".into(),
        tool: "private-tool-name".into(),
        status,
        duration_ms: Some(12),
    })
    .collect::<Vec<_>>();
    let event = FromAgent::CodeModeProgress {
        call_id: "parent".into(),
        children: children.clone(),
    };
    assert!(native_chat_terminal_status(&event).is_none());
    assert!(!native_chat_acknowledges_peer_messages(&event));
    let payload = super::codemode_progress_status(&children);
    assert_eq!(payload["type"], "status");
    assert_eq!(
        payload["status"],
        "Script · 1 completed · 1 running or waiting · 1 need attention"
    );
    assert_eq!(payload["details"], serde_json::json!({}));
    assert!(!payload.to_string().contains("private"));

    for websocket in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let mut writer = TcpStream::connect(listener.local_addr().expect("address"))
            .await
            .expect("writer");
        let (mut reader, _) = listener.accept().await.expect("reader");
        if websocket {
            super::send_ws_json(&mut writer, &payload)
                .await
                .expect("WebSocket status");
        } else {
            super::send_sse(&mut writer, &payload)
                .await
                .expect("SSE status");
        }
        writer.shutdown().await.expect("finish frame");
        let mut wire = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_to_end(&mut wire),
        )
        .await
        .expect("status read deadline")
        .expect("status read");
        let json = if websocket {
            assert_eq!(wire[0], 0x81);
            let offset = match wire[1] {
                126 => 4,
                127 => 10,
                _ => 2,
            };
            &wire[offset..]
        } else {
            assert!(wire.starts_with(b"data: ") && wire.ends_with(b"\n\n"));
            &wire[6..wire.len() - 2]
        };
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(json).expect("status JSON"),
            payload
        );
    }
}
