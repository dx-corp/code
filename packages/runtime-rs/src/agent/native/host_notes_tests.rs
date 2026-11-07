//! Host notes: an idle agent wakes for a notification; a busy one appends it
//! after the turn without starting another (unless the note must be seen).

use super::NativeAgentConfig;
use super::host_notes::HostNoteWake;
use super::tests::*;
use crate::agent::{FromAgent, NativeAgent};
use crate::ai::UnifiedClient;
use serde_json::Value;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);
/// Long enough for a spurious wake to have sent its provider request.
const QUIET: Duration = Duration::from_millis(500);

/// A provider stub: each request is reported, then answered once released.
struct Provider {
    requests: mpsc::UnboundedReceiver<Value>,
    release: mpsc::UnboundedSender<()>,
    server: tokio::task::JoinHandle<()>,
}

async fn provider() -> (Provider, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (release, mut release_rx) = mpsc::unbounded_channel::<()>();
    let server = tokio::spawn(async move {
        let mut index = 0;
        while let Ok((mut stream, _)) = listener.accept().await {
            if request_tx
                .send(read_scripted_provider_request(&mut stream).await)
                .is_err()
            {
                return;
            }
            if release_rx.recv().await.is_none() {
                return;
            }
            index += 1;
            let response = chat_sse_response(&format!("reply-{index}"), "Noted.", false);
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            let _ = stream.write_all(wire.as_bytes()).await;
        }
    });
    (
        Provider {
            requests,
            release,
            server,
        },
        format!("http://{address}/v1"),
    )
}

fn agent(
    base_url: &str,
) -> (
    NativeAgent,
    mpsc::UnboundedReceiver<FromAgent>,
    tempfile::TempDir,
) {
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        ..Default::default()
    };
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", base_url).unwrap(),
    );
    let (agent, events) =
        new_runtime_test_agent_with_host(config.clone(), RuntimeTestHost::new(config.cwd, client))
            .unwrap();
    (agent, events, workspace)
}

fn carries(request: &Value, note: &str) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|message| message["content"] == note))
}

async fn next_request(provider: &mut Provider) -> Value {
    tokio::time::timeout(WAIT, provider.requests.recv())
        .await
        .expect("provider request timeout")
        .expect("provider closed")
}

async fn turn_finished(events: &mut mpsc::UnboundedReceiver<FromAgent>) {
    tokio::time::timeout(WAIT, async {
        while let Some(event) = events.recv().await {
            match event {
                FromAgent::ResponseEnd { .. } => return,
                FromAgent::Error { message, .. } | FromAgent::ProviderError { message, .. } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
        panic!("agent event channel closed");
    })
    .await
    .expect("turn end timeout");
}

async fn stays_quiet(provider: &mut Provider) {
    assert!(
        tokio::time::timeout(QUIET, provider.requests.recv())
            .await
            .is_err(),
        "an appended note must not start a turn"
    );
}

#[tokio::test]
async fn idle_agent_starts_a_turn_for_a_host_notification() {
    let (mut provider, base_url) = provider().await;
    let (agent, mut events, _workspace) = agent(&base_url);
    let note =
        "[tool_notification kind=\"monitor\"]\nevent: output\n| build ok\n[/tool_notification]";

    let (applied, consumed) = agent
        .deliver_host_note(note, HostNoteWake::WhenIdle)
        .unwrap();
    tokio::time::timeout(WAIT, applied).await.unwrap().unwrap();
    let request = next_request(&mut provider).await;
    assert!(carries(&request, note), "{request}");
    provider.release.send(()).unwrap();
    turn_finished(&mut events).await;
    tokio::time::timeout(WAIT, consumed)
        .await
        .expect("consumption timeout")
        .expect("the woken turn carried the note to the model");
    stays_quiet(&mut provider).await;

    agent.shutdown().await;
    provider.server.abort();
}

#[tokio::test]
async fn idle_agent_only_appends_a_plain_user_note() {
    let (mut provider, base_url) = provider().await;
    let (agent, _events, _workspace) = agent(&base_url);

    let (applied, mut consumed) = agent.inject_user_note("Background task exited.").unwrap();
    tokio::time::timeout(WAIT, applied).await.unwrap().unwrap();
    stays_quiet(&mut provider).await;
    assert!(matches!(
        consumed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    agent.shutdown().await;
    provider.server.abort();
}

#[tokio::test]
async fn busy_agent_appends_output_after_the_turn_without_another() {
    let (mut provider, base_url) = provider().await;
    let (agent, mut events, _workspace) = agent(&base_url);
    agent
        .prompt("Start the build.".into(), vec![])
        .await
        .unwrap();
    let first = next_request(&mut provider).await;

    let note =
        "[tool_notification kind=\"monitor\"]\nevent: output\n| compiling\n[/tool_notification]";
    let (mut applied, mut consumed) = agent
        .deliver_host_note(note, HostNoteWake::WhenIdle)
        .unwrap();
    tokio::time::sleep(QUIET).await;
    assert!(
        matches!(
            applied.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "a note is never inserted into a running turn"
    );
    assert!(!carries(&first, note));

    provider.release.send(()).unwrap();
    turn_finished(&mut events).await;
    tokio::time::timeout(WAIT, &mut applied)
        .await
        .unwrap()
        .unwrap();
    stays_quiet(&mut provider).await;
    assert!(matches!(
        consumed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    // The appended note reaches the model with the next turn.
    agent.prompt("Status?".into(), vec![]).await.unwrap();
    let next = next_request(&mut provider).await;
    assert!(carries(&next, note), "{next}");
    provider.release.send(()).unwrap();
    tokio::time::timeout(WAIT, consumed).await.unwrap().unwrap();

    agent.shutdown().await;
    provider.server.abort();
}

#[tokio::test]
async fn busy_agent_wakes_once_after_the_turn_for_a_terminal_notice() {
    let (mut provider, base_url) = provider().await;
    let (agent, mut events, _workspace) = agent(&base_url);
    agent
        .prompt("Start the build.".into(), vec![])
        .await
        .unwrap();
    let _first = next_request(&mut provider).await;

    let output =
        "[tool_notification kind=\"monitor\"]\nevent: output\n| linking\n[/tool_notification]";
    let terminal =
        "[tool_notification kind=\"monitor\"]\nevent: exited\nexit_code: 0\n[/tool_notification]";
    let (_output_applied, output_consumed) = agent
        .deliver_host_note(output, HostNoteWake::WhenIdle)
        .unwrap();
    let (_terminal_applied, terminal_consumed) = agent
        .deliver_host_note(terminal, HostNoteWake::AfterCurrentTurn)
        .unwrap();
    provider.release.send(()).unwrap();
    turn_finished(&mut events).await;

    // One follow-up turn carries both notes.
    let follow_up = next_request(&mut provider).await;
    assert!(carries(&follow_up, output), "{follow_up}");
    assert!(carries(&follow_up, terminal), "{follow_up}");
    provider.release.send(()).unwrap();
    tokio::time::timeout(WAIT, output_consumed)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(WAIT, terminal_consumed)
        .await
        .unwrap()
        .unwrap();
    stays_quiet(&mut provider).await;

    agent.shutdown().await;
    provider.server.abort();
}
