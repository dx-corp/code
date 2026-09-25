#![cfg(test)]

use super::{FromAgent, NativeAgent, NativeAgentConfig};
use crate::ai::{ContentBlock, Message, MessageContent, Role, StopReason, UnifiedClient};
use crate::state::ApprovalMode;
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn fixture_event_kind(event: &FromAgent) -> &'static str {
    match event {
        FromAgent::ConversationSnapshot { .. } => "conversation_snapshot",
        FromAgent::Error { .. } => "error",
        FromAgent::ProviderError { .. } => "provider_error",
        FromAgent::ResponseStart { .. } => "response_start",
        FromAgent::ResponseChunk { .. } => "response_chunk",
        FromAgent::ResponseEnd { .. } => "response_end",
        FromAgent::TurnCompleted { .. } => "turn_completed",
        FromAgent::TurnInterrupted { .. } => "turn_interrupted",
        FromAgent::ToolCall { .. } => "tool_call",
        FromAgent::ToolStart { .. } => "tool_start",
        FromAgent::ToolEnd { .. } => "tool_end",
        _ => "other",
    }
}

fn configure_codex_fixture_identity() {
    crate::credential_mode::install_test_identity_env();
}

struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

impl EnvRestore {
    fn capture(names: &[&'static str]) -> Self {
        Self(
            names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        )
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn append_codex_tool_use(
    messages: &mut Vec<Message>,
    call_id: &str,
    tool_name: &str,
    input: Value,
) {
    messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: call_id.to_owned(),
            name: tool_name.to_owned(),
            input,
            gemini_context: None,
        }]),
    });
}

fn append_codex_tool_result(
    messages: &mut Vec<Message>,
    call_id: &str,
    content: String,
    is_error: bool,
) {
    messages.push(Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: call_id.to_owned(),
            content,
            is_error: Some(is_error),
        }]),
    });
}

async fn read_scripted_provider_request(stream: &mut tokio::net::TcpStream) -> Value {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "provider request closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header end");
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .find_map(|(_, value)| value.trim().parse::<usize>().ok())
        .expect("content length");
    let body_start = header_end + 4;
    while buffer.len() - body_start < content_length {
        let read = stream.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "provider request closed before body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&buffer[body_start..body_start + content_length])
        .expect("provider request json")
}

fn chat_sse_response(id: &str, content: &str, tool_call: bool) -> String {
    let mut events = vec![serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": content}, "finish_reason": null}]
    })];
    if tool_call {
        events.push(serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-4o", "choices": [{"index": 0,
                "delta": {"tool_calls": [{"index": 0, "id": "call-native-1", "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"Cargo.toml\"}"}}]},
                "finish_reason": "tool_calls"}]
        }));
    } else {
        events.push(serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-4o", "choices": [{"index": 0,
                "delta": {}, "finish_reason": "stop"}]
        }));
    }
    let mut body = String::new();
    for event in events {
        write!(body, "data: {event}\n\n").expect("write SSE event");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn chat_sse_tool_response(id: &str, name: &str, arguments: &str) -> String {
    let start = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
    });
    let tool = serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": 0,
        "model": "gpt-4o", "choices": [{"index": 0,
            "delta": {"tool_calls": [{"index": 0, "id": "call-search-1", "type": "function",
                "function": {"name": name, "arguments": arguments}}]},
            "finish_reason": "tool_calls"}]
    });
    format!("data: {start}\n\ndata: {tool}\n\ndata: [DONE]\n\n")
}

async fn scripted_native_provider() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        for (index, response) in [
            chat_sse_response("first-tool", "I will read it.", true),
            chat_sse_response("first-final", "The first turn is complete.", false),
            chat_sse_response("second-final", "Continuation observed.", false),
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, _) = listener.accept().await.expect("provider accept");
            let request = read_scripted_provider_request(&mut stream).await;
            captured.lock().unwrap().push(request);
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            stream
                .write_all(wire.as_bytes())
                .await
                .expect("provider response");
            if index == 2 {
                break;
            }
        }
    });
    (format!("http://{address}/v1"), requests)
}

async fn scripted_single_turn_provider() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("provider accept");
        let request = read_scripted_provider_request(&mut stream).await;
        captured.lock().unwrap().push(request);
        let response = chat_sse_response("hosted-fast-final", "The hosted turn completed.", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream
            .write_all(wire.as_bytes())
            .await
            .expect("provider response");
    });
    (format!("http://{address}/v1"), requests)
}

async fn scripted_rlm_search_provider() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        for response in [
            chat_sse_tool_response(
                "hosted-fast-search",
                "tool_search",
                "{\"names\":[\"set_rlm_context\"]}",
            ),
            chat_sse_response("hosted-fast-after-search", "Search stayed bounded.", false),
        ] {
            let (mut stream, _) = listener.accept().await.expect("provider accept");
            let request = read_scripted_provider_request(&mut stream).await;
            captured.lock().unwrap().push(request);
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            stream
                .write_all(wire.as_bytes())
                .await
                .expect("provider response");
        }
    });
    (format!("http://{address}/v1"), requests)
}

async fn receive_codex_fixture_snapshot(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<FromAgent>,
) -> (Vec<Message>, Vec<String>, Vec<String>) {
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut errors = Vec::new();
        let mut statuses = Vec::new();
        let mut snapshot = None;
        loop {
            match events.recv().await {
                Some(FromAgent::Error {
                    message, terminal, ..
                }) => {
                    errors.push(message);
                    if terminal {
                        break;
                    }
                }
                Some(FromAgent::ProviderError { message, .. }) => {
                    errors.push(message);
                    break;
                }
                Some(FromAgent::Status { message }) => statuses.push(message),
                Some(FromAgent::ConversationSnapshot { messages, .. }) => {
                    snapshot = Some(messages);
                }
                Some(FromAgent::TurnCompleted { .. } | FromAgent::TurnInterrupted { .. }) => {
                    break;
                }
                Some(_) => {}
                None => panic!("fixture closed before terminal boundary"),
            }
        }
        (
            snapshot.expect("semantic snapshot must precede the terminal boundary"),
            errors,
            statuses,
        )
    })
    .await
    .expect("fixture terminal timeout")
}

#[tokio::test]
async fn selective_summary_failure_keeps_original_and_rejects_incomplete_output() {
    use crate::ai::{ScriptedBlock, ScriptedResponse};
    for response in [
        ScriptedResponse {
            blocks: vec![ScriptedBlock::Text("partial".into())],
            stop_reason: StopReason::MaxTokens,
            error: None,
        },
        ScriptedResponse {
            blocks: vec![ScriptedBlock::Text("partial".into())],
            stop_reason: StopReason::EndTurn,
            error: Some("provider rejected test-secret".into()),
        },
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "tool".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"touch forbidden"}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
    ] {
        let reports_usage = !matches!(response.stop_reason, StopReason::ToolUse);
        let harness = super::harness::AgentHarness::with_scripted(vec![response]).unwrap();
        harness
            .agent
            .replace_history_preserving_credentials(vec![Message {
                role: Role::User,
                content: MessageContent::text("retain original"),
            }]);
        let preview = harness
            .agent
            .start_selective_summary_preview()
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let request = harness
            .agent
            .start_selective_summary(
                super::RangeSelection::FromTurn(1),
                preview.history_digest.clone(),
            )
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), request.receiver)
            .await
            .unwrap()
            .unwrap();
        if reports_usage {
            assert!(
                outcome.usage.is_some(),
                "failed summary must settle reported usage"
            );
        }
        let error = outcome.result.unwrap_err().to_string();
        assert!(!error.contains("test-secret"));
        assert_eq!(
            harness
                .agent
                .start_selective_summary_preview()
                .unwrap()
                .await
                .unwrap()
                .unwrap()
                .history_digest,
            preview.history_digest
        );
        assert!(!harness.workspace.path().join("forbidden").exists());
        harness.agent.shutdown().await;
    }
}

#[tokio::test]
async fn selective_summary_precancel_preserves_history_without_provider_request() {
    let harness = super::harness::AgentHarness::with_scripted(vec![]).unwrap();
    harness
        .agent
        .replace_history_preserving_credentials(vec![Message {
            role: Role::User,
            content: MessageContent::text("retain me"),
        }]);
    let preview = harness
        .agent
        .start_selective_summary_preview()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    let request = harness
        .agent
        .start_selective_summary(
            super::RangeSelection::FromTurn(1),
            preview.history_digest.clone(),
        )
        .unwrap();
    request.cancellation.cancel();
    let outcome = request.receiver.await.unwrap();
    assert!(
        outcome
            .result
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(outcome.usage.is_none());
    assert_eq!(
        harness
            .agent
            .start_selective_summary_preview()
            .unwrap()
            .await
            .unwrap()
            .unwrap()
            .history_digest,
        preview.history_digest
    );
    harness.agent.shutdown().await;
}

#[tokio::test]
async fn hosted_hard_kill_at_terminal_restores_prior_user_and_assistant_turn() {
    let (base_url, requests) = scripted_native_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .expect("fixture file");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let source_client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url.clone())
            .expect("owner-1 client"),
    );
    let (source, mut source_events) =
        NativeAgent::new_with_test_client(config.clone(), source_client).expect("owner-1");
    source
        .prompt("owner-1-user-sentinel".to_owned(), vec![])
        .await
        .expect("owner-1 prompt");

    let sessions = tempfile::tempdir().expect("sessions");
    let mut recorder =
        crate::headless::SessionRecorder::new(sessions.path()).expect("session recorder");
    // Model the hosted owner being killed as soon as the public response
    // terminal is visible. Only semantic snapshots observed before that
    // boundary can be durable input for owner 2.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match source_events.recv().await {
                Some(FromAgent::ConversationSnapshot {
                    protocol_version,
                    messages,
                    ..
                }) => recorder
                    .record_received(
                        &crate::headless::messages::FromAgentMessage::ConversationSnapshot {
                            protocol_version,
                            messages,
                            processed_queue_ids: vec![],
                        },
                    )
                    .expect("persist owner-1 snapshot"),
                Some(FromAgent::ResponseEnd { response_id, .. }) if response_id == "done" => {
                    break;
                }
                Some(FromAgent::Error { message, .. }) => {
                    panic!("owner-1 failed: {message}")
                }
                Some(FromAgent::ProviderError { message, .. }) => {
                    panic!("owner-1 provider failed: {message}")
                }
                Some(_) => {}
                None => panic!("owner-1 closed before public terminal"),
            }
        }
    })
    .await
    .expect("owner-1 terminal timeout");
    let session_id = recorder.id().to_owned();
    drop(recorder);
    // Do not consume any event after the public terminal: this is the
    // process-kill boundary the hosted lifecycle exposes to its owner.
    drop(source_events);
    source.shutdown().await;

    let restored_history = crate::headless::SessionRecorder::resume(sessions.path(), &session_id)
        .expect("resume owner-1 checkpoint")
        .replay()
        .semantic_conversation
        .expect("owner-1 semantic checkpoint must predate its terminal");
    let restored_client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("owner-2 client"),
    );
    let (restored, mut restored_events) =
        NativeAgent::new_with_test_client(config, restored_client).expect("owner-2");
    restored.replace_history(restored_history);
    restored
        .prompt("owner-2-user-sentinel".to_owned(), vec![])
        .await
        .expect("owner-2 prompt");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = restored_events.recv().await {
            if matches!(event, FromAgent::ResponseEnd { .. }) {
                break;
            }
        }
    })
    .await
    .expect("owner-2 terminal timeout");
    restored.shutdown().await;

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 3);
    let owner_2_request =
        serde_json::to_string(&captured[2]).expect("owner-2 provider request json");
    assert!(
        owner_2_request.contains("owner-1-user-sentinel"),
        "owner-2 provider body lost the prior user turn: {owner_2_request}"
    );
    assert!(
        owner_2_request.contains("The first turn is complete."),
        "owner-2 provider body lost the prior assistant turn: {owner_2_request}"
    );
}

#[tokio::test]
async fn hosted_fast_tool_search_cannot_reactivate_rlm_mutations() {
    let (base_url, requests) = scripted_rlm_search_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("scripted client"),
    );
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("hosted agent");

    agent
        .prompt(
            "Find the requested capability, then answer.".to_owned(),
            vec![],
        )
        .await
        .expect("hosted prompt");

    let mut approval_requested = false;
    let mut completed = false;
    let mut observed = Vec::new();
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            observed.push(format!("{event:?}"));
            match event {
                FromAgent::ToolCall {
                    requires_approval: true,
                    ..
                } => approval_requested = true,
                FromAgent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                FromAgent::Error { message, .. } => panic!("hosted turn failed: {message}"),
                FromAgent::ProviderError { message, .. } => {
                    panic!("provider turn failed: {message}")
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(terminal.is_ok(), "hosted turn timeout: {observed:#?}");
    agent.shutdown().await;

    assert!(completed);
    assert!(!approval_requested);
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    let second_request = serde_json::to_string(&captured[1]).expect("second request JSON");
    assert!(
        second_request.contains("No tools matched"),
        "{second_request}"
    );
    let advertised = captured[1]["tools"]
        .as_array()
        .expect("OpenAI tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<HashSet<_>>();
    assert!(!advertised.contains("set_rlm_context"));
}

#[tokio::test]
async fn injected_user_note_is_consumed_only_after_a_successful_turn() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("provider accept");
        let _request = read_scripted_provider_request(&mut stream).await;
        let response = chat_sse_response("note-consumption", "Done.", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream
            .write_all(wire.as_bytes())
            .await
            .expect("provider response");
    });

    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1"))
            .expect("test client"),
    );
    let (agent, _events) = NativeAgent::new_with_test_client(config, client).expect("native agent");
    let (applied, consumed) = agent
        .inject_user_note("Subagent child-1 completed.")
        .expect("queue user note");
    applied.await.expect("agent note application");

    agent
        .prompt("Use the completion note.".to_string(), Vec::new())
        .await
        .expect("start turn");
    tokio::time::timeout(std::time::Duration::from_secs(5), consumed)
        .await
        .expect("agent note consumption timeout")
        .expect("agent note consumption acknowledgement");

    agent.shutdown().await;
}

#[tokio::test]
async fn native_provider_checkpoint_survives_process_death_and_restores_tool_continuity() {
    let (base_url, requests) = scripted_native_provider().await;
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .expect("fixture file");
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let source_client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url.clone())
            .expect("source client"),
    );
    let (source, mut source_events) =
        NativeAgent::new_with_test_client(config.clone(), source_client).expect("source agent");
    source
        .prompt("first turn".to_owned(), vec![])
        .await
        .expect("source prompt");

    let sessions = tempfile::tempdir().expect("sessions");
    let mut recorder =
        crate::headless::SessionRecorder::new(sessions.path()).expect("session recorder");
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match source_events.recv().await {
                Some(FromAgent::ConversationSnapshot { messages, .. }) => break messages,
                Some(FromAgent::Error { message, .. }) => {
                    panic!("fixture agent error: {message}")
                }
                Some(_) => continue,
                None => panic!("source event channel closed before snapshot"),
            }
        }
    })
    .await
    .expect("runtime snapshot timeout");
    recorder
        .record_received(
            &crate::headless::messages::FromAgentMessage::ConversationSnapshot {
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_owned(),
                messages: snapshot,
                processed_queue_ids: vec![],
            },
        )
        .expect("persist runtime snapshot");
    let session_id = recorder.id().to_owned();
    drop(recorder);
    source.shutdown().await;

    let restored_history = crate::headless::SessionRecorder::resume(sessions.path(), &session_id)
        .expect("resume runtime checkpoint")
        .replay()
        .semantic_conversation
        .expect("restored semantic history");
    let restored_client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).expect("restored client"),
    );
    let (restored, mut restored_events) =
        NativeAgent::new_with_test_client(config, restored_client).expect("restored agent");
    restored.replace_history(restored_history);
    restored
        .prompt("second turn".to_owned(), vec![])
        .await
        .expect("second prompt");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = restored_events.recv().await {
            if matches!(event, FromAgent::ConversationSnapshot { .. }) {
                break;
            }
        }
    })
    .await
    .expect("second terminal snapshot timeout");
    restored.shutdown().await;

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 3);
    let continuation = serde_json::to_string(&captured[2]).expect("continuation request json");
    assert!(continuation.contains("first turn"), "{continuation}");
    assert!(continuation.contains("call-native-1"), "{continuation}");
    assert!(continuation.contains("second turn"), "{continuation}");
}

#[tokio::test]
async fn codex_process_continuation_fixture() {
    let Ok(role) = std::env::var("MAESTRO_CODEX_FIXTURE_ROLE") else {
        return;
    };
    configure_codex_fixture_identity();
    let workspace = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_WORKSPACE").expect("fixture workspace"),
    );
    let checkpoint = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_CHECKPOINT").expect("fixture checkpoint"),
    );
    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex fixture agent");
    if role == "restore" {
        let history =
            serde_json::from_slice(&std::fs::read(&checkpoint).expect("hydrated checkpoint"))
                .expect("checkpoint messages");
        agent.replace_history(history);
    } else if std::env::var_os("MAESTRO_CODEX_FIXTURE_OVERSIZED").is_some() {
        let mut history = vec![Message {
            role: Role::User,
            content: MessageContent::Text("older context".to_owned()),
        }];
        append_codex_tool_use(
            &mut history,
            "bounded-call-1",
            "read",
            serde_json::json!({ "payload": "x".repeat(500_000) }),
        );
        append_codex_tool_result(
            &mut history,
            "bounded-call-1",
            "bounded tool result".to_owned(),
            false,
        );
        agent.replace_history(history);
    }
    agent
        .prompt(
            if role == "source" {
                "first prompt".to_owned()
            } else {
                "second prompt".to_owned()
            },
            vec![],
        )
        .await
        .expect("fixture prompt");
    // This nested Rust-and-Node fixture verifies state preservation, not
    // startup latency. Keep its finite deadline above normal CI scheduling
    // lag while the parent workspace suite is running in parallel.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut observed_events = Vec::new();
    let snapshot = loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(FromAgent::ConversationSnapshot { messages, .. })) => break messages,
            Ok(Some(event)) => observed_events.push(fixture_event_kind(&event)),
            Ok(None) => panic!(
                "fixture closed before terminal snapshot; observed events: {observed_events:?}"
            ),
            Err(elapsed) => {
                panic!("fixture snapshot timeout ({elapsed}); observed events: {observed_events:?}")
            }
        }
    };
    if role == "source" {
        std::fs::write(
            &checkpoint,
            serde_json::to_vec(&snapshot).expect("snapshot json"),
        )
        .expect("persist runtime checkpoint");
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn codex_usage_notification_fixture() {
    let Ok(output_path) = std::env::var("MAESTRO_CODEX_USAGE_EVENTS") else {
        return;
    };
    configure_codex_fixture_identity();
    let workspace = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_WORKSPACE").expect("fixture workspace"),
    );
    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex usage fixture agent");
    agent
        .prompt("usage notification prompt".to_owned(), vec![])
        .await
        .expect("fixture prompt");

    let mut captured = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let event = events.recv().await.expect("fixture event");
            let done = matches!(event, FromAgent::ResponseEnd { .. });
            if matches!(
                event,
                FromAgent::CodexUsageState { .. }
                    | FromAgent::ResponseEnd { .. }
                    | FromAgent::ResponseChunk { .. }
            ) {
                captured.push(serde_json::to_value(&event).expect("event json"));
            }
            if done {
                break;
            }
        }
    })
    .await
    .expect("usage fixture timeout");
    std::fs::write(
        &output_path,
        serde_json::to_vec(&captured).expect("captured event json"),
    )
    .expect("write usage fixture events");
    agent.shutdown().await;
}

#[tokio::test]
async fn codex_cancel_lifecycle_fixture() {
    let Ok(output_path) = std::env::var("MAESTRO_CODEX_CANCEL_EVENTS") else {
        return;
    };
    configure_codex_fixture_identity();
    let workspace = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_WORKSPACE").expect("fixture workspace"),
    );
    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex cancel fixture agent");
    agent
        .prompt("cancel lifecycle prompt".to_owned(), vec![])
        .await
        .expect("fixture prompt");
    // The fixture exercises cancellation after acceptance, not Rust/Node
    // process startup latency. Wait for the peer's explicit acceptance
    // marker before starting the event-delivery deadline below.
    let accepted = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_CANCEL_MARKER").expect("acceptance marker"),
    );
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !accepted.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture peer should accept the turn during startup");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Some(FromAgent::CodexTurnState { state, .. }) if state == "accepted" => break,
                Some(_) => continue,
                None => panic!("fixture closed before accepting the turn"),
            }
        }
    })
    .await
    .expect("turn acceptance event");
    agent.cancel();

    let mut captured = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let event = events.recv().await.expect("fixture event");
            let done = matches!(event, FromAgent::ResponseEnd { .. });
            if matches!(
                event,
                FromAgent::CodexTurnState { .. }
                    | FromAgent::Status { .. }
                    | FromAgent::ResponseEnd { .. }
            ) {
                captured.push(serde_json::to_value(&event).expect("event json"));
            }
            if done {
                break;
            }
        }
    })
    .await
    .expect("cancel fixture timeout");
    std::fs::write(
        &output_path,
        serde_json::to_vec(&captured).expect("captured event json"),
    )
    .expect("write cancel fixture events");
    agent.shutdown().await;
}

#[tokio::test]
async fn codex_terminal_response_fixture() {
    let Ok(output_path) = std::env::var("MAESTRO_CODEX_TERMINAL_EVENTS") else {
        return;
    };
    configure_codex_fixture_identity();
    let workspace = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_WORKSPACE").expect("fixture workspace"),
    );
    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex terminal fixture agent");
    agent
        .prompt("terminal response prompt".to_owned(), vec![])
        .await
        .expect("fixture prompt");

    let mut captured = Vec::new();
    // Empty terminal responses enter bounded retry (1s + 2s + 4s backoff
    // plus jitter and per-attempt overhead) before failing closed, so the
    // capture window must cover the full retry budget.
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let event = events.recv().await.expect("fixture event");
            let done = match &event {
                FromAgent::TurnCompleted { .. } | FromAgent::TurnInterrupted { .. } => true,
                FromAgent::CodexTransportReceipt { outcome, .. } => outcome == "failed",
                _ => false,
            };
            captured.push(serde_json::to_value(&event).expect("event json"));
            if done {
                break;
            }
        }
    })
    .await
    .expect("terminal fixture timeout");
    std::fs::write(
        &output_path,
        serde_json::to_vec(&captured).expect("captured event json"),
    )
    .expect("write terminal fixture events");
    agent.shutdown().await;
}

#[tokio::test]
async fn codex_file_change_completion_before_approval_fixture() {
    let Ok(output_path) = std::env::var("MAESTRO_CODEX_FILE_CHANGE_EVENTS") else {
        return;
    };
    configure_codex_fixture_identity();
    let workspace = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_WORKSPACE").expect("fixture workspace"),
    );
    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex file-change fixture agent");
    agent
        .prompt(
            "use the file-change tool and then finish".to_owned(),
            vec![],
        )
        .await
        .expect("fixture prompt");

    let mut captured = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("fixture event");
            let done = matches!(event, FromAgent::ResponseEnd { .. });
            captured.push(serde_json::to_value(&event).expect("event json"));
            if done {
                break;
            }
        }
    })
    .await
    .expect("file-change fixture timeout");
    std::fs::write(
        &output_path,
        serde_json::to_vec(&captured).expect("events json"),
    )
    .expect("write file-change fixture events");
    agent.shutdown().await;
}

#[test]
fn queued_file_change_completion_is_drained_before_item_id_only_approval() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let changed_path = workspace.join("src.rs");
    std::fs::write(&changed_path, "before").expect("fixture source");
    let events_path = root.path().join("events.json");
    let script_log = root.path().join("app-server.log");
    let script = root.path().join("app-server.js");
    let changed_path_literal =
        serde_json::to_string(&changed_path.display().to_string()).expect("changed path literal");
    let script_log_literal =
        serde_json::to_string(&script_log.display().to_string()).expect("script log literal");
    let script_source = r"const rl = require('readline').createInterface({input: process.stdin});
const fs = require('fs');
const changedPath = __CHANGED_PATH__;
const log = __SCRIPT_LOG__;
function send(value) {
  fs.appendFileSync(log, `OUT ${JSON.stringify(value)}\n`);
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
function completedFileChange() {
  return {
method: 'item/completed',
params: {
  turnId: 'turn-causal',
  item: {
    id: 'item-write',
    type: 'fileChange',
    status: 'completed',
    changes: [{path: changedPath, kind: {type: 'update'}, content: 'patched'}]
  }
}
  };
}
rl.on('line', line => {
  fs.appendFileSync(log, `IN ${line}\n`);
  const message = JSON.parse(line);
  if (message.method === 'initialize') {
send({id: message.id, result: {protocolVersion: '2025-01-01', capabilities: {}}});
  } else if (message.method === 'model/list') {
send({id: message.id, result: {data: [{id: 'gpt-5.5', model: 'gpt-5.5', defaultReasoningEffort: 'medium', supportedReasoningEfforts: [{reasoningEffort: 'low'}, {reasoningEffort: 'medium'}, {reasoningEffort: 'high'}, {reasoningEffort: 'xhigh'}]}], nextCursor: null}});
  } else if (message.method === 'thread/start') {
send({id: message.id, result: {thread: {id: 'thread-causal'}}});
  } else if (message.method === 'turn/start') {
send({id: message.id, result: {turn: {id: 'turn-causal'}}});
setTimeout(() => {
  send({method: 'item/agentMessage/delta', params: {turnId: 'turn-causal', delta: 'before tool '}});
  // This notification is deliberately adjacent to the approval request.
  // The runtime must drain it before the item-id-only policy check.
  send(completedFileChange());
  send({
    id: 'approval-1',
    method: 'item/fileChange/requestApproval',
    params: {threadId: 'thread-causal', turnId: 'turn-causal', itemId: 'item-write'}
  });
}, 10);
  } else if (message.id === 'approval-1' && message.result) {
fs.appendFileSync(log, `DECISION ${JSON.stringify(message.result)}\n`);
setTimeout(() => {
  send({method: 'item/agentMessage/delta', params: {turnId: 'turn-causal', delta: 'after tool'}});
  send({method: 'turn/completed', params: {turnId: 'turn-causal'}});
}, 10);
  }
});
"
        .replace("__CHANGED_PATH__", &changed_path_literal)
        .replace("__SCRIPT_LOG__", &script_log_literal);
    std::fs::write(&script, script_source).expect("app-server script");

    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg("agent::native_codex_tests::codex_file_change_completion_before_approval_fixture")
        .arg("--exact")
        .arg("--nocapture")
        .env("MAESTRO_CODEX_FILE_CHANGE_EVENTS", &events_path)
        .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", &workspace)
        .env("MAESTRO_HOME", root.path().join("maestro-home"))
        .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
        .env("MAESTRO_TOOL_PROFILE", "all")
        .env("OPENAI_CODEX_TOKEN", "fixture-token")
        .env("RUST_BACKTRACE", "1")
        .env("RUST_MIN_STACK", "16777216")
        .env(
            "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
            serde_json::to_string(&vec![script.display().to_string()]).expect("script args"),
        )
        .output()
        .expect("spawn fixture child");
    assert!(
        output.status.success(),
        "file-change fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(&script_log).unwrap_or_default(),
    );
    let events: Vec<Value> =
        serde_json::from_slice(&std::fs::read(&events_path).expect("file-change events file"))
            .expect("file-change events json");
    let decision = events
        .iter()
        .find(|event| event["type"] == "codex_native_decision")
        .expect("native approval decision");
    assert_eq!(decision["method"], "item/fileChange/requestApproval");
    assert_eq!(
        decision["decision"], "approved_policy",
        "the queued completion path must make the item-id-only approval firewall-safe: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_end")
            .count(),
        1,
        "the approved completion must project one terminal tool receipt: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "response_chunk" && event["content"] == "before tool "
        })
    );
    assert!(
        events
            .iter()
            .any(|event| { event["type"] == "response_chunk" && event["content"] == "after tool" })
    );
    let log = std::fs::read_to_string(&script_log).expect("app-server log");
    assert!(
        log.contains(r#"DECISION {"decision":"accept"}"#),
        "Codex must receive an accepted approval after the causal completion drain: {log}"
    );
}

#[test]
fn codex_terminal_empty_response_fails_closed() {
    let root = tempfile::tempdir().expect("fixture root");
    let current = std::env::current_exe().expect("current test binary");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        r"const rl=require('readline').createInterface({input:process.stdin});
const mode=process.env.MAESTRO_CODEX_TERMINAL_MODE;
function send(x){process.stdout.write(JSON.stringify(x)+'\n')}
rl.on('line',line=>{const x=JSON.parse(line);
if(x.method==='initialize'){send({id:x.id,result:{protocolVersion:'2025-01-01',capabilities:{}}})}
else if(x.method==='model/list'){send({id:x.id,result:{data:[{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'medium'},{reasoningEffort:'high'},{reasoningEffort:'xhigh'}]}],nextCursor:null}})}
else if(x.method==='thread/start'){send({id:x.id,result:{thread:{id:'thread-terminal'}}})}
else if(x.method==='turn/start'){send({id:x.id,result:{turn:{id:'turn-terminal'}}});setTimeout(()=>{
  if(mode==='text'){send({method:'item/agentMessage/delta',params:{turnId:'turn-terminal',delta:'visible answer'}})}
  if(mode==='usage_limit'){
const usageMessage='You\'ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again later.';
const usageError={message:usageMessage,codexErrorInfo:'usageLimitExceeded',additionalDetails:null};
send({method:'error',params:{error:usageError,willRetry:false,threadId:'thread-terminal',turnId:'turn-terminal'}});
send({method:'turn/completed',params:{threadId:'thread-terminal',turn:{id:'turn-terminal',items:[],itemsView:'notLoaded',status:'failed',error:usageError}}});
return;
  }
  send({method:'turn/usage',params:{turnId:'turn-terminal',usage:{inputTokens:13,outputTokens:5}}})
  send({method:'turn/completed',params:{turnId:'turn-terminal'}})
},10)}
});",
    )
    .expect("app-server script");

    let run_child = |mode: &str| -> Vec<Value> {
        let dir = root.path().join(mode);
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).expect("fixture workspace");
        let events_path = dir.join("events.json");
        let output = std::process::Command::new(&current)
            .arg("agent::native_codex_tests::codex_terminal_response_fixture")
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_CODEX_TERMINAL_EVENTS", &events_path)
            .env("MAESTRO_CODEX_TERMINAL_MODE", mode)
            .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", &workspace)
            .env("MAESTRO_HOME", dir.join("maestro-home"))
            // Other in-process tests temporarily publish managed-policy paths.
            // This child owns a standalone fixture configuration, so inheriting
            // those paths races their cleanup and can block startup before the
            // terminal-response behavior under test is reached.
            .env_remove("MAESTRO_MANAGED_POLICY_PATH")
            .env_remove("MAESTRO_MANAGED_POLICY_STATE_PATH")
            .env_remove("MAESTRO_MANAGED_POLICY_PUBLIC_KEY")
            .env_remove("MAESTRO_MANAGED_POLICY_KEY_ID")
            .env_remove("MAESTRO_MANAGED_POLICY_AUDIT_PATH")
            .env_remove("MAESTRO_ENTERPRISE_POLICY_PATH")
            .env_remove("MAESTRO_POLICY_PATH")
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_MIN_STACK", "16777216")
            .env(
                "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                serde_json::to_string(&vec![script.display().to_string()]).expect("script args"),
            )
            .output()
            .expect("spawn fixture child");
        assert!(
            output.status.success(),
            "{mode} fixture failed: {}; stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        serde_json::from_slice(&std::fs::read(&events_path).expect("events file"))
            .expect("events json")
    };

    let empty_events = run_child("empty");
    let empty_errors: Vec<_> = empty_events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == "error")
        .collect();
    // Retry exhaustion must still fail closed with exactly one terminal
    // error naming the empty assistant response.
    assert_eq!(empty_errors.len(), 1, "{empty_events:?}");
    assert_eq!(empty_errors[0].1["terminal"], true);
    assert!(
        empty_errors[0].1["message"]
            .as_str()
            .is_some_and(|message| message.contains("empty_assistant_response"))
    );
    let failed_states: Vec<_> = empty_events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == "codex_turn_state" && event["state"] == "failed")
        .collect();
    // Empty terminal responses are transient (#3367): the driver retries
    // the turn up to the default attempt cap (1 initial + 3 retries)
    // before giving up.
    assert_eq!(failed_states.len(), 4, "{empty_events:?}");
    let retry_statuses: Vec<_> = empty_events
        .iter()
        .filter(|event| {
            event["type"] == "status"
                && event["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Retrying"))
        })
        .collect();
    assert_eq!(
        retry_statuses.len(),
        3,
        "each retry must be announced exactly once: {empty_events:?}"
    );
    assert_eq!(
        empty_events
            .iter()
            .filter(|event| {
                event["type"] == "codex_turn_state" && event["state"] == "completed"
            })
            .count(),
        0,
        "{empty_events:?}"
    );
    assert!(!empty_events.iter().any(|event| {
        event["type"] == "response_chunk" && !event["content"].as_str().unwrap_or("").is_empty()
    }));
    assert!(
        !empty_events
            .iter()
            .any(|event| event["type"] == "response_end")
    );
    let empty_usage: Vec<_> = empty_events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == "codex_usage_state")
        .collect();
    assert_eq!(empty_usage.len(), 4, "{empty_events:?}");
    for (_, usage) in &empty_usage {
        assert_eq!(usage["source"], "exact");
        assert_eq!(usage["usage"]["input_tokens"], 13);
        assert_eq!(usage["usage"]["output_tokens"], 5);
    }
    let failed_receipts: Vec<_> = empty_events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"] == "codex_transport_receipt" && event["outcome"] == "failed"
        })
        .collect();
    // The transport receipt is emitted once, when the turn fails closed
    // after retry exhaustion.
    assert_eq!(failed_receipts.len(), 1, "{empty_events:?}");
    let snapshot_index = empty_events
        .iter()
        .position(|event| event["type"] == "conversation_snapshot")
        .expect("failed turn semantic snapshot");
    // Every attempt reports usage before its turn fails, and the terminal
    // error lands only after the final failed attempt and its persistable
    // semantic checkpoint.
    for (usage, failed) in empty_usage.iter().zip(failed_states.iter()) {
        assert!(usage.0 < failed.0, "{empty_events:?}");
    }
    assert!(failed_states.last().expect("failed states").0 < empty_errors[0].0);
    assert!(snapshot_index < empty_errors[0].0, "{empty_events:?}");
    assert!(empty_errors[0].0 < failed_receipts[0].0);

    let text_events = run_child("text");
    assert_eq!(
        text_events
            .iter()
            .filter(|event| {
                event["type"] == "response_chunk" && event["content"] == "visible answer"
            })
            .count(),
        1,
        "{text_events:?}"
    );
    assert_eq!(
        text_events
            .iter()
            .filter(|event| {
                event["type"] == "codex_turn_state" && event["state"] == "completed"
            })
            .count(),
        1,
        "{text_events:?}"
    );
    assert!(!text_events.iter().any(|event| event["type"] == "error"));
    assert_eq!(
        text_events
            .iter()
            .filter(|event| { event["type"] == "response_end" && event["response_id"] == "done" })
            .count(),
        1,
        "{text_events:?}"
    );

    let usage_events = run_child("usage_limit");
    let usage_errors: Vec<_> = usage_events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == "error")
        .collect();
    assert_eq!(usage_errors.len(), 1, "{usage_events:?}");
    assert_eq!(usage_errors[0].1["terminal"], true);
    let usage_message = usage_errors[0].1["message"]
        .as_str()
        .expect("usage-limit error message");
    assert!(usage_message.contains("usage limit"), "{usage_message}");
    assert!(
        !usage_message.contains("empty_assistant_response"),
        "{usage_message}"
    );
    assert_eq!(
        usage_events
            .iter()
            .filter(|event| {
                event["type"] == "status"
                    && event["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("Retrying"))
            })
            .count(),
        0,
        "quota failures must not consume the empty-assistant retry budget: {usage_events:?}"
    );
    assert_eq!(
        usage_events
            .iter()
            .filter(|event| { event["type"] == "codex_turn_state" && event["state"] == "failed" })
            .count(),
        1,
        "{usage_events:?}"
    );
    assert_eq!(
        usage_events
            .iter()
            .filter(|event| {
                event["type"] == "codex_turn_state" && event["state"] == "completed"
            })
            .count(),
        0,
        "{usage_events:?}"
    );
    assert!(
        !usage_events
            .iter()
            .any(|event| event["type"] == "response_end")
    );
}

#[test]
fn codex_app_server_prompt_crosses_turn_start_once_after_pre_turn_restart() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let checkpoint = workspace.join("checkpoint.json");
    let script_log = root.path().join("app-server.log");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        format!(
            r"const rl=require('readline').createInterface({{input:process.stdin}});
const fs=require('fs'); const log='{}'; fs.appendFileSync(log,'started\n');
function send(x){{fs.appendFileSync(log,'OUT '+JSON.stringify(x)+'\n');process.stdout.write(JSON.stringify(x)+'\n')}}
rl.on('line', line=>{{fs.appendFileSync(log,line+'\n'); const x=JSON.parse(line);
if(x.method==='initialize'){{send({{id:x.id,result:{{protocolVersion:'2025-01-01',capabilities:{{methods:['thread/start','turn/start','turn/interrupt','thread/resume'],notifications:['item/tool/call','item/agentMessage/delta','turn/completed']}}}}}})}}
else if(x.method==='model/list'){{send({{id:x.id,result:{{data:[{{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{{reasoningEffort:'low'}},{{reasoningEffort:'medium'}},{{reasoningEffort:'high'}},{{reasoningEffort:'xhigh'}}]}}],nextCursor:null}}}})}}
else if(x.method==='thread/start'){{send({{id:x.id,result:{{thread:{{id:'thread-persisted'}}}}}})}}
else if(x.method==='thread/resume'){{send({{id:x.id,result:{{thread:{{id:x.params.threadId}}}}}})}}
else if(x.method==='thread/inject_items'){{send({{id:x.id,result:{{}}}})}}
else if(x.method==='turn/start'){{send({{id:x.id,result:{{turn:{{id:'turn'}}}}}});setTimeout(()=>{{send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'fixture answer'}}}});send({{method:'turn/completed',params:{{turnId:'turn'}}}})}},10)}}
}});",
            script_log.display(),
        ),
    )
    .expect("app-server script");

    let current = std::env::current_exe().expect("current test binary");
    let run_child = |role: &str| {
        let output = std::process::Command::new(&current)
            .arg("agent::native_codex_tests::codex_process_continuation_fixture")
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_CODEX_FIXTURE_ROLE", role)
            .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", &workspace)
            .env("MAESTRO_CODEX_FIXTURE_CHECKPOINT", &checkpoint)
            .env("MAESTRO_HOME", root.path().join("maestro-home"))
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_MIN_STACK", "16777216")
            .env(
                "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                serde_json::to_string(&vec![script.display().to_string()]).expect("script args"),
            )
            .output()
            .expect("spawn fixture child");
        assert!(
            output.status.success(),
            "{role} fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(&script_log).unwrap_or_default(),
        );
    };

    run_child("source");
    run_child("restore");

    let app_server_log = std::fs::read_to_string(&script_log).expect("app-server log");
    assert_eq!(
        app_server_log
            .matches("\"method\":\"thread/start\"")
            .count(),
        1,
        "restart should resume the persisted thread: {app_server_log}"
    );
    assert_eq!(
        app_server_log
            .matches("\"method\":\"thread/resume\"")
            .count(),
        1,
        "restart should issue exactly one resume: {app_server_log}"
    );
    assert_eq!(
        app_server_log
            .matches("\"method\":\"thread/inject_items\"")
            .count(),
        0,
        "successful resume must not inject restored history: {app_server_log}"
    );
    assert_eq!(
        app_server_log.matches("second prompt").count(),
        1,
        "the post-restart prompt must cross turn/start once: {app_server_log}"
    );
    assert_eq!(
        app_server_log.matches("first prompt").count(),
        1,
        "the restored prompt must not be replayed after resume: {app_server_log}"
    );
}

#[test]
fn codex_usage_notification_reaches_usage_state_and_response_end() {
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let events_path = root.path().join("events.json");
    let script_log = root.path().join("app-server.log");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        format!(
            r"const rl=require('readline').createInterface({{input:process.stdin}});
const fs=require('fs'); const log='{}'; fs.appendFileSync(log,'started\n');
function send(x){{fs.appendFileSync(log,'OUT '+JSON.stringify(x)+'\n');process.stdout.write(JSON.stringify(x)+'\n')}}
rl.on('line', line=>{{fs.appendFileSync(log,line+'\n'); const x=JSON.parse(line);
if(x.method==='initialize'){{send({{id:x.id,result:{{protocolVersion:'2025-01-01',capabilities:{{}}}}}})}}
else if(x.method==='model/list'){{send({{id:x.id,result:{{data:[{{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{{reasoningEffort:'low'}},{{reasoningEffort:'medium'}},{{reasoningEffort:'high'}},{{reasoningEffort:'xhigh'}}]}}],nextCursor:null}}}})}}
else if(x.method==='thread/start'){{send({{id:x.id,result:{{thread:{{id:'thread'}}}}}})}}
else if(x.method==='turn/start'){{send({{id:x.id,result:{{turn:{{id:'turn-usage'}}}}}});
setTimeout(()=>{{send({{method:'item/agentMessage/delta',params:{{turnId:'turn-usage',delta:'usage answer'}}}});
send({{method:'turn/usage',params:{{turnId:'turn-usage',usage:{{inputTokens:11,outputTokens:7,cacheWriteTokens:3,cost:0.02}}}}}});
send({{method:'turn/completed',params:{{turnId:'turn-usage'}}}});}},10)}}
}});",
            script_log.display(),
        ),
    )
    .expect("app-server script");

    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg("agent::native_codex_tests::codex_usage_notification_fixture")
        .arg("--exact")
        .arg("--nocapture")
        .env("MAESTRO_CODEX_USAGE_EVENTS", &events_path)
        .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", &workspace)
        .env("MAESTRO_HOME", root.path().join("maestro-home"))
        .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
        .env("OPENAI_CODEX_TOKEN", "fixture-token")
        .env("RUST_BACKTRACE", "1")
        .env("RUST_MIN_STACK", "16777216")
        .env(
            "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
            serde_json::to_string(&vec![script.display().to_string()]).expect("script args"),
        )
        .output()
        .expect("spawn fixture child");
    assert!(
        output.status.success(),
        "usage fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(&script_log).unwrap_or_default(),
    );
    let events: Vec<Value> =
        serde_json::from_slice(&std::fs::read(&events_path).expect("events file"))
            .expect("events json");
    let requests = std::fs::read_to_string(&script_log).expect("app-server requests");
    let turn: Value = requests
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|request| request["method"] == "turn/start")
        .expect("native turn request");
    assert_eq!(turn["params"]["effort"], "medium");
    let usage_state = events
        .iter()
        .find(|event| event["type"] == "codex_usage_state")
        .expect("CodexUsageState event");
    assert_eq!(usage_state["source"], "exact");
    assert_eq!(usage_state["usage"]["input_tokens"], 11);
    assert_eq!(usage_state["usage"]["output_tokens"], 7);
    assert_eq!(usage_state["usage"]["cache_write_tokens"], 3);
    let response_end = events
        .iter()
        .find(|event| event["type"] == "response_end")
        .expect("ResponseEnd event");
    assert_eq!(response_end["usage"], usage_state["usage"]);
    assert!(
        !serde_json::to_string(&events)
            .expect("event text")
            .contains("usage notification prompt"),
        "usage lifecycle events must not contain prompt text"
    );
}

#[test]
fn codex_cancel_after_turn_acceptance_emits_one_terminal_lifecycle_event() {
    let root = tempfile::tempdir().expect("fixture root");
    let current = std::env::current_exe().expect("current test binary");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        r"const rl=require('readline').createInterface({input:process.stdin});
const fs=require('fs'); const marker=process.env.MAESTRO_CODEX_CANCEL_MARKER; const scenario=process.env.MAESTRO_CODEX_CANCEL_SCENARIO; const log=process.env.MAESTRO_CODEX_CANCEL_LOG;
fs.appendFileSync(log,'started\n');
function send(x){fs.appendFileSync(log,'OUT '+JSON.stringify(x)+'\n');process.stdout.write(JSON.stringify(x)+'\n')}
rl.on('line', line=>{fs.appendFileSync(log,line+'\n'); const x=JSON.parse(line);
if(x.method==='initialize'){send({id:x.id,result:{protocolVersion:'2025-01-01',capabilities:{}}})}
else if(x.method==='model/list'){send({id:x.id,result:{data:[{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'medium'},{reasoningEffort:'high'},{reasoningEffort:'xhigh'}]}],nextCursor:null}})}
else if(x.method==='thread/start'){send({id:x.id,result:{thread:{id:'thread'}}})}
else if(x.method==='turn/start'){send({id:x.id,result:{turn:{id:'turn-cancel'}}});fs.writeFileSync(marker,'accepted')}
else if(x.method==='turn/interrupt'){
  if(scenario==='ok'){send({id:x.id,result:{}})}
  else if(scenario==='error'){send({id:x.id,error:{code:-32000,message:'interrupt denied by fixture'}})}
}
});",
    )
    .expect("app-server script");
    let run_child = |scenario: &str| -> Vec<Value> {
        let dir = root.path().join(scenario);
        std::fs::create_dir_all(&dir).expect("scenario dir");
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let events_path = dir.join("events.json");
        let marker = dir.join("accepted");
        let log = dir.join("app-server.log");
        let output = std::process::Command::new(&current)
            .arg("agent::native_codex_tests::codex_cancel_lifecycle_fixture")
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_CODEX_CANCEL_EVENTS", &events_path)
            .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", &workspace)
            .env("MAESTRO_CODEX_CANCEL_MARKER", &marker)
            .env("MAESTRO_CODEX_CANCEL_SCENARIO", scenario)
            .env("MAESTRO_CODEX_CANCEL_LOG", &log)
            .env("MAESTRO_HOME", dir.join("maestro-home"))
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_MIN_STACK", "16777216")
            .env(
                "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                serde_json::to_string(&vec![script.display().to_string()]).expect("script args"),
            )
            .output()
            .expect("spawn fixture child");
        assert!(
            output.status.success(),
            "{scenario} fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(&log).unwrap_or_default(),
        );
        serde_json::from_slice(&std::fs::read(&events_path).expect("events file"))
            .expect("events json")
    };

    let ok_events = run_child("ok");
    let ok_terminal: Vec<_> = ok_events
        .iter()
        .filter(|event| {
            event["type"] == "codex_turn_state"
                && matches!(
                    event["state"].as_str(),
                    Some("completed" | "interrupted" | "failed")
                )
        })
        .collect();
    assert_eq!(ok_terminal.len(), 1, "{ok_events:?}");
    assert_eq!(ok_terminal[0]["state"], "interrupted");
    assert!(
        ok_events.iter().any(|event| {
            event["type"] == "status"
                && event["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Codex turn interrupted"))
        }),
        "{ok_events:?}"
    );

    let error_events = run_child("error");
    let error_terminal: Vec<_> = error_events
        .iter()
        .filter(|event| {
            event["type"] == "codex_turn_state"
                && matches!(
                    event["state"].as_str(),
                    Some("completed" | "interrupted" | "failed")
                )
        })
        .collect();
    assert_eq!(error_terminal.len(), 1, "{error_events:?}");
    assert_eq!(error_terminal[0]["state"], "failed");
    assert!(
        error_events.iter().any(|event| {
            event["type"] == "status"
                && event["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("turn/interrupt"))
        }),
        "{error_events:?}"
    );

    let timeout_events = run_child("timeout");
    let timeout_terminal: Vec<_> = timeout_events
        .iter()
        .filter(|event| {
            event["type"] == "codex_turn_state"
                && matches!(
                    event["state"].as_str(),
                    Some("completed" | "interrupted" | "failed")
                )
        })
        .collect();
    assert_eq!(timeout_terminal.len(), 1, "{timeout_events:?}");
    assert_eq!(timeout_terminal[0]["state"], "failed");
    assert!(
        timeout_events.iter().any(|event| {
            event["type"] == "status"
                && event["message"].as_str().is_some_and(|message| {
                    message.contains("turn interrupt") && message.contains("timed out")
                })
        }),
        "{timeout_events:?}"
    );
}

#[tokio::test]
async fn codex_pre_turn_failure_fixture() {
    let Ok(scenario) = std::env::var("MAESTRO_CODEX_FAILURE_SCENARIO") else {
        return;
    };
    configure_codex_fixture_identity();
    let workspace = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_WORKSPACE").expect("fixture workspace"),
    );
    let checkpoint = std::path::PathBuf::from(
        std::env::var("MAESTRO_CODEX_FIXTURE_CHECKPOINT").expect("fixture checkpoint"),
    );
    let history: Vec<Message> =
        serde_json::from_slice(&std::fs::read(&checkpoint).expect("fixture checkpoint"))
            .expect("checkpoint messages");
    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex fixture agent");
    agent.replace_history(history);

    let prompts: &[&str] = match scenario.as_str() {
        "transient-inject" => &["retry prompt"],
        "exhausted-start" => &["failed prompt", "second prompt"],
        "cancelled-start" => &["cancelled prompt"],
        "malformed-spawn" => &["malformed prompt"],
        "restart" => &["restart prompt"],
        other => panic!("unsupported failure scenario: {other}"),
    };
    let mut final_snapshot = Vec::new();
    for (index, prompt) in prompts.iter().enumerate() {
        agent
            .prompt((*prompt).to_owned(), vec![])
            .await
            .expect("fixture prompt");
        if scenario == "cancelled-start" {
            let marker = std::path::PathBuf::from(
                std::env::var("MAESTRO_CODEX_FAILURE_MARKER").expect("failure marker"),
            );
            // Startup includes model discovery before turn/start can be sent.
            // Match the adjacent cancellation fixture's startup budget, then
            // keep the terminal snapshot deadline and cancellation assertions.
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while !marker.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("turn/start observation barrier");
            agent.cancel();
        }
        let (snapshot, errors, statuses) = receive_codex_fixture_snapshot(&mut events).await;
        if scenario == "exhausted-start" && index == 0 {
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("Exhausted 3 retry attempts")),
                "terminal GiveUp must remain visible: {errors:?}"
            );
            assert!(
                !snapshot
                    .iter()
                    .any(|message| message.content.as_text() == Some("failed prompt")),
                "an undelivered prompt must not enter the semantic snapshot: {snapshot:?}"
            );
        }
        if scenario == "cancelled-start" {
            assert!(
                !snapshot
                    .iter()
                    .any(|message| message.content.as_text() == Some("cancelled prompt")),
                "a prompt cancelled before turn/start acceptance must not enter provider history: {snapshot:?}"
            );
        }
        if scenario == "malformed-spawn" {
            assert!(
                errors.iter().any(|error| {
                    error.contains("invalid MAESTRO_CODEX_APP_SERVER_ARGS_JSON")
                        && error.contains("column 429")
                        && error.contains("Unknown error - not retrying")
                        && !error.contains("Exhausted")
                }),
                "malformed local config must fail immediately as non-retryable: {errors:?}"
            );
            assert!(
                !statuses.iter().any(|status| status.contains("Retrying")),
                "malformed local config must GiveUp immediately: {statuses:?}"
            );
        }
        final_snapshot = snapshot;
    }

    std::fs::write(
        &checkpoint,
        serde_json::to_vec(&final_snapshot).expect("snapshot json"),
    )
    .expect("persist fixture checkpoint");
    agent.shutdown().await;
}

#[test]
fn codex_pre_turn_failures_preserve_retry_prompt_and_discard_terminal_give_up() {
    let root = tempfile::tempdir().expect("fixture root");
    let maestro_home = root.path().join("maestro-home");
    let codex_home = root.path().join("codex-home");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        r"const rl=require('readline').createInterface({input:process.stdin});
const fs=require('fs');
const scenario=process.env.MAESTRO_CODEX_FAILURE_SCENARIO;
const log=process.env.MAESTRO_CODEX_FAILURE_LOG;
const observed=process.env.MAESTRO_CODEX_FAILURE_ITEMS;
const marker=process.env.MAESTRO_CODEX_FAILURE_MARKER;
fs.appendFileSync(log,'START\n');
function send(x){fs.appendFileSync(log,'OUT '+JSON.stringify(x)+'\n');process.stdout.write(JSON.stringify(x)+'\n')}
function fail(x){send({id:x.id,error:{code:-32000,message:'429 rate limit retry-after: 0 seconds'}})}
rl.on('line',line=>{fs.appendFileSync(log,line+'\n');const x=JSON.parse(line);
if(x.method==='initialize'){send({id:x.id,result:{protocolVersion:'2025-01-01',capabilities:{}}})}
else if(x.method==='model/list'){const respond=()=>send({id:x.id,result:{data:[{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'medium'},{reasoningEffort:'high'},{reasoningEffort:'xhigh'}]}],nextCursor:null}});if(scenario==='cancelled-start'){setTimeout(respond,3000)}else{respond()}}
else if(x.method==='thread/start'){send({id:x.id,result:{thread:{id:'thread'}}})}
else if(x.method==='thread/resume'){send({id:x.id,result:{thread:{id:x.params.threadId}}})}
else if(x.method==='thread/inject_items'){
  if(scenario==='transient-inject'&&!fs.existsSync(marker)){fs.writeFileSync(marker,'failed');fail(x)}
  else{fs.writeFileSync(observed,JSON.stringify(x.params.items));send({id:x.id,result:{}})}
}
else if(x.method==='turn/start'){
  const wire=JSON.stringify(x);
  if(scenario==='cancelled-start'){fs.writeFileSync(marker,'turn observed')}
  else if(scenario==='exhausted-start'&&wire.includes('failed prompt')){fail(x)}
  else{fs.appendFileSync(log,'ACCEPT '+wire+'\n');send({id:x.id,result:{turn:{id:'turn'}}});send({method:'item/agentMessage/delta',params:{turnId:'turn',delta:'fixture answer'}});send({method:'turn/completed',params:{turnId:'turn'}})}
}
});",
    )
    .expect("app-server script");
    let current = std::env::current_exe().expect("current test binary");

    let run_child = |scenario: &str,
                     workspace: &std::path::Path,
                     checkpoint: &std::path::Path,
                     log: &std::path::Path,
                     items: &std::path::Path,
                     marker: &std::path::Path| {
        std::fs::create_dir_all(workspace).expect("fixture workspace");
        let spawn_args = if scenario == "malformed-spawn" {
            format!("[\"{}\"", "x".repeat(426))
        } else {
            serde_json::to_string(&vec![script.display().to_string()]).expect("script args")
        };
        let output = std::process::Command::new(&current)
            .arg("agent::native_codex_tests::codex_pre_turn_failure_fixture")
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_CODEX_FAILURE_SCENARIO", scenario)
            .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", workspace)
            .env("MAESTRO_CODEX_FIXTURE_CHECKPOINT", checkpoint)
            .env("MAESTRO_CODEX_FAILURE_LOG", log)
            .env("MAESTRO_CODEX_FAILURE_ITEMS", items)
            .env("MAESTRO_CODEX_FAILURE_MARKER", marker)
            .env("MAESTRO_HOME", &maestro_home)
            .env("CODEX_HOME", &codex_home)
            // This child owns a standalone fixture configuration. Managed
            // policy tests in the parent process publish temporary paths, and
            // inheriting one after its fixture is removed can block startup
            // before the Codex retry behavior under test is reached.
            .env_remove("MAESTRO_MANAGED_POLICY_PATH")
            .env_remove("MAESTRO_MANAGED_POLICY_STATE_PATH")
            .env_remove("MAESTRO_MANAGED_POLICY_PUBLIC_KEY")
            .env_remove("MAESTRO_MANAGED_POLICY_KEY_ID")
            .env_remove("MAESTRO_MANAGED_POLICY_AUDIT_PATH")
            .env_remove("MAESTRO_ENTERPRISE_POLICY_PATH")
            .env_remove("MAESTRO_POLICY_PATH")
            .env("MAESTRO_OAUTH_STORAGE_MODE", "file")
            .env("MAESTRO_DISABLE_KEYCHAIN", "1")
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_MIN_STACK", "16777216")
            .env("MAESTRO_CODEX_APP_SERVER_ARGS_JSON", spawn_args)
            .output()
            .expect("spawn fixture child");
        assert!(
            output.status.success(),
            "{scenario} fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(log).unwrap_or_default(),
        );
    };

    let initial_history = serde_json::to_vec(&vec![Message {
        role: Role::User,
        content: MessageContent::Text("restored context".to_owned()),
    }])
    .expect("initial history");

    let malformed = root.path().join("malformed");
    std::fs::create_dir_all(&malformed).expect("malformed root");
    let malformed_checkpoint = malformed.join("checkpoint.json");
    let malformed_log = malformed.join("app-server.log");
    let malformed_items = malformed.join("items.json");
    let malformed_marker = malformed.join("unused-marker");
    std::fs::write(&malformed_checkpoint, &initial_history).expect("malformed checkpoint");
    run_child(
        "malformed-spawn",
        &malformed.join("workspace"),
        &malformed_checkpoint,
        &malformed_log,
        &malformed_items,
        &malformed_marker,
    );

    let transient = root.path().join("transient");
    std::fs::create_dir_all(&transient).expect("transient root");
    let transient_checkpoint = transient.join("checkpoint.json");
    let transient_log = transient.join("app-server.log");
    let transient_items = transient.join("items.json");
    let transient_marker = transient.join("failed-once");
    std::fs::write(&transient_checkpoint, &initial_history).expect("transient checkpoint");
    run_child(
        "transient-inject",
        &transient.join("workspace"),
        &transient_checkpoint,
        &transient_log,
        &transient_items,
        &transient_marker,
    );
    let transient_log = std::fs::read_to_string(&transient_log).expect("transient log");
    assert_eq!(
        transient_log
            .matches("\"method\":\"thread/inject_items\"")
            .count(),
        2,
        "the frozen restore prefix must survive one failed injection: {transient_log}"
    );
    assert_eq!(
        transient_log.matches("START").count(),
        2,
        "a pre-turn failure must replace the app-server exactly once: {transient_log}"
    );

    assert_eq!(
        transient_log.matches("ACCEPT ").count(),
        1,
        "the retried prompt must cross turn/start exactly once: {transient_log}"
    );
    assert!(
        transient_log
            .lines()
            .any(|line| line.starts_with("ACCEPT ") && line.contains("retry prompt")),
        "the retried prompt must remain the same pending live input: {transient_log}"
    );
    assert!(
        !std::fs::read_to_string(&transient_items)
            .expect("transient injected items")
            .contains("retry prompt"),
        "the live retry prompt must not leak into injected history"
    );

    let cancelled = root.path().join("cancelled");
    std::fs::create_dir_all(&cancelled).expect("cancelled root");
    let cancelled_checkpoint = cancelled.join("checkpoint.json");
    let cancelled_log = cancelled.join("app-server.log");
    let cancelled_items = cancelled.join("items.json");
    let cancelled_marker = cancelled.join("turn-observed");
    std::fs::write(&cancelled_checkpoint, &initial_history).expect("cancelled checkpoint");
    run_child(
        "cancelled-start",
        &cancelled.join("workspace"),
        &cancelled_checkpoint,
        &cancelled_log,
        &cancelled_items,
        &cancelled_marker,
    );
    let cancelled_snapshot =
        std::fs::read_to_string(&cancelled_checkpoint).expect("cancelled snapshot");
    assert!(
        !cancelled_snapshot.contains("cancelled prompt"),
        "pre-start cancellation must not persist the prompt: {cancelled_snapshot}"
    );

    let exhausted = root.path().join("exhausted");
    std::fs::create_dir_all(&exhausted).expect("exhausted root");
    let exhausted_checkpoint = exhausted.join("checkpoint.json");
    let exhausted_log = exhausted.join("app-server.log");
    let exhausted_items = exhausted.join("items.json");
    let exhausted_marker = exhausted.join("unused-marker");
    std::fs::write(&exhausted_checkpoint, &initial_history).expect("exhausted checkpoint");
    run_child(
        "exhausted-start",
        &exhausted.join("workspace"),
        &exhausted_checkpoint,
        &exhausted_log,
        &exhausted_items,
        &exhausted_marker,
    );
    run_child(
        "restart",
        &exhausted.join("restored-workspace"),
        &exhausted_checkpoint,
        &exhausted_log,
        &exhausted_items,
        &exhausted_marker,
    );
    let exhausted_log = std::fs::read_to_string(&exhausted_log).expect("exhausted log");
    assert_eq!(
        exhausted_log.matches("ACCEPT ").count(),
        2,
        "only the second and post-restart prompts may be accepted: {exhausted_log}"
    );
    let restored_items =
        std::fs::read_to_string(&exhausted_items).expect("post-GiveUp restored items");
    assert!(
        !restored_items.contains("failed prompt"),
        "the undelivered prompt must never be injected after restart: {restored_items}"
    );
    assert!(
        restored_items.contains("second prompt"),
        "the successfully started prompt must remain provider history: {restored_items}"
    );
    let binding_dir = maestro_home.join("codex/thread-bindings");
    assert!(
        std::fs::read_dir(&binding_dir)
            .expect("fixture-owned Codex thread bindings")
            .next()
            .is_some(),
        "fixture children must persist their bindings under the owned Maestro home"
    );
}

#[test]
fn codex_process_death_restores_split_delta_tool_history() {
    let root = tempfile::tempdir().expect("fixture root");
    let source_workspace = root.path().join("source");
    let restored_workspace = root.path().join("restored");
    std::fs::create_dir_all(&source_workspace).expect("source workspace");
    std::fs::create_dir_all(&restored_workspace).expect("restored workspace");
    std::fs::write(
        source_workspace.join("Cargo.toml"),
        "[package]\nname='fixture'\n",
    )
    .expect("source tool file");
    let checkpoint = source_workspace.join("checkpoint.json");
    let observed_items = root.path().join("restored-items.json");
    let script_log = root.path().join("app-server.log");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        format!(
            r"const rl=require('readline').createInterface({{input:process.stdin}});
const fs=require('fs'); const source=process.env.MAESTRO_CODEX_FIXTURE_ROLE==='source'; const log='{}'; fs.appendFileSync(log,'started\n');
function send(x){{fs.appendFileSync(log,'OUT '+JSON.stringify(x)+'\n');process.stdout.write(JSON.stringify(x)+'\n')}}
rl.on('line', line=>{{fs.appendFileSync(log,line+'\n'); const x=JSON.parse(line); if(!x.method){{if(x.id==='tool-1')setTimeout(()=>{{send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'suffix'}}}});send({{method:'turn/completed',params:{{turnId:'turn'}}}})}},10);return}} if(x.method==='initialize'){{send({{id:x.id,result:{{protocolVersion:'2025-01-01',capabilities:{{}}}}}})}}
else if(x.method==='model/list'){{send({{id:x.id,result:{{data:[{{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{{reasoningEffort:'low'}},{{reasoningEffort:'medium'}},{{reasoningEffort:'high'}},{{reasoningEffort:'xhigh'}}]}}],nextCursor:null}}}})}}
else if(x.method==='thread/start'){{send({{id:x.id,result:{{thread:{{id:'thread'}}}}}})}}
else if(x.method==='thread/inject_items'){{fs.writeFileSync('{}',JSON.stringify(x.params.items));send({{id:x.id,result:{{}}}})}}
else if(x.method==='turn/start'){{send({{id:x.id,result:{{turn:{{id:'turn'}}}}}}); if(source){{setTimeout(()=>{{send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'prefix '}}}});send({{id:'tool-1',method:'item/tool/call',params:{{tool:'read',callId:'call-codex-1',arguments:{{path:'Cargo.toml'}}}}}})}},10)}} else {{setTimeout(()=>{{send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'restored answer'}}}});send({{method:'turn/completed',params:{{turnId:'turn'}}}})}},10)}}}}
}});",
            script_log.display(),
            observed_items.display()
        ),
    )
    .expect("app-server script");
    let current = std::env::current_exe().expect("current test binary");
    let run_child = |role: &str, workspace: &std::path::Path, checkpoint: &std::path::Path| {
        let output = std::process::Command::new(&current)
            .arg("agent::native_codex_tests::codex_process_continuation_fixture")
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_CODEX_FIXTURE_ROLE", role)
            .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", workspace)
            .env("MAESTRO_CODEX_FIXTURE_CHECKPOINT", checkpoint)
            .env("MAESTRO_HOME", workspace.join("maestro-home"))
            .env("MAESTRO_OAUTH_STORAGE_MODE", "file")
            .env("MAESTRO_DISABLE_KEYCHAIN", "1")
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .env("RUST_BACKTRACE", "1")
            .env("RUST_MIN_STACK", "16777216")
            .env(
                "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                serde_json::to_string(&vec![script.display().to_string()]).expect("script args"),
            )
            .output()
            .expect("spawn fixture child");
        assert!(
            output.status.success(),
            "{role} fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(&script_log).unwrap_or_default(),
        );
    };
    run_child("source", &source_workspace, &checkpoint);
    std::fs::copy(&checkpoint, restored_workspace.join("checkpoint.json"))
        .expect("hydrate runtime-generated checkpoint");
    run_child(
        "restore",
        &restored_workspace,
        &restored_workspace.join("checkpoint.json"),
    );
    let restored_items_text = std::fs::read_to_string(&observed_items).expect("restored items");
    let restored_items: Vec<serde_json::Value> =
        serde_json::from_str(&restored_items_text).expect("restored item JSON");
    let item_shape: Vec<(&str, Option<&str>)> = restored_items
        .iter()
        .map(|item| {
            let kind = item["type"].as_str().expect("item type");
            let value = match kind {
                "message" => item["content"][0]["text"].as_str(),
                "function_call" | "function_call_output" => item["call_id"].as_str(),
                other => panic!("unexpected restored item type: {other}"),
            };
            (kind, value)
        })
        .collect();
    assert_eq!(
        item_shape,
        vec![
            ("message", Some("first prompt")),
            ("message", Some("prefix ")),
            ("function_call", Some("call-codex-1")),
            ("function_call_output", Some("call-codex-1")),
            ("message", Some("suffix")),
        ],
        "provider-visible chronology changed: {restored_items_text}"
    );
    assert!(
        !restored_items_text.contains("second prompt"),
        "{restored_items_text}"
    );
    let app_server_log = std::fs::read_to_string(&script_log).expect("app-server log");
    assert_eq!(
        app_server_log.matches("second prompt").count(),
        1,
        "the live prompt must appear exactly once in turn/start: {app_server_log}"
    );
}

#[test]
fn codex_semantic_history_is_bounded_before_snapshot_and_reinjection() {
    let root = tempfile::tempdir().expect("fixture root");
    let source_workspace = root.path().join("source");
    let restored_workspace = root.path().join("restored");
    std::fs::create_dir_all(&source_workspace).expect("source workspace");
    std::fs::create_dir_all(&restored_workspace).expect("restored workspace");
    let checkpoint = source_workspace.join("checkpoint.json");
    let observed_items = root.path().join("restored-items.json");
    let script_log = root.path().join("app-server.log");
    let script = root.path().join("app-server.js");
    std::fs::write(
        &script,
        format!(
            r"const rl=require('readline').createInterface({{input:process.stdin}});
const fs=require('fs'); const log='{}';
function send(x){{fs.appendFileSync(log,'OUT '+JSON.stringify(x)+'\n');process.stdout.write(JSON.stringify(x)+'\n')}}
rl.on('line', line=>{{fs.appendFileSync(log,line+'\n'); const x=JSON.parse(line);
if(x.method==='initialize'){{send({{id:x.id,result:{{protocolVersion:'2025-01-01',capabilities:{{}}}}}})}}
else if(x.method==='model/list'){{send({{id:x.id,result:{{data:[{{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{{reasoningEffort:'low'}},{{reasoningEffort:'medium'}},{{reasoningEffort:'high'}},{{reasoningEffort:'xhigh'}}]}}],nextCursor:null}}}})}}
else if(x.method==='thread/start'){{send({{id:x.id,result:{{thread:{{id:'thread'}}}}}})}}
else if(x.method==='thread/inject_items'){{fs.writeFileSync('{}',JSON.stringify(x.params.items));send({{id:x.id,result:{{}}}})}}
else if(x.method==='turn/start'){{send({{id:x.id,result:{{turn:{{id:'turn'}}}}}});setTimeout(()=>{{send({{method:'item/agentMessage/delta',params:{{turnId:'turn',delta:'fixture answer'}}}});send({{method:'turn/completed',params:{{turnId:'turn'}}}})}},10)}}
}});",
            script_log.display(),
            observed_items.display()
        ),
    )
    .expect("app-server script");

    let current = std::env::current_exe().expect("current test binary");
    let run_child =
        |role: &str, workspace: &std::path::Path, checkpoint: &std::path::Path, oversized: bool| {
            let mut command = std::process::Command::new(&current);
            command
                .arg("agent::native_codex_tests::codex_process_continuation_fixture")
                .arg("--exact")
                .arg("--nocapture")
                .env("MAESTRO_CODEX_FIXTURE_ROLE", role)
                .env("MAESTRO_CODEX_FIXTURE_WORKSPACE", workspace)
                .env("MAESTRO_CODEX_FIXTURE_CHECKPOINT", checkpoint)
                .env("MAESTRO_HOME", workspace.join("maestro-home"))
                .env("MAESTRO_OAUTH_STORAGE_MODE", "file")
                .env("MAESTRO_DISABLE_KEYCHAIN", "1")
                .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
                .env("OPENAI_CODEX_TOKEN", "fixture-token")
                .env("RUST_BACKTRACE", "1")
                .env("RUST_MIN_STACK", "16777216")
                .env(
                    "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                    serde_json::to_string(&vec![script.display().to_string()])
                        .expect("script args"),
                );
            if oversized {
                command.env("MAESTRO_CODEX_FIXTURE_OVERSIZED", "1");
            }
            let output = command.output().expect("spawn fixture child");
            assert!(
                output.status.success(),
                "{role} fixture failed: {}; stdout: {}; stderr: {}; app-server log: {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                std::fs::read_to_string(&script_log).unwrap_or_default(),
            );
        };

    run_child("source", &source_workspace, &checkpoint, true);
    let snapshot_bytes = std::fs::read(&checkpoint).expect("semantic snapshot");
    let snapshot: Vec<Message> =
        serde_json::from_slice(&snapshot_bytes).expect("snapshot messages");
    let compaction_config =
        super::compaction::CompactionConfig::for_model("openai-codex/gpt-5.5", None);
    let token_budget = compaction_config.max_context_tokens;
    let compactor = super::compaction::ContextCompactor::new(compaction_config);
    assert!(
        compactor.estimate_tokens(&snapshot) <= token_budget,
        "semantic snapshot exceeded configured token budget: {} > {token_budget}",
        compactor.estimate_tokens(&snapshot)
    );
    assert!(
        snapshot_bytes.len() as u64
            <= token_budget * maestro_context::token_estimation::BYTES_PER_TOKEN as u64,
        "serialized semantic snapshot exceeded the configured byte-derived bound"
    );
    assert!(
        snapshot
            .iter()
            .any(|message| message.content.as_text() == Some("first prompt")),
        "the most recent live prompt must survive compaction"
    );
    let snapshot_blocks: Vec<&ContentBlock> = snapshot
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::Blocks(blocks) => Some(blocks.iter()),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .collect();
    let snapshot_tool_use_index = snapshot_blocks
        .iter()
        .position(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { id, name, .. }
                    if id == "bounded-call-1" && name == "read"
            )
        })
        .expect("snapshot tool use");
    let snapshot_tool_result_index = snapshot_blocks
        .iter()
        .position(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { tool_use_id, .. }
                    if tool_use_id == "bounded-call-1"
            )
        })
        .expect("snapshot tool result");
    assert!(
        snapshot_tool_use_index < snapshot_tool_result_index,
        "snapshot tool use/result pair must remain valid and ordered"
    );

    std::fs::copy(&checkpoint, restored_workspace.join("checkpoint.json"))
        .expect("hydrate bounded checkpoint");
    run_child(
        "restore",
        &restored_workspace,
        &restored_workspace.join("checkpoint.json"),
        false,
    );

    let restored_items_bytes = std::fs::read(&observed_items).expect("restored provider items");
    assert!(
        restored_items_bytes.len() as u64
            <= token_budget * maestro_context::token_estimation::BYTES_PER_TOKEN as u64,
        "reinjected provider items exceeded the configured byte-derived bound"
    );
    let restored_items: Vec<serde_json::Value> =
        serde_json::from_slice(&restored_items_bytes).expect("provider items json");
    let tool_use_index = restored_items
        .iter()
        .position(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
                && item.get("call_id").and_then(serde_json::Value::as_str) == Some("bounded-call-1")
        })
        .expect("bounded tool use");
    let tool_result_index = restored_items
        .iter()
        .position(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(serde_json::Value::as_str) == Some("bounded-call-1")
        })
        .expect("bounded tool result");
    assert!(
        tool_use_index < tool_result_index,
        "tool use/result pair must remain ordered: {restored_items:?}"
    );
    assert!(
        !String::from_utf8_lossy(&restored_items_bytes).contains("second prompt"),
        "live prompt leaked into restored provider items"
    );
    let app_server_log = std::fs::read_to_string(&script_log).expect("app-server log");
    assert_eq!(
        app_server_log.matches("second prompt").count(),
        1,
        "the post-restore prompt must appear once in turn/start: {app_server_log}"
    );
}

#[tokio::test]
async fn set_model_normalizes_nonreasoning_target_before_next_request() {
    let _guard = crate::config::test_process_env_lock_async().await;
    let _restore = EnvRestore::capture(&[
        "MAESTRO_HOME",
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
        "OPENROUTER_API_KEY",
        "OPENROUTER_BASE_URL",
        "MAESTRO_CONNECTION",
    ]);
    let home = tempfile::tempdir().expect("Maestro home");
    std::env::set_var("MAESTRO_HOME", home.path());
    configure_codex_fixture_identity();
    std::env::set_var(crate::credential_mode::WORKSPACE_ID_ENV, "workspace-test");
    std::env::remove_var("MAESTRO_CONNECTION");
    std::env::set_var("OPENROUTER_API_KEY", "fixture-openrouter-key");
    let (base_url, requests) = scripted_single_turn_provider().await;
    std::env::set_var("OPENROUTER_BASE_URL", base_url);

    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "openrouter/openai/o1".to_owned(),
        cwd: workspace.path().display().to_string(),
        thinking_enabled: true,
        thinking_budget: 15_000,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "fixture",
        vec![crate::ai::ScriptedResponse::text("unused")],
    ));
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("fixture agent");
    agent
        .set_model("openrouter/openai/gpt-4o")
        .expect("queue model switch");

    let mut changed = false;
    let mut normalized = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !(changed && normalized) {
            match events.recv().await.expect("model switch event") {
                FromAgent::ModelChanged { model, .. } => {
                    assert_eq!(model, "openrouter/openai/gpt-4o");
                    changed = true;
                }
                FromAgent::BoostChanged {
                    status: crate::model_dynamics::BoostStatus::Idle,
                    thinking: Some(crate::session::ThinkingLevel::Off),
                } => normalized = true,
                FromAgent::ModelChangeFailed { reason, .. } => {
                    panic!("target model must resolve: {reason}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("model switch timeout");

    agent
        .prompt("Use the normalized setting.".to_owned(), vec![])
        .await
        .expect("prompt after model switch");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("request event") {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("normalized target request failed: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("target request timeout");
    agent.shutdown().await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].get("reasoning_effort").is_none(),
        "nonreasoning target must not inherit reasoning_effort: {}",
        requests[0]
    );
}

#[tokio::test]
async fn failed_codex_model_switch_preserves_the_live_app_server_session() {
    let _guard = crate::config::test_process_env_lock_async().await;
    let _restore = EnvRestore::capture(&[
        "MAESTRO_HOME",
        "CODEX_HOME",
        "OPENAI_CODEX_TOKEN",
        "MAESTRO_CODEX_APP_SERVER_COMMAND",
        "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
    ]);
    let root = tempfile::tempdir().expect("fixture root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("fixture workspace");
    std::env::set_var("MAESTRO_HOME", root.path().join("maestro-home"));
    std::env::set_var("CODEX_HOME", root.path().join("codex-home"));
    std::env::set_var("OPENAI_CODEX_TOKEN", "fixture-codex-token");
    configure_codex_fixture_identity();
    std::env::set_var(crate::credential_mode::WORKSPACE_ID_ENV, "workspace-test");
    std::env::remove_var("MAESTRO_CONNECTION");

    let log_path = root.path().join("app-server.log");
    let script_path = root.path().join("app-server.js");
    let log_literal =
        serde_json::to_string(&log_path.display().to_string()).expect("log path literal");
    let script = r"const readline = require('readline');
const fs = require('fs');
const log = __LOG__;
let turn = 0;
function send(value) {
  fs.appendFileSync(log, `OUT ${JSON.stringify(value)}\n`);
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
const rl = readline.createInterface({input: process.stdin});
rl.on('line', line => {
  const message = JSON.parse(line);
  fs.appendFileSync(log, `IN ${message.method || ''}\n`);
  if (message.method === 'initialize') {
send({id: message.id, result: {protocolVersion: '2025-01-01', capabilities: {
  methods: ['thread/start', 'turn/start', 'turn/interrupt'],
  notifications: ['item/tool/call', 'item/agentMessage/delta', 'turn/completed']
}}});
  } else if (message.method === 'model/list') {
send({id: message.id, result: {data: [{id: 'gpt-5.5', model: 'gpt-5.5',
  defaultReasoningEffort: 'medium', supportedReasoningEfforts: [
    {reasoningEffort: 'low'}, {reasoningEffort: 'medium'},
    {reasoningEffort: 'high'}, {reasoningEffort: 'xhigh'}
  ]}], nextCursor: null}});
  } else if (message.method === 'thread/start') {
send({id: message.id, result: {thread: {id: 'thread-stable'}}});
  } else if (message.method === 'turn/start') {
const turnId = `turn-${++turn}`;
send({id: message.id, result: {turn: {id: turnId}}});
setTimeout(() => {
  send({method: 'item/agentMessage/delta', params: {turnId, delta: 'turn complete'}});
  send({method: 'turn/completed', params: {turnId}});
}, 5);
  }
});
"
    .replace("__LOG__", &log_literal);
    std::fs::write(&script_path, script).expect("app-server script");
    std::env::set_var("MAESTRO_CODEX_APP_SERVER_COMMAND", "node");
    std::env::set_var(
        "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
        serde_json::to_string(&vec![script_path.display().to_string()]).expect("script args"),
    );

    let config = NativeAgentConfig {
        model: "openai-codex/gpt-5.5".to_owned(),
        cwd: workspace.display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new(config).expect("Codex fixture agent");
    agent
        .prompt("Start the preserved session.".to_owned(), vec![])
        .await
        .expect("first prompt");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("first turn event") {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("first Codex turn failed: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("first turn timeout");

    agent
        .set_model("openrouter/openai/model-that-is-not-configured")
        .expect("queue invalid model switch");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("model switch event") {
                FromAgent::ModelChangeFailed { model, .. } => {
                    assert_eq!(model, "openrouter/openai/model-that-is-not-configured");
                    break;
                }
                FromAgent::ModelChanged { model, .. } => {
                    panic!("invalid model unexpectedly activated: {model}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("model switch timeout");

    agent
        .prompt("Continue on the original session.".to_owned(), vec![])
        .await
        .expect("second prompt");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("second turn event") {
                FromAgent::TurnCompleted { .. } => break,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("preserved Codex turn failed: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("second turn timeout");
    agent.shutdown().await;

    let log = std::fs::read_to_string(log_path).expect("app-server log");
    let count = |method: &str| {
        log.lines()
            .filter(|line| line.strip_prefix("IN ").is_some_and(|name| name == method))
            .count()
    };
    assert_eq!(
        count("initialize"),
        1,
        "failed switch restarted Codex: {log}"
    );
    assert_eq!(
        count("thread/start"),
        1,
        "failed switch replaced thread: {log}"
    );
    assert_eq!(
        count("turn/start"),
        2,
        "both prompts must use one thread: {log}"
    );
}
