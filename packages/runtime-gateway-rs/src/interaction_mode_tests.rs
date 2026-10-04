use super::*;

#[test]
fn plan_endpoint_is_not_registered() {
    for method in ["GET", "POST"] {
        let head = RequestHead {
            method: method.to_string(),
            path: "/api/plan".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
        };
        assert!(!is_extended_endpoint(&head));
    }
}

fn remote_token(secret: &[u8], write: bool) -> String {
    hs256_signed_claims(
        secret,
        serde_json::json!({
            "sub":"user-remote", "organization_id":"org-remote", "workspace_id":"ws-remote",
            "scopes": if write { vec!["maestro:write"] } else { vec!["maestro:read"] },
            "exp":now_millis()/1000 + 60
        }),
    )
}

#[tokio::test]
async fn remote_capabilities_bind_verified_principal_and_selected_model() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);
    env::set_var("MAESTRO_JWT_SECRET", "remote-test-secret");
    let state = test_app_state_with_sessions(HashMap::new());
    {
        let mut selected = state.selected_model.lock().await;
        selected.provider = "openai".into();
        selected.id = "gpt-5.6".into();
    }
    let token = remote_token(b"remote-test-secret", false);
    let (_client, mut server) = tcp_stream_pair().await;
    let head = auth_head("GET", "/api/native/capabilities", &token);
    assert!(is_local_endpoint(&head));
    let value =
        response_json(handle_local_endpoint(&mut server, &mut Vec::new(), head, &state).await);
    assert_eq!(
        value["principal"],
        serde_json::json!({
            "subject":"user-remote", "organizationId":"org-remote", "workspaceId":"ws-remote"
        })
    );
    assert_eq!(value["workspacePath"], ".");
    assert_eq!(value["version"], 1);
    assert_eq!(value["gatewayEpoch"], state.native_turns.gateway_epoch());
    assert_eq!(
        value["interactionModes"],
        serde_json::json!(["discuss", "implement"])
    );
    {
        let mut selected = state.selected_model.lock().await;
        selected.provider = "openai-codex".into();
        selected.id = "gpt-5-codex".into();
    }
    let head = auth_head("GET", "/api/native/capabilities", &token);
    let value =
        response_json(handle_local_endpoint(&mut server, &mut Vec::new(), head, &state).await);
    assert_eq!(value["modelId"], "openai-codex/gpt-5-codex");
    assert_eq!(value["interactionModes"], serde_json::json!(["implement"]));
    for token in ["api-key", "invalid-token"] {
        let head = auth_head("GET", "/api/native/capabilities", token);
        let response = handle_local_endpoint(&mut server, &mut Vec::new(), head, &state).await;
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with(if token == "api-key" {
            "HTTP/1.1 403"
        } else {
            "HTTP/1.1 401"
        }));
    }
    restore_env(snapshot);
}

#[tokio::test]
async fn discuss_codex_default_rejects_before_acceptance_and_message_persistence() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);
    env::set_var("MAESTRO_JWT_SECRET", "remote-test-secret");
    let mut session = test_session_record("remote-session");
    session.owner = Some("user-remote".into());
    session.organization_id = Some("org-remote".into());
    session.workspace_id = Some("ws-remote".into());
    let created_at = session.created_at.clone();
    let state = test_app_state_with_sessions(HashMap::from([(session.id.clone(), session)]));
    {
        let mut selected = state.selected_model.lock().await;
        selected.provider = "openai-codex".into();
        selected.id = "gpt-5-codex".into();
    }
    let body = serde_json::json!({
        "sessionId":"remote-session", "sessionCreatedAt":created_at,
        "turnId":"remote-discuss", "request":{
            "interactionMode":"discuss", "messages":[{"role":"user","content":"change source.txt"}]
        }
    })
    .to_string();
    let mut initial = format!("POST /api/native/turns HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{}", remote_token(b"remote-test-secret", true), body.len(), body).into_bytes();
    let head = parse_request_head(&initial).unwrap();
    let (_client, mut server) = tcp_stream_pair().await;
    let response = handle_local_endpoint(&mut server, &mut initial, head, &state).await;
    assert!(
        String::from_utf8(response.clone())
            .unwrap()
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        response_json(response)["error"]
            .as_str()
            .unwrap()
            .contains("Discuss is unavailable")
    );
    assert!(
        state.sessions.lock().await.sessions["remote-session"]
            .messages
            .is_empty()
    );
    restore_env(snapshot);
}

#[test]
fn interaction_mode_defaults_to_implement_and_unknown_values_fail_closed() {
    let legacy: ChatRequest = serde_json::from_value(serde_json::json!({"messages":[]})).unwrap();
    assert_eq!(
        legacy.interaction_mode,
        crate::chat::InteractionMode::Implement
    );
    assert!(
        serde_json::from_value::<ChatRequest>(
            serde_json::json!({"messages":[],"interactionMode":"plan-ish"})
        )
        .is_err()
    );
}

#[tokio::test]
async fn hosted_companion_rejects_nonproxy_and_ownerless_auth_and_local_implementation() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);
    let prior = env::var_os("MAESTRO_NATIVE_CODE_COMPANION");
    env::set_var("MAESTRO_NATIVE_CODE_COMPANION", "1");
    env::set_var("MAESTRO_WEB_API_KEY", "must-not-authorize");
    env::set_var("MAESTRO_JWT_SECRET", "remote-test-secret");
    env::set_var(
        "MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN",
        "private-companion-secret",
    );
    let config = Config::from_env();
    let token = remote_token(b"remote-test-secret", true);
    assert!(authorized_context(&auth_head("GET", "/api/sessions", &token), &config).is_err());
    let mut proxy = auth_head("GET", "/api/sessions", "");
    proxy.headers.insert(
        "x-maestro-proxy-auth".into(),
        "private-companion-secret".into(),
    );
    proxy
        .headers
        .insert("x-auth-request-user".into(), "user-remote".into());
    proxy.headers.insert(
        "x-auth-request-scope".into(),
        "maestro:read maestro:write".into(),
    );
    assert!(authorized_context(&proxy, &config).is_err());
    proxy
        .headers
        .insert("x-organization-id".into(), "org-remote".into());
    proxy
        .headers
        .insert("x-workspace-id".into(), "ws-remote".into());
    assert!(authorized_context(&proxy, &config).is_ok());
    let mut state = test_app_state_with_sessions(HashMap::new());
    state.config = Arc::new(config);
    proxy.path = "/api/native/capabilities".into();
    let (_client, mut stream) = tcp_stream_pair().await;
    let capabilities =
        response_json(handle_local_endpoint(&mut stream, &mut Vec::new(), proxy, &state).await);
    assert_eq!(capabilities["interactionModes"], serde_json::json!([]));
    {
        let mut selected = state.selected_model.lock().await;
        selected.provider = "openai".into();
        selected.id = "gpt-5.6".into();
    }
    let mut proxy = auth_head("GET", "/api/native/capabilities", "");
    for (name, value) in [
        ("x-maestro-proxy-auth", "private-companion-secret"),
        ("x-auth-request-user", "user-remote"),
        ("x-auth-request-scope", "maestro:read maestro:write"),
        ("x-organization-id", "org-remote"),
        ("x-workspace-id", "ws-remote"),
    ] {
        proxy.headers.insert(name.into(), value.into());
    }
    let capabilities = response_json(
        handle_local_endpoint(&mut stream, &mut Vec::new(), proxy.clone(), &state).await,
    );
    assert_eq!(
        capabilities["interactionModes"],
        serde_json::json!(["discuss"])
    );
    let body = serde_json::json!({
        "sessionId":"unaccepted-session", "sessionCreatedAt":"unaccepted-generation", "turnId":"unaccepted-implementation",
        "request":{"interactionMode":"implement","messages":[{"role":"user","content":"write source.txt"}]}
    }).to_string();
    proxy.method = "POST".into();
    proxy.path = "/api/native/turns".into();
    proxy
        .headers
        .insert("content-length".into(), body.len().to_string());
    let mut initial = format!(
        "POST /api/native/turns HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let response = handle_local_endpoint(&mut stream, &mut initial, proxy, &state).await;
    assert!(
        String::from_utf8(response.clone())
            .unwrap()
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        response_json(response)["error"]
            .as_str()
            .unwrap()
            .contains("Platform turn admission")
    );
    assert!(state.sessions.lock().await.sessions.is_empty());
    restore_env(snapshot);
    match prior {
        Some(value) => env::set_var("MAESTRO_NATIVE_CODE_COMPANION", value),
        None => env::remove_var("MAESTRO_NATIVE_CODE_COMPANION"),
    }
}
