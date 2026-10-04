use super::*;

fn shared() -> (tempfile::TempDir, SharedRunner) {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = HostedRunnerConfig::new("runner", workspace.path()).unwrap();
    config.workload_identity = Some(config::HostedRunnerWorkloadIdentityConfig {
        kubernetes_token_file: "/token".into(),
        identity_tls_ca_file: "/ca".into(),
        identity_exchange_url: "https://identity.test/exchange".parse().unwrap(),
        organization_id: "org".into(),
        workspace_id: "workspace".into(),
        sandbox_id: Uuid::nil(),
        placement_generation: 1,
    });
    (workspace, SharedRunner::new(config))
}
fn request() -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        path: "/api/native-code/api/native/turns".into(),
        query: HashMap::new(),
        raw_query: Some("cursor=a%2Fb&limit=100".into()),
        headers: HashMap::from([
            ("x-auth-request-user".into(), "alice".into()),
            ("x-evalops-organization-id".into(), "org".into()),
            ("x-evalops-workspace-id".into(), "workspace".into()),
            ("x-auth-request-scope".into(), "console:write".into()),
            ("authorization".into(), "Bearer platform-secret".into()),
            ("x-maestro-proxy-auth".into(), "injected".into()),
        ]),
        body: br#"{"turnId":"one","request":{"messages":[{"content":"hello"}]}}"#.to_vec(),
    }
}

#[tokio::test]
async fn native_code_requires_workload_mtls_and_exact_tenant() {
    let (_workspace, shared) = shared();
    assert!(
        proxy_inner(
            request(),
            &shared,
            false,
            Some("http://127.0.0.1:9"),
            Some("secret")
        )
        .await
        .is_err()
    );
    let mut input = request();
    input
        .headers
        .insert("x-evalops-workspace-id".into(), "foreign".into());
    assert!(
        proxy_inner(
            input,
            &shared,
            true,
            Some("http://127.0.0.1:9"),
            Some("secret")
        )
        .await
        .is_err()
    );
    let mut input = request();
    input.path = "/api/native-code/api/admin/policy".into();
    assert!(
        proxy_inner(
            input,
            &shared,
            true,
            Some("http://127.0.0.1:9"),
            Some("secret")
        )
        .await
        .is_err()
    );
    assert!(
        proxy_inner(
            request(),
            &shared,
            true,
            Some("https://untrusted.test"),
            Some("secret")
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn native_code_forwards_exact_bytes_and_scoped_identity_without_caller_tokens() {
    let (_workspace, shared) = shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let child = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let input = read_request(&mut socket).await.unwrap().unwrap();
        assert_eq!(input.path, "/api/native/turns");
        assert_eq!(input.raw_query.as_deref(), Some("cursor=a%2Fb&limit=100"));
        assert_eq!(input.body, request().body);
        assert_eq!(input.headers["x-maestro-proxy-auth"], "child-secret");
        assert_eq!(input.headers["x-auth-request-user"], "alice");
        assert_eq!(input.headers["x-evalops-workspace-id"], "workspace");
        assert!(!input.headers.contains_key("authorization"));
        socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"ok\":true}\n").await.unwrap();
    });
    let result = proxy_inner(
        request(),
        &shared,
        true,
        Some(&endpoint),
        Some("child-secret"),
    )
    .await
    .unwrap();
    match result {
        ResponseBody::Bytes { status, body, .. } => {
            assert_eq!(status, 202);
            assert_eq!(body, b"{\"ok\":true}\n");
        }
        _ => panic!("expected exact polling response"),
    }
    child.await.unwrap();
}

#[tokio::test]
async fn native_code_never_follows_redirects() {
    let (_workspace, shared) = shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let child = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await.unwrap();
        socket.write_all(b"HTTP/1.1 307 Redirect\r\nLocation: http://untrusted.test/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let result = proxy_inner(
        request(),
        &shared,
        true,
        Some(&endpoint),
        Some("child-secret"),
    )
    .await
    .unwrap();
    assert!(matches!(result, ResponseBody::Bytes { status: 307, .. }));
    child.await.unwrap();
}

#[tokio::test]
async fn native_code_read_scope_can_poll_but_cannot_submit() {
    let (_workspace, shared) = shared();
    let mut input = request();
    input
        .headers
        .insert("x-auth-request-scope".into(), "console:read".into());
    assert!(
        proxy_inner(
            input,
            &shared,
            true,
            Some("http://127.0.0.1:9"),
            Some("secret")
        )
        .await
        .is_err()
    );
    assert!(route_allowed("GET", "api/sessions/session/page"));
    assert!(!route_allowed("POST", "api/sessions/session/page"));
}

#[tokio::test]
async fn native_code_request_body_is_bounded_before_forwarding() {
    let (_workspace, shared) = shared();
    let mut input = request();
    input.body = vec![0; REQUEST_LIMIT + 1];
    assert!(
        proxy_inner(
            input,
            &shared,
            true,
            Some("http://127.0.0.1:9"),
            Some("secret")
        )
        .await
        .is_err()
    );
    let (mut client, mut server) = tokio::io::duplex(4096);
    let sender = tokio::spawn(async move {
        client.write_all(b"POST /api/native-code/api/native/turns HTTP/1.1\r\nContent-Length: 2097153\r\n\r\n").await.unwrap();
    });
    assert!(read_request(&mut server).await.is_err());
    sender.await.unwrap();
}

#[tokio::test]
async fn native_code_cannot_admit_writes_while_resident_is_draining() {
    let (_workspace, shared) = shared();
    shared.state.lock().unwrap().draining = true;
    let result = proxy_inner(
        request(),
        &shared,
        true,
        Some("http://127.0.0.1:9"),
        Some("secret"),
    )
    .await;
    assert!(matches!(
        result,
        Err(HostedError {
            code: HostedRunnerErrorCode::RuntimeNotReady,
            ..
        })
    ));
}
