//! Tests for A2A task wire-format parsing and terminal-state classification.
//!
//! `A2ATask` is deserialized directly from a remote peer's HTTP response, so
//! coverage here favors malformed/adversarial shapes (missing required
//! fields, wrong types, unexpected response envelopes) over the happy path.

use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sample_task_json() -> Value {
    json!({
        "id": "task-1",
        "contextId": "ctx-1",
        "status": {
            "state": "TASK_STATE_COMPLETED",
            "message": {
                "messageId": "msg-1",
                "role": "agent",
                "parts": [{"text": "done", "mediaType": "text/plain"}],
            },
            "timestamp": "2026-07-21T00:00:00.000Z",
        },
        "artifacts": [
            {"artifactId": "artifact-1", "parts": [{"text": "artifact text"}]},
        ],
        "history": [
            {"role": "user", "parts": [{"text": "please do the thing"}]},
            {"role": "agent", "parts": [{"text": "history reply"}]},
        ],
        "metadata": {"workGraph": {"nodes": []}},
    })
}

fn working_task() -> String {
    json!({"id":"task-1","status":{"state":"TASK_STATE_WORKING"}}).to_string()
}

fn completed_task() -> String {
    json!({"id":"task-1","status":{"state":"TASK_STATE_COMPLETED"}}).to_string()
}

fn test_config(base_url: String) -> A2AServiceConfig {
    A2AServiceConfig {
        base_url,
        token: Some("test-token".into()),
        organization_id: Some("org-1".into()),
        workspace_id: Some("workspace-1".into()),
        agent_id: Some("agent-1".into()),
        session_id: Some("session-1".into()),
        actor_id: Some("actor-1".into()),
        timeout_ms: 1_000,
        max_attempts: 1,
    }
}

async fn scripted_server(responses: Vec<Vec<Vec<u8>>>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let size = socket.read(&mut chunk).await.expect("read request");
                assert!(size > 0, "request ended before its headers completed");
                request.extend_from_slice(&chunk[..size]);
                assert!(
                    request.len() <= 64 * 1024,
                    "test request headers are too large"
                );
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            captured
                .lock()
                .expect("request lock")
                .push(String::from_utf8_lossy(&request).to_string());
            for chunk in response {
                socket
                    .write_all(&chunk)
                    .await
                    .expect("write response chunk");
                tokio::task::yield_now().await;
            }
        }
    });
    (format!("http://{address}"), requests)
}

fn json_response(body: String) -> Vec<Vec<u8>> {
    vec![format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()]
}

#[tokio::test]
async fn wait_for_task_consumes_split_multiline_sse_with_headers() {
    let event = concat!(
        ": heartbeat\r\n",
        "id: event-1\r\n",
        "data: {\"statusUpdate\":{\"taskId\":\"task-1\",\r\n",
        "data: \"status\":{\"state\":\"TASK_STATE_COMPLETED\"}}}\r\n\r\n"
    );
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        event.len()
    );
    let split = event.len() / 2;
    let (base_url, requests) = scripted_server(vec![
        json_response(working_task()),
        vec![
            header.into_bytes(),
            event.as_bytes()[..split].to_vec(),
            event.as_bytes()[split..].to_vec(),
        ],
    ])
    .await;

    let task = wait_for_task(&test_config(base_url), "task-1", 10_000, 10)
        .await
        .expect("terminal stream update");
    assert!(is_completed_state(&task.status.state));
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    let subscribe = requests
        .last()
        .expect("subscription request")
        .to_ascii_lowercase();
    assert!(subscribe.contains("get /tasks/task-1:subscribe"));
    assert!(subscribe.contains("authorization: bearer test-token"));
    assert!(subscribe.contains("x-organization-id: org-1"));
    assert!(subscribe.contains("x-evalops-workspace-id: workspace-1"));
}

#[tokio::test]
async fn wait_for_task_polls_only_when_subscription_is_unsupported() {
    let unsupported = vec![
        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
    ];
    let (base_url, requests) = scripted_server(vec![
        json_response(working_task()),
        unsupported,
        json_response(completed_task()),
    ])
    .await;
    let task = wait_for_task(&test_config(base_url), "task-1", 10_000, 1)
        .await
        .expect("polling fallback");
    assert!(is_completed_state(&task.status.state));
    assert_eq!(requests.lock().expect("requests").len(), 3);
}

#[tokio::test]
async fn wait_for_task_reconciles_clean_disconnect_and_reconnects() {
    let empty_stream = vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()];
    let completed_event =
        "data: {\"task\":{\"id\":\"task-1\",\"status\":{\"state\":\"TASK_STATE_COMPLETED\"}}}\n\n";
    let complete_stream = vec![format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{completed_event}",
        completed_event.len()
    ).into_bytes()];
    let (base_url, requests) = scripted_server(vec![
        json_response(working_task()),
        empty_stream,
        json_response(working_task()),
        complete_stream,
    ])
    .await;
    let task = wait_for_task(&test_config(base_url), "task-1", 30_000, 1)
        .await
        .expect("reconnected subscription");
    assert!(is_completed_state(&task.status.state));
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests.iter().filter(|r| r.contains(":subscribe")).count(),
        2
    );
}

#[test]
fn sse_parser_rejects_oversized_events() {
    let mut parser = SseParser::default();
    let event = vec![b'x'; A2A_SSE_EVENT_LIMIT_BYTES + 1];
    let error = parser.push(&event).expect_err("oversized event must fail");
    assert!(error.to_string().contains("exceeded"));
}

#[test]
fn a2a_task_round_trips_full_shape() {
    let task: A2ATask = serde_json::from_value(sample_task_json()).expect("deserialize task");
    assert_eq!(task.id, "task-1");
    assert_eq!(task.context_id.as_deref(), Some("ctx-1"));
    assert_eq!(task.status.state, "TASK_STATE_COMPLETED");
    assert_eq!(task.artifacts.as_ref().map(Vec::len), Some(1));
    assert_eq!(task.history.as_ref().map(Vec::len), Some(2));
    assert!(task.metadata.is_some());
}

#[test]
fn a2a_task_deserializes_with_only_required_fields() {
    let value = json!({"id": "task-2", "status": {"state": "TASK_STATE_WORKING"}});
    let task: A2ATask = serde_json::from_value(value).expect("minimal task must deserialize");
    assert_eq!(task.id, "task-2");
    assert_eq!(task.status.state, "TASK_STATE_WORKING");
    assert!(task.context_id.is_none());
    assert!(task.artifacts.is_none());
    assert!(task.history.is_none());
    assert!(task.metadata.is_none());
    assert!(task.status.message.is_none());
    assert!(task.status.timestamp.is_none());
}

#[test]
fn a2a_task_rejects_missing_id() {
    let value = json!({"status": {"state": "TASK_STATE_WORKING"}});
    let err = serde_json::from_value::<A2ATask>(value)
        .expect_err("task without an id must fail to parse");
    assert!(err.to_string().contains("id"), "{err}");
}

#[test]
fn a2a_task_rejects_missing_status() {
    let value = json!({"id": "task-3"});
    let err = serde_json::from_value::<A2ATask>(value)
        .expect_err("task without a status must fail to parse");
    assert!(err.to_string().contains("status"), "{err}");
}

#[test]
fn a2a_task_rejects_wrong_type_for_state() {
    let value = json!({"id": "task-4", "status": {"state": 42}});
    let err = serde_json::from_value::<A2ATask>(value)
        .expect_err("numeric state must fail to parse (state is a String)");
    assert!(
        err.to_string().contains("invalid type") && err.to_string().contains("expected a string"),
        "{err}"
    );
}

#[test]
fn extract_task_text_prefers_status_message() {
    let task: A2ATask = serde_json::from_value(sample_task_json()).expect("task");
    assert_eq!(extract_task_text(&task).as_deref(), Some("done"));
}

#[test]
fn extract_task_text_falls_back_to_artifacts() {
    let mut value = sample_task_json();
    value["status"]["message"] = Value::Null;
    let task: A2ATask = serde_json::from_value(value).expect("task");
    assert_eq!(extract_task_text(&task).as_deref(), Some("artifact text"));
}

#[test]
fn extract_task_text_falls_back_to_last_agent_history_entry() {
    let mut value = sample_task_json();
    value["status"]["message"] = Value::Null;
    value["artifacts"] = json!([]);
    let task: A2ATask = serde_json::from_value(value).expect("task");
    assert_eq!(extract_task_text(&task).as_deref(), Some("history reply"));
}

#[test]
fn extract_task_text_returns_none_when_nothing_matches() {
    let value = json!({"id": "task-5", "status": {"state": "TASK_STATE_WORKING"}});
    let task: A2ATask = serde_json::from_value(value).expect("task");
    assert_eq!(extract_task_text(&task), None);
}

#[test]
fn parse_send_result_accepts_wrapped_task_envelope() {
    let response = json!({"task": sample_task_json()});
    let result = parse_send_result(response).expect("wrapped task must parse");
    assert_eq!(result.task.id, "task-1");
}

#[test]
fn parse_send_result_accepts_bare_task_shape() {
    let response = sample_task_json();
    let result = parse_send_result(response).expect("bare id+status shape must parse");
    assert_eq!(result.task.id, "task-1");
}

#[test]
fn parse_send_result_accepts_result_envelope() {
    let response = json!({"result": sample_task_json()});
    let result = parse_send_result(response).expect("result-wrapped task must parse");
    assert_eq!(result.task.id, "task-1");
}

#[test]
fn parse_send_result_rejects_response_without_a_task() {
    let response = json!({"unrelated": "value"});
    let err =
        parse_send_result(response).expect_err("response without a task shape must be rejected");
    assert!(err.to_string().contains("did not include a task"), "{err}");
}

#[test]
fn state_classification_recognizes_known_terminal_states() {
    assert!(is_completed_state("COMPLETED"));
    assert!(is_completed_state("completed"));
    assert!(is_completed_state("TASK_STATE_COMPLETED"));
    assert!(is_completed_state("Succeeded"));

    assert!(is_failed_state("FAILED"));
    assert!(is_failed_state("CANCELLED"));
    assert!(is_failed_state("TASK_STATE_CANCELED"));

    assert!(is_action_required_state("INPUT_REQUIRED"));
    assert!(is_action_required_state("TASK_STATE_AUTH_REQUIRED"));

    assert!(is_terminal_state("TASK_STATE_COMPLETED"));
    assert!(is_terminal_state("FAILED"));
    assert!(is_terminal_state("INPUT_REQUIRED"));
    assert!(!is_terminal_state("TASK_STATE_WORKING"));
    assert!(!is_terminal_state("SUBMITTED"));
}

#[test]
fn state_classification_does_not_confuse_working_with_terminal() {
    assert!(!is_completed_state("WORKING"));
    assert!(!is_failed_state("WORKING"));
    assert!(!is_action_required_state("WORKING"));
}

/// Negated and partial peer states must not end a wait or report success.
#[test]
fn is_completed_state_does_not_false_positive_on_negated_state() {
    for state in [
        "NOT_COMPLETED",
        "NOT_FAILED",
        "NO_AUTH_REQUIRED",
        "NOT_REJECTED",
        "TASK_STATE_NOT_COMPLETED",
        "TASK_STATE_NO_AUTH_REQUIRED",
        "UNKNOWN_COMPLETED",
        "TASK_STATE_TASK_STATE_COMPLETED",
        "STATE_COMPLETED",
        "REQUIRED",
        "COMPLETED_EXTRA",
        "",
    ] {
        assert!(!is_completed_state(state), "unexpected completion: {state}");
        assert!(!is_failed_state(state), "unexpected failure: {state}");
        assert!(
            !is_action_required_state(state),
            "unexpected action: {state}"
        );
        assert!(!is_final_state(state), "unexpected final state: {state}");
        assert!(
            !is_terminal_state(state),
            "unexpected terminal state: {state}"
        );
    }
}

#[test]
fn state_classification_preserves_aliases_and_wire_enum_prefix() {
    type StateClassifier = fn(&str) -> bool;
    let groups: &[(StateClassifier, &[&str])] = &[
        (is_completed_state, &["COMPLETED", "SUCCEEDED", "SUCCESS"]),
        (
            is_failed_state,
            &["FAILED", "CANCELED", "CANCELLED", "REJECTED"],
        ),
        (
            is_action_required_state,
            &["INPUT_REQUIRED", "AUTH_REQUIRED"],
        ),
    ];
    for (classify, states) in groups {
        for state in *states {
            for variant in [
                state.to_string(),
                format!("TASK_STATE_{state}"),
                format!("  {}  ", state.to_ascii_lowercase()),
                state.to_ascii_lowercase().replace('_', "-"),
                format!(
                    "task state {}",
                    state.to_ascii_lowercase().replace('_', " ")
                ),
            ] {
                assert!(classify(&variant), "unrecognized state: {variant}");
                assert!(is_terminal_state(&variant), "nonterminal state: {variant}");
            }
        }
    }
}

#[tokio::test]
async fn wait_for_task_keeps_polling_after_negated_states() {
    for state in ["NOT_COMPLETED", "NOT_FAILED", "NO_AUTH_REQUIRED"] {
        let pending = json!({"id": "task-1", "status": {"state": state}}).to_string();
        let unsupported = vec![
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ];
        let (base_url, requests) = scripted_server(vec![
            json_response(pending.clone()),
            unsupported,
            json_response(pending),
            json_response(completed_task()),
        ])
        .await;
        let task = wait_for_task(&test_config(base_url), "task-1", 30_000, 1)
            .await
            .expect("wait for actual completion");
        assert_eq!(
            task.status.state, "TASK_STATE_COMPLETED",
            "premature return for {state}"
        );
        assert_eq!(requests.lock().expect("requests").len(), 4);
    }
}
