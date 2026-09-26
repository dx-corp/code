use super::*;

#[tokio::test]
async fn managed_tool_continuation_uses_fresh_invocation_authority() {
    assert_managed_invocation_renewal(false).await;
}

#[tokio::test]
async fn managed_transport_retry_and_tool_continuation_use_fresh_authority() {
    assert_managed_invocation_renewal(true).await;
}

async fn assert_managed_invocation_renewal(fail_first_open: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        let mut consumed = HashSet::new();
        let mut successful_invocations = 0;
        while let Ok((mut stream, _)) = listener.accept().await {
            let request = read_scripted_provider_request(&mut stream).await;
            let authorization =
                request["managed_inference_authorization"]["claims"]["authorization_id"]
                    .as_str()
                    .expect("invocation authority")
                    .to_owned();
            let first_attempt = consumed.is_empty();
            let fresh = consumed.insert(authorization);
            captured.lock().unwrap().push(request);
            let (status, content_type, body) = if fresh && first_attempt && fail_first_open {
                ("503 Service Unavailable", "application/json", serde_json::json!({"error": {
                    "code": "provider_unavailable", "message": "injected failure after admission"
                }}).to_string())
            } else if fresh {
                let first = successful_invocations == 0;
                successful_invocations += 1;
                (
                    "200 OK",
                    "text/event-stream",
                    chat_sse_response("managed-round", if first { "" } else { "done" }, first),
                )
            } else {
                (
                    "409 Conflict",
                    "application/json",
                    serde_json::json!({"error": {
                        "code": "managed_authorization_replay",
                        "message": "managed inference authorization has already been consumed"
                    }})
                    .to_string(),
                )
            };
            let wire = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nX-Request-ID: request-round\r\nX-EvalOps-Record-ID: record-round\r\nX-EvalOps-Lineage-ID: lineage-round\r\nX-EvalOps-Record-Status: planned\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            if stream.write_all(wire.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let config = NativeAgentConfig {
        model: "evalops/openai/gpt-5.6-terra".to_owned(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        max_turn_steps: 4,
        ..NativeAgentConfig::default()
    };
    let client = UnifiedClient::from_model_with_env(
        &config.model,
        &HashMap::from([
            ("MAESTRO_EVALOPS_ACCESS_TOKEN".into(), "test-token".into()),
            (
                "MAESTRO_EVALOPS_BASE_URL".into(),
                format!("http://{address}/v1"),
            ),
            ("MAESTRO_EVALOPS_ORG_ID".into(), "org-test".into()),
            (
                "MAESTRO_EVALOPS_WORKSPACE_ID".into(),
                "workspace-test".into(),
            ),
            ("MAESTRO_EVALOPS_PROVIDER".into(), "openrouter".into()),
            ("MAESTRO_EVALOPS_ENVIRONMENT".into(), "production".into()),
        ]),
    )
    .unwrap();
    let host = RuntimeTestHost::new(config.cwd.clone(), client);
    let (agent, mut events) = new_runtime_test_agent_with_host(config, host).unwrap();
    let authorization = serde_json::json!({
        "claims": {
            "authorization_id": "initial-invocation",
            "endpoint": "chat.completions",
            "lineage_id": "lineage-round",
            "session_id": "session-round", "thread_id": "thread-round",
            "run_id": "run-round", "turn_id": "turn-round",
            "model": "openai/gpt-5.6-terra",
            "providerCandidates": [{"provider": "openrouter", "environment": "production",
                "credentialName": "default", "teamId": "", "model": "openai/gpt-5.6-terra"}],
            "routing": "ordered",
            "output_token_budget": {"value": 4096, "origin": "route_policy",
                "origin_reference": "managed-round-test"}
        },
        "signature": "test-signature"
    });
    agent
        .prompt_with_kind_and_managed_context(
            "read then answer".into(),
            Vec::new(),
            PromptKind::Prompt,
            None,
            Some("lineage-round".into()),
            Some(ManagedInferenceAuthorization::new(
                authorization.to_string(),
            )),
        )
        .await
        .unwrap();
    let gateway_credential =
        maestro_runtime_contracts::ManagedGatewayCredential::new("test-token", i64::MAX);
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(FromAgent::ManagedAuthorizationRequest { request_id }) => {
                    let mut renewed = authorization.clone();
                    renewed["claims"]["authorization_id"] = request_id.clone().into();
                    agent
                        .managed_authorization_coordinator()
                        .respond(
                            &request_id,
                            ManagedInferenceAuthorization::new(renewed.to_string()),
                            Some(gateway_credential.clone()),
                        )
                        .unwrap();
                }
                Some(FromAgent::TurnCompleted { .. }) => break Ok(()),
                Some(FromAgent::ProviderError { message, .. }) => break Err(message),
                Some(FromAgent::Error {
                    message,
                    terminal: true,
                    ..
                }) => break Err(message),
                Some(_) => {}
                None => break Err("agent closed before completing the continuation".into()),
            }
        }
    })
    .await;
    agent.shutdown().await;
    server.abort();
    let _ = server.await;
    assert!(result.is_ok(), "managed continuation timed out");
    assert_eq!(
        result.unwrap(),
        Ok(()),
        "a tool continuation needs fresh authority"
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        if fail_first_open { 3 } else { 2 }
    );
}
