use super::*;

#[tokio::test]
async fn unauthenticated_owner_and_reused_session_generation_fail_closed() {
    let session = crate::tests::test_session_record("session-1");
    let generation = session.created_at.clone();
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("session-1".into(), session)]));
    assert_eq!(
        binding_for(&state, &AuthContext::default(), "session-1", &generation)
            .await
            .unwrap_err()
            .status,
        401
    );
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    assert!(
        binding_for(&state, &auth, "session-1", &generation)
            .await
            .is_ok()
    );
    assert_eq!(
        binding_for(&state, &auth, "session-1", "reused-generation")
            .await
            .unwrap_err()
            .status,
        404
    );
}

#[tokio::test]
async fn attachment_admission_rejects_omitted_or_unconsumed_bytes_and_native_truncation() {
    use base64::Engine;
    let state = crate::tests::test_app_state_with_sessions(HashMap::new());
    for attachment in [
        serde_json::json!({"fileName":"missing.txt","contentOmitted":true}),
        serde_json::json!({"fileName":"binary.pdf","content":BASE64_STANDARD.encode([0xff,0xfe])}),
        serde_json::json!({"fileName":"oversized.txt","content":BASE64_STANDARD.encode("x".repeat(100_001))}),
    ] {
        let request:ChatRequest=serde_json::from_value(serde_json::json!({"messages":[{"role":"user","content":"inspect","attachments":[attachment]}]})).unwrap();
        assert!(
            validate_attachments(&request, &state, "provider/model")
                .await
                .is_err()
        );
    }
    let extracted:ChatRequest=serde_json::from_value(serde_json::json!({"messages":[{"role":"user","content":"inspect","attachments":[{"fileName":"binary.pdf","content":BASE64_STANDARD.encode([0xff,0xfe]),"extractedText":"actual extracted text"}]}]})).unwrap();
    assert!(
        validate_attachments(&extracted, &state, "provider/model")
            .await
            .is_ok()
    );
}

#[test]
fn prompt_text_limit_is_independent_of_attachment_wire_budget() {
    let request: ChatRequest = serde_json::from_value(
        serde_json::json!({"messages":[{"role":"user","content":"x".repeat(64*1024+1)}]}),
    )
    .unwrap();
    assert_eq!(
        validate_request(&request, "session-1").unwrap_err().status,
        400
    );
}

#[test]
fn discuss_rejects_client_tools_before_turn_acceptance() {
    let request: ChatRequest = serde_json::from_value(serde_json::json!({
        "messages":[{"role":"user","content":"discuss"}],
        "interactionMode":"discuss",
        "tools":[{"name":"write_file","description":"write","parameters":{}}]
    }))
    .unwrap();
    assert!(validate_request(&request, "session-1").is_err());
}

#[tokio::test]
async fn acceptance_cannot_replay_a_prompt_on_a_restarted_gateway() {
    let previous = NativeTurnRuntime::default();
    let session = crate::tests::test_session_record("session-1");
    let created_at = session.created_at.clone();
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([(session.id.clone(), session)]));
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    let body = serde_json::json!({
        "gatewayEpoch":previous.gateway_epoch(), "sessionId":"session-1",
        "sessionCreatedAt":created_at, "turnId":"retry-after-restart",
        "request":{"messages":[{"role":"user","content":"execute once"}]}
    })
    .to_string();
    let mut initial = format!(
        "POST /api/native/turns HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let head = crate::http::parse_request_head(&initial).unwrap();
    let (_client, mut stream) = crate::tests::tcp_stream_pair().await;
    let result = handle_endpoint(&mut stream, &mut initial, &head, &state, auth.clone()).await;
    assert_eq!(result.unwrap_err().status, 409);
    assert!(
        state
            .native_turns
            .entries
            .lock()
            .unwrap()
            .accepted
            .is_empty()
    );
    assert!(
        state.sessions.lock().await.sessions["session-1"]
            .messages
            .is_empty()
    );

    // Keep the execution lane occupied so acceptance can be tested without model calls.
    let binding = binding_for(&state, &auth, "session-1", &created_at)
        .await
        .unwrap();
    let request: ChatRequest = serde_json::from_value(serde_json::json!({
        "messages":[{"role":"user","content":"existing prompt"}]
    }))
    .unwrap();
    state
        .native_turns
        .accept(
            binding.clone(),
            "existing-turn".into(),
            request,
            auth.clone(),
            false,
            None,
        )
        .unwrap();
    state.native_turns.next(&binding.lane()).unwrap();
    let mut body: Value = serde_json::from_str(&body).unwrap();
    body["gatewayEpoch"] = Value::String(state.native_turns.gateway_epoch().into());
    let body = body.to_string();
    let mut initial = format!(
        "POST /api/native/turns HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let head = crate::http::parse_request_head(&initial).unwrap();
    let accepted = handle_endpoint(&mut stream, &mut initial, &head, &state, auth)
        .await
        .unwrap();
    assert_eq!(accepted["turnId"], "retry-after-restart");
    assert_eq!(accepted["state"], "queued");
    assert!(
        state.sessions.lock().await.sessions["session-1"]
            .messages
            .is_empty()
    );
    assert!(validate_gateway_epoch(None, &state).is_ok());
}

#[tokio::test]
async fn bound_approval_cannot_rebind_to_another_session() {
    let first = crate::tests::test_session_record("session-1");
    let second = crate::tests::test_session_record("session-2");
    let created_at = first.created_at.clone();
    let state = crate::tests::test_app_state_with_sessions(HashMap::from([
        (first.id.clone(), first),
        (second.id.clone(), second),
    ]));
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    let binding = binding_for(&state, &auth, "session-1", &created_at)
        .await
        .unwrap();
    let request: ChatRequest = serde_json::from_value(
        serde_json::json!({"messages":[{"role":"user","content":"implement"}]}),
    )
    .unwrap();
    state
        .native_turns
        .accept(
            binding,
            "turn-approval".into(),
            request,
            auth.clone(),
            false,
            None,
        )
        .unwrap();
    let mut input = NativeApprovalBinding {
        gateway_epoch: Some(state.native_turns.gateway_epoch().into()),
        session_id: "session-1".into(),
        session_created_at: created_at,
        turn_id: "turn-approval".into(),
        generation: 1,
    };
    assert!(bound_approval_turn(&state, &auth, &input).await.is_ok());
    input.session_id = "session-2".into();
    assert!(bound_approval_turn(&state, &auth, &input).await.is_err());
}

#[tokio::test]
async fn omitted_native_binding_cannot_dispatch_an_approval_after_stop() {
    let session = crate::tests::test_session_record("session-1");
    let created_at = session.created_at.clone();
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([(session.id.clone(), session)]));
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    let binding = binding_for(&state, &auth, "session-1", &created_at)
        .await
        .unwrap();
    let request: ChatRequest = serde_json::from_value(
        serde_json::json!({"messages":[{"role":"user","content":"implement"}]}),
    )
    .unwrap();
    let turn = state
        .native_turns
        .accept(
            binding.clone(),
            "turn-approval".into(),
            request,
            auth.clone(),
            false,
            None,
        )
        .unwrap()
        .turn;
    state.native_turns.next(&binding.lane()).unwrap();
    turn.publish(
        serde_json::json!({"type":"action_approval_required","request":{"id":"approval-1"}}),
    );
    let generation = turn.snapshot(&state.native_turns.epoch, 0, Vec::new())["generation"]
        .as_u64()
        .unwrap();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    state
        .pending_tool_responses
        .lock()
        .await
        .insert("approval-1".into(), sender);
    state
        .pending_tool_response_sessions
        .lock()
        .await
        .insert("approval-1".into(), turn.pending_request_owner());
    let mut bound_payload = serde_json::json!({
        "kind":"approval", "decision":"approved", "nativeTurn":{
            "sessionId":"session-1", "sessionCreatedAt":created_at,
            "turnId":"turn-approval", "generation":generation
        }
    });
    bound_payload["nativeTurn"]["generation"] = Value::from(generation + 1);
    let wrong_body = bound_payload.to_string();
    let mut wrong_initial = format!("POST /api/pending-requests/approval-1/resume HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}", wrong_body.len(), wrong_body).into_bytes();
    let wrong_head = crate::http::parse_request_head(&wrong_initial).unwrap();
    let (_client, mut wrong_stream) = crate::tests::tcp_stream_pair().await;
    let wrong_response = crate::sessions::handle_pending_request_resume_endpoint(
        &mut wrong_stream,
        &mut wrong_initial,
        &wrong_head,
        &state,
        &auth,
    )
    .await;
    assert!(
        String::from_utf8(wrong_response)
            .unwrap()
            .starts_with("HTTP/1.1 409")
    );
    assert!(receiver.try_recv().is_err());
    bound_payload["nativeTurn"]["generation"] = Value::from(generation);
    let bound_body = bound_payload.to_string();
    let mut bound_initial = format!("POST /api/pending-requests/approval-1/resume HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}", bound_body.len(), bound_body).into_bytes();
    let bound_head = crate::http::parse_request_head(&bound_initial).unwrap();
    let (_client, mut bound_stream) = crate::tests::tcp_stream_pair().await;
    let response = crate::sessions::handle_pending_request_resume_endpoint(
        &mut bound_stream,
        &mut bound_initial,
        &bound_head,
        &state,
        &auth,
    )
    .await;
    assert!(
        String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 200")
    );
    let decision = receiver.try_recv().unwrap();
    assert_eq!(decision.0, "approval-1");
    assert!(decision.1);
    assert!(decision.2.is_none());
    let (sender, mut receiver) = mpsc::unbounded_channel();
    state
        .pending_tool_responses
        .lock()
        .await
        .insert("approval-1".into(), sender);
    state
        .pending_tool_response_sessions
        .lock()
        .await
        .insert("approval-1".into(), turn.pending_request_owner());
    turn.stop(generation).unwrap();
    let body = r#"{"kind":"approval","decision":"approved"}"#;
    let mut initial = format!("POST /api/pending-requests/approval-1/resume HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_bytes();
    let head = crate::http::parse_request_head(&initial).unwrap();
    let (_client, mut stream) = crate::tests::tcp_stream_pair().await;
    let response = crate::sessions::handle_pending_request_resume_endpoint(
        &mut stream,
        &mut initial,
        &head,
        &state,
        &auth,
    )
    .await;
    assert!(
        String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 409")
    );
    assert!(receiver.try_recv().is_err());
    assert!(
        state
            .pending_tool_responses
            .lock()
            .await
            .contains_key("approval-1")
    );
}

#[tokio::test]
async fn old_epoch_control_is_rejected_before_any_owner_mutation() {
    let state = crate::tests::test_app_state_with_sessions(HashMap::new());
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    let body = serde_json::json!({"gatewayEpoch":"old-epoch","sessionId":"session","sessionCreatedAt":"created","turnId":"turn","generation":1,"operation":"stop"}).to_string();
    let mut initial = format!("POST /api/native/turns/turn/control HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_bytes();
    let head = crate::http::parse_request_head(&initial).unwrap();
    let (_client, mut stream) = crate::tests::tcp_stream_pair().await;
    let response = handle_endpoint(&mut stream, &mut initial, &head, &state, auth).await;
    assert_eq!(response.unwrap_err().status, 409);
    assert!(state.native_turns.entries.lock().unwrap().turns.is_empty());
}

#[tokio::test]
async fn stop_during_governed_http_observation_retains_terminal_saved_changes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _guard = crate::tests::ENV_LOCK.lock().await;
    let directory = tempfile::tempdir().unwrap();
    let mut session = crate::tests::test_session_record("session");
    session.created_at = "created".into();
    session.owner = Some("human".into());
    session.organization_id = Some("org".into());
    session.workspace_id = Some("workspace".into());
    session.messages = vec![serde_json::json!({"role":"user","content":"edit"})];
    let mut state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("session".into(), session)]));
    let mut config = (*state.config).clone();
    config.session_store_path = directory.path().join("sessions.json");
    state.config = Arc::new(config);
    let auth = AuthContext {
        subject: Some("human".into()),
        organization_id: Some("org".into()),
        workspace_id: Some("workspace".into()),
        source: AuthSource::IdentityJwt,
        ..Default::default()
    };
    let binding = binding_for(&state, &auth, "session", "created")
        .await
        .unwrap();
    let request =
        serde_json::from_value(serde_json::json!({"messages":[{"role":"user","content":"edit"}]}))
            .unwrap();
    let accepted = state
        .native_turns
        .accept(binding.clone(), "turn".into(), request, auth, false, None)
        .unwrap();
    state.native_turns.next(&binding.lane()).unwrap();
    let generation = accepted.turn.data.lock().unwrap().generation;
    let admission: crate::governed_native::Admission = serde_json::from_value(serde_json::json!({"admissionId":"admission","organizationId":"org","workspaceId":"workspace","subject":"human","runnerSessionId":"runner","applicationId":"deixic","agentId":"maestro","actorId":"human","gatewayEpoch":state.native_turns.gateway_epoch(),"nativeSessionId":"session","nativeSessionCreatedAt":"created","nativeTurnId":"turn","requestSha256":"digest","state":"active","expiresAt":"2100-01-01T00:00:00Z"})).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let path = directory.path().join("descriptor");
    let expiry = chrono::Utc::now().timestamp() + 300;
    let descriptor = serde_json::json!({"version":1,"organizationId":"org","workspaceId":"workspace","runnerSessionId":"runner","gateway":{"token":"gateway-token","expiresAtEpochSeconds":expiry},"toolExecution":{"baseUrl":origin,"platformBaseUrl":origin,"token":"tool-token","expiresAtEpochSeconds":expiry},"refresh":null});
    std::fs::write(&path, descriptor.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    let old_descriptor = env::var_os("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE");
    let old_gateway = env::var_os("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE");
    env::set_var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE", &path);
    env::set_var(
        "MAESTRO_EVALOPS_ACCESS_TOKEN_FILE",
        directory.path().join("gateway"),
    );
    let client = crate::governed_native::Client::from_env().unwrap();
    let key =
        maestro_runtime_contracts::native_code::checkpoint_key("org", "workspace", "admission");
    let (observing_sender, observing) = tokio::sync::oneshot::channel();
    let (complete, completion) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut execution = Value::Null;
        let mut observing_sender = Some(observing_sender);
        let mut completion = Some(completion);
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 1024];
            let body = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let header = std::str::from_utf8(&bytes[..end])
                    .unwrap()
                    .to_ascii_lowercase();
                let length: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() < end + 4 + length {
                    continue;
                }
                assert!(header.contains("authorization: bearer tool-token"));
                assert!(header.contains(if index == 0 {
                    "/executetool "
                } else {
                    "/gettoolexecution "
                }));
                break serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length]).unwrap();
            };
            if index == 0 {
                execution = body;
                execution["id"] = serde_json::json!("execution");
                execution["state"] = serde_json::json!("TOOL_EXECUTION_STATE_RUNNING");
            } else {
                assert_eq!(body["id"], "execution");
                observing_sender.take().unwrap().send(()).unwrap();
                completion.take().unwrap().await.unwrap();
                execution["state"] = serde_json::json!("TOOL_EXECUTION_STATE_SUCCEEDED");
                execution["output"] = serde_json::json!({"safeOutput":{"nativeCheckpointKey":key,"nativeChanges":{"availability":"ready","files":[{"path":"saved.txt","kind":"created","availability":"patch","beforeContent":null,"afterContent":"saved after Stop"}]}}});
            }
            let response = serde_json::json!({"execution":execution}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        }
    });
    let task_state = state.clone();
    let owner = accepted.turn.clone();
    let task = tokio::spawn(async move {
        crate::dex_chat::drive_governed_tool_for_test(
            &task_state,
            &client,
            &admission,
            owner,
            "session:created:1",
            &serde_json::json!({"command":"write file"}),
        )
        .await
    });
    observing.await.unwrap();
    accepted.turn.stop(generation).unwrap();
    assert!(accepted.turn.cancel.is_cancelled());
    assert!(
        !task.is_finished(),
        "Stop discarded the in-flight receipt observer"
    );
    complete.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task).await;
    match old_descriptor {
        Some(value) => env::set_var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE", value),
        None => env::remove_var("MAESTRO_NATIVE_CODE_DESCRIPTOR_FILE"),
    }
    match old_gateway {
        Some(value) => env::set_var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE", value),
        None => env::remove_var("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE"),
    }
    result.unwrap().unwrap().unwrap();
    server.await.unwrap();
    let (stored, valid) = load_session_store(&state.config.session_store_path).await;
    assert!(valid);
    let saved = crate::governed_changes::saved(&stored.sessions["session"], 0).unwrap();
    assert_eq!(saved["toolExecutionId"], "execution");
    assert_eq!(
        saved["changes"]["files"][0]["afterContent"],
        "saved after Stop"
    );
}
