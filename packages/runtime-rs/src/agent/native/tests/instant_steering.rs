use super::*;

/// The first response never completes. Only cancellation can release it, and
/// the fixture refuses the replacement request until that connection closes.
async fn stalled_provider(
    subsequent_responses: usize,
) -> (
    UnifiedClient,
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<Vec<Value>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (released_tx, released_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let mut requests = vec![read_scripted_provider_request(&mut first).await];
        let chunk = json!({"id":"stalled","object":"chat.completion.chunk","created":0,
            "model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant",
            "content":"Working on the original request."},"finish_reason":null}]});
        first.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\ndata: {chunk}\n\n").as_bytes()).await.unwrap();
        let mut byte = [0];
        assert_eq!(first.read(&mut byte).await.unwrap(), 0);
        released_tx.send(()).unwrap();
        for index in 0..subsequent_responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_scripted_provider_request(&mut stream).await);
            let body = chat_sse_response(&format!("replacement-{index}"), "Done.", false);
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", format!("http://{address}/v1")).unwrap(),
    );
    (client, released_rx, server)
}

async fn wait_for_partial_response(events: &mut mpsc::UnboundedReceiver<FromAgent>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await, Some(FromAgent::ResponseChunk { .. })) {
                break;
            }
        }
    })
    .await
    .expect("first response must start");
}

#[tokio::test]
async fn explicit_steer_preempts_stalled_stream_but_follow_up_waits() {
    let (client, mut released, server) = stalled_provider(2).await;
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..Default::default()
    };
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent
        .prompt("Original request".into(), vec![])
        .await
        .unwrap();
    wait_for_partial_response(&mut events).await;
    agent
        .prompt_with_kind(
            "Follow up later".into(),
            vec![],
            PromptKind::FollowUp,
            Some(1),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut released)
            .await
            .is_err(),
        "a follow-up must leave the active response running"
    );
    agent
        .prompt_with_kind(
            "Change direction now".into(),
            vec![],
            PromptKind::Steer,
            Some(2),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), released)
        .await
        .expect("steering must release a stalled provider without waiting for its terminal event")
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    agent.shutdown().await;
    let requests = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    let second = requests[1]["messages"].to_string();
    assert!(second.contains("Change direction now"), "{second}");
    assert!(
        !second.contains("Follow up later"),
        "follow-up overtook steering: {second}"
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("Follow up later")
    );
}

#[tokio::test]
async fn cancellation_before_steer_keeps_the_new_prompt_for_the_next_request() {
    let (client, released, server) = stalled_provider(1).await;
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..Default::default()
    };
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent
        .prompt("Original request".into(), vec![])
        .await
        .unwrap();
    wait_for_partial_response(&mut events).await;
    agent.cancel();
    agent
        .prompt_with_kind(
            "New direction after cancel".into(),
            vec![],
            PromptKind::Steer,
            Some(3),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), released)
        .await
        .unwrap()
        .unwrap();
    let mut interrupted = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                FromAgent::TurnInterrupted { .. } => interrupted = true,
                FromAgent::TurnCompleted { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    assert!(
        interrupted,
        "the cancelled request must not report completion"
    );
    let requests = server.await.unwrap();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("New direction after cancel")
    );
}

#[tokio::test]
async fn cancellation_after_steer_discards_the_correction_without_restarting() {
    let (client, released, server) = stalled_provider(0).await;
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "openai/gpt-4o".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..Default::default()
    };
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).unwrap();
    agent
        .prompt("Original request".into(), vec![])
        .await
        .unwrap();
    wait_for_partial_response(&mut events).await;
    agent
        .prompt_with_kind(
            "Discard this correction".into(),
            vec![],
            PromptKind::Steer,
            Some(4),
        )
        .await
        .unwrap();
    agent.cancel();
    tokio::time::timeout(Duration::from_secs(2), released)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                FromAgent::TurnInterrupted { .. } => break,
                FromAgent::TurnCompleted { .. } => panic!("cancelled turn reported completion"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    agent.shutdown().await;
    assert_eq!(server.await.unwrap().len(), 1);
}
