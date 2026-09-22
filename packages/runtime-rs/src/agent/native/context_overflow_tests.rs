//! Context-window overflow: compaction before a request, and recovery after
//! the provider rejects one.

use super::tests::*;
use super::{MIN_RESPONSE_RESERVE_TOKENS, NativeAgentConfig};
use crate::agent::FromAgent;
use crate::ai::{Message, MessageContent, Role, UnifiedClient};
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// History sized to cross a compaction trigger derived from `context_window`.
fn bulky_history(messages: usize) -> Vec<Message> {
    (0..messages)
        .map(|index| Message {
            role: if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: MessageContent::text(format!(
                "Turn {index}: {}",
                "retain this context ".repeat(80)
            )),
        })
        .collect()
}

#[tokio::test]
async fn compaction_runs_between_tool_batches_of_one_turn() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(workspace.path().join("evidence.txt"), "retained evidence")
        .expect("write fixture");
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/mid-turn-compaction",
        vec![
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "read-evidence".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path":"evidence.txt"}),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::text("done"),
        ],
    );
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        context_window: Some(8_192),
        approval_mode: crate::agent::ApprovalMode::Yolo,
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) =
        new_runtime_test_agent(config, UnifiedClient::Scripted(scripted)).expect("test agent");
    agent.replace_history_with_continuation(bulky_history(40), None);
    agent
        .prompt("read the evidence".to_owned(), Vec::new())
        .await
        .expect("prompt queued");

    // The turn's first response carries a tool call, so post-response
    // compaction is skipped for it. Compaction must still happen before the
    // second request goes out, which is the only place the growing history
    // can be bounded inside a tool chain.
    let compacted_before_second_request = tokio::time::timeout(Duration::from_secs(20), async {
        let mut response_starts = 0;
        let mut compacted = false;
        loop {
            match events.recv().await {
                Some(FromAgent::ResponseStart { .. }) => {
                    response_starts += 1;
                    if response_starts == 2 {
                        break compacted;
                    }
                }
                Some(FromAgent::Compaction { .. }) => compacted = true,
                Some(FromAgent::TurnCompleted { .. }) => break compacted,
                Some(FromAgent::Error { message, .. })
                | Some(FromAgent::ProviderError { message, .. }) => panic!("{message}"),
                Some(_) => {}
                None => panic!("agent event channel closed"),
            }
        }
    })
    .await
    .expect("turn timeout");

    assert!(
        compacted_before_second_request,
        "a tool chain must be compacted before its next request, not only after the turn ends"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn provider_context_rejection_compacts_and_retries_instead_of_ending_the_turn() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let scripted = crate::ai::ScriptedClient::new(
        "runtime-test/context-overflow",
        vec![
            crate::ai::ScriptedResponse::stream_error(
                "prompt is too long: 213462 tokens > 200000 maximum",
            ),
            crate::ai::ScriptedResponse::text("recovered"),
        ],
    );
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".to_owned(),
        cwd: workspace.path().display().to_string(),
        // A 50,000-token window puts the pre-request trigger at 42,500 and
        // the retained recent window at 10,000. The history below lands
        // between them under either token counter, so the rejection is the
        // only thing that can compact here, and it has history to give up.
        context_window: Some(50_000),
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) =
        new_runtime_test_agent(config, UnifiedClient::Scripted(scripted)).expect("test agent");
    agent.replace_history_with_continuation(bulky_history(60), None);
    agent
        .prompt("continue".to_owned(), Vec::new())
        .await
        .expect("prompt queued");

    let (compacted, answered) = tokio::time::timeout(Duration::from_secs(20), async {
        let mut compacted = false;
        let mut answered = false;
        loop {
            match events.recv().await {
                Some(FromAgent::Compaction { .. }) => compacted = true,
                Some(FromAgent::LocalAssistantContent { content, .. }) => {
                    answered |= content.iter().any(|block| {
                        matches!(block, crate::ai::ContentBlock::Text { text } if text.contains("recovered"))
                    });
                }
                Some(FromAgent::TurnCompleted { .. }) => break (compacted, answered),
                Some(FromAgent::Error { message, .. })
                | Some(FromAgent::ProviderError { message, .. }) => {
                    panic!("turn ended on a recoverable context rejection: {message}")
                }
                Some(_) => {}
                None => panic!("agent event channel closed"),
            }
        }
    })
    .await
    .expect("turn timeout");

    assert!(compacted, "the rejected request must trigger a compaction");
    assert!(answered, "the retried request must produce the answer");
    agent.shutdown().await;
}

#[tokio::test]
async fn hosted_request_clamps_its_output_allowance_to_the_live_context_window() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        request_tx
            .send(read_scripted_provider_request(&mut stream).await)
            .unwrap();
        let response = chat_sse_response("clamped", "Done.", false);
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream.write_all(wire.as_bytes()).await.unwrap();
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        // A model with no "local/" or "llamacpp/" prefix: the clamp used to
        // skip exactly this case and let the request reach the provider with
        // an output ceiling the window cannot hold.
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        // Far above the live window below, so the compactor leaves this
        // history alone and the clamp is the only thing under test.
        context_window: Some(1_000_000),
        ..Default::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    // An 8_000-token output ceiling inside an 8_192-token window: any history
    // at all makes the unclamped request larger than the window.
    let host = RuntimeTestHost::new(config.cwd.clone(), client).with_model_limits(8_000, 8_192);
    let (agent, _events) = new_runtime_test_agent_with_host(config, host).unwrap();
    agent.replace_history_with_continuation(bulky_history(4), None);
    agent.prompt("continue".into(), vec![]).await.unwrap();

    let request = tokio::time::timeout(Duration::from_secs(10), request_rx.recv())
        .await
        .expect("provider request deadline")
        .expect("provider request");
    let max_tokens = request["max_tokens"]
        .as_u64()
        .or_else(|| request["max_completion_tokens"].as_u64())
        .expect("request carries an output ceiling");

    assert!(
        max_tokens < 8_000,
        "the configured ceiling must be clamped down to what the window holds, got {max_tokens}"
    );
    assert!(
        max_tokens >= u64::from(MIN_RESPONSE_RESERVE_TOKENS),
        "a sent request must still leave a usable response, got {max_tokens}"
    );
    agent.shutdown().await;
    server.abort();
}
