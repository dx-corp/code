use super::*;

#[tokio::test]
async fn managed_gateway_retry_open_failures_emit_one_terminal_without_a_fourth_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock managed gateway");
    let address = listener.local_addr().expect("mock gateway address");
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for attempt in 1..=3 {
            let (mut stream, _) = listener.accept().await.expect("gateway request");
            let _ = read_scripted_provider_request(&mut stream).await;
            server_requests.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                let body = r#"{"error":{"type":"server_error","message":"operation timed out"}}"#;
                let wire = format!(
                    "HTTP/1.1 504 Gateway Timeout\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream
                    .write_all(wire.as_bytes())
                    .await
                    .expect("gateway timeout response");
            }
            // Later attempts model the production response-open failure:
            // the gateway accepted the request but closed before headers.
        }
    });

    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        model: "evalops/openai/gpt-5.6-terra".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::from_model_with_env(
        "evalops/openai/gpt-5.6-terra",
        &HashMap::from([
            (
                "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
                "delegated-token".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_BASE_URL".to_string(),
                format!("http://{address}/v1"),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".to_string(), "org-test".to_string()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".to_string(),
                "workspace-test".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_PROVIDER".to_string(),
                "openrouter".to_string(),
            ),
            (
                "MAESTRO_EVALOPS_ENVIRONMENT".to_string(),
                "production".to_string(),
            ),
        ]),
    )
    .expect("managed gateway client");
    let (agent, mut events) =
        NativeAgent::new_with_test_client(config, client).expect("hosted agent");
    agent
        .set_session_context(Some("managed-retry-session".to_owned()), "test", false)
        .expect("session context");

    agent
        .prompt("return a terminal outcome".to_owned(), vec![])
        .await
        .expect("hosted prompt");
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ProviderError { kind, message }) => break (kind, message),
                Some(FromAgent::TurnCompleted { .. }) => {
                    panic!("failed gateway turn must not complete successfully")
                }
                Some(_) => {}
                None => panic!("agent event channel closed before provider terminal"),
            }
        }
    })
    .await
    .expect("provider terminal timeout");
    agent.shutdown().await;
    server.await.expect("mock gateway server");

    assert_eq!(terminal.0, ProviderStreamErrorKind::TransientProtocol);
    assert!(terminal.1.contains("gateway response") || terminal.1.contains("request"));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "the managed stream budget is exactly three total requests; the native outer loop must not start a fourth"
    );
}
