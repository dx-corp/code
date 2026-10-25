#[allow(unused_imports)]
use super::*;

pub(crate) fn restore_env(snapshot: Vec<(&'static str, Option<std::ffi::OsString>)>) {
    for (name, value) in snapshot {
        if let Some(value) = value {
            env::set_var(name, value);
        } else {
            env::remove_var(name);
        }
    }
}

#[test]
pub(crate) fn unresolvable_bind_hosts_are_treated_as_network_exposed() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);
    env::set_var("MAESTRO_CONTROL_HOST", "maestro.internal.example");

    let config = Config::from_env();

    assert!(!config.listen_host_is_loopback());
    assert!(config.require_key);
    restore_env(snapshot);
}

pub(crate) fn capture_http_request(
    listener: &StdTcpListener,
    status_line: &str,
    response_body: &str,
) -> CapturedHttpRequest {
    let (mut stream, _) = listener
        .accept()
        .expect("test server should accept request");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream
            .read(&mut chunk)
            .expect("test server should read request");
        assert!(read > 0, "client closed before request headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header_text.split("\r\n").filter(|line| !line.is_empty());
    let request_line = lines
        .next()
        .expect("request should include request line")
        .to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect::<HashMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let read = stream
            .read(&mut chunk)
            .expect("test server should read request body");
        assert!(read > 0, "client closed before request body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = if content_length == 0 {
        serde_json::json!({})
    } else {
        serde_json::from_slice(&buffer[header_end..header_end + content_length])
            .expect("request body should be JSON")
    };
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("test server should write response");
    CapturedHttpRequest {
        request_line,
        headers,
        body,
    }
}

pub(crate) fn csrf_head(method: &str, token: Option<&str>) -> RequestHead {
    csrf_head_for_path(method, "/api/status", token)
}

pub(crate) fn csrf_head_for_path(method: &str, path: &str, token: Option<&str>) -> RequestHead {
    let mut headers = HashMap::new();
    if let Some(token) = token {
        headers.insert("x-maestro-csrf".to_string(), token.to_string());
    }
    RequestHead {
        method: method.to_string(),
        path: path.to_string(),
        query: HashMap::new(),
        headers,
    }
}

pub(crate) fn response_json(response: Vec<u8>) -> Value {
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| &response[index + 4..])
        .expect("response should contain header separator");
    serde_json::from_slice(body).expect("response body should be json")
}

pub(crate) fn response_status(response: &[u8]) -> u16 {
    let text = std::str::from_utf8(response).expect("response should be utf-8");
    let status = text
        .lines()
        .next()
        .expect("response should include status line")
        .split_whitespace()
        .nth(1)
        .expect("status line should include code");
    status.parse().expect("status code should parse")
}

#[test]
pub(crate) fn parses_request_path_without_query() {
    let request =
        b"GET /api/status?action=mark-onboarding-seen HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let head = parse_request_head(request).expect("request should parse");

    assert_eq!(head.method, "GET");
    assert_eq!(head.path, "/api/status");
    assert_eq!(
        head.query.get("action"),
        Some(&"mark-onboarding-seen".to_string())
    );
    assert_eq!(head.headers.get("host"), Some(&"localhost".to_string()));
}

#[test]
pub(crate) fn parses_percent_encoded_query_components() {
    let request =
            b"GET /api/artifact-access?filename=logs%2Ftoday%20one.txt&action=mark+done HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let head = parse_request_head(request).expect("request should parse");

    assert_eq!(
        head.query.get("filename"),
        Some(&"logs/today one.txt".to_string())
    );
    assert_eq!(head.query.get("action"), Some(&"mark done".to_string()));
}

pub(crate) fn head_with_host(host: Option<&str>) -> RequestHead {
    let request = match host {
        Some(host) => format!("GET /api/chat HTTP/1.1\r\nHost: {host}\r\n\r\n"),
        None => "GET /api/chat HTTP/1.1\r\n\r\n".to_string(),
    };
    parse_request_head(request.as_bytes()).expect("request should parse")
}

#[tokio::test]
pub(crate) async fn rebound_host_header_is_rejected_before_any_route_runs() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let mut config = Config::test_default();
    config.listen_host = "127.0.0.1".to_string();
    config.listen_port = addr.port();

    tokio::spawn(async move {
        let _ = serve_listener(listener, config).await;
    });

    async fn send(addr: std::net::SocketAddr, host: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET /healthz HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).to_string()
    }

    let rebound = send(addr, "evil.example").await;
    assert!(
        rebound.starts_with("HTTP/1.1 421 Misdirected Request"),
        "rebound Host should get 421, got: {rebound}"
    );
    assert!(!rebound.contains("ok\n"), "route must not run: {rebound}");

    let legitimate = send(addr, &format!("localhost:{}", addr.port())).await;
    assert!(
        legitimate.starts_with("HTTP/1.1 200 OK"),
        "legitimate Host should be served, got: {legitimate}"
    );
    assert!(legitimate.contains("ok\n"));
}

#[test]
pub(crate) fn combines_duplicate_headers_in_parse_request_head() {
    let request = b"GET /api/status HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 10.0.0.1\r\nx-forwarded-for: 10.0.0.2\r\nCookie: a=1\r\nCookie: b=2\r\n\r\n";
    let head = parse_request_head(request).expect("request should parse");

    assert_eq!(
        head.headers.get("x-forwarded-for"),
        Some(&"10.0.0.1, 10.0.0.2".to_string())
    );
    assert_eq!(head.headers.get("cookie"), Some(&"a=1, b=2".to_string()));
}

#[test]
pub(crate) fn pending_resume_path_decodes_url_encoded_request_ids() {
    assert_eq!(
        pending_request_id_from_resume_path(
            "/api/pending-requests/codex%3Asession-1%3Arun-1%3Aapproval-1/resume"
        )
        .as_deref(),
        Some("codex:session-1:run-1:approval-1")
    );
    assert!(pending_request_id_from_resume_path("/api/pending-requests/bad%2Fid/resume").is_none());
}

#[tokio::test]
pub(crate) async fn background_update_parses_body_and_returns_update_contract() {
    let _guard = ENV_LOCK.lock().await;
    let body = r#"{"enabled":true}"#;
    let request = format!(
        "POST /api/background?action=notify HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_local_endpoint(&mut server, &mut initial, head, &state).await);
    let settings = state.background_settings.lock().await.clone();
    let status = background_response(
        &RequestHead {
            method: "GET".to_string(),
            path: "/api/background".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
        },
        &settings,
    );

    assert_eq!(response.get("success").and_then(Value::as_bool), Some(true));
    assert_eq!(
        response.get("message").and_then(Value::as_str),
        Some("Background task notifications enabled.")
    );
    assert!(response.get("settings").is_none());
    assert_eq!(
        status
            .pointer("/settings/notificationsEnabled")
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
pub(crate) async fn framework_update_parses_body_and_returns_contract_shape() {
    let _guard = ENV_LOCK.lock().await;
    let body = r#"{"framework":"fastapi","scope":"workspace"}"#;
    let request = format!(
        "POST /api/framework HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_local_endpoint(&mut server, &mut initial, head, &state).await);
    let framework = state.framework_preference.lock().await.clone();
    let status = framework_response(
        &RequestHead {
            method: "GET".to_string(),
            path: "/api/framework".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
        },
        framework.as_deref(),
    );

    assert_eq!(response.get("success").and_then(Value::as_bool), Some(true));
    assert_eq!(
        response.get("framework").and_then(Value::as_str),
        Some("fastapi")
    );
    assert_eq!(
        response.get("scope").and_then(Value::as_str),
        Some("workspace")
    );
    assert!(response.get("summary").and_then(Value::as_str).is_some());
    assert_eq!(
        status.get("framework").and_then(Value::as_str),
        Some("fastapi")
    );
}

#[test]
pub(crate) fn builds_store_zip_archive_without_node_runtime() {
    let zip = build_store_zip([("artifact.txt", b"hello artifact".as_slice())])
        .expect("zip archive should build");

    assert!(zip.starts_with(&[0x50, 0x4b, 0x03, 0x04]));
    assert!(
        zip.windows("artifact.txt".len())
            .any(|window| window == b"artifact.txt")
    );
    assert!(
        zip.windows("hello artifact".len())
            .any(|window| window == b"hello artifact")
    );
    assert!(
        zip.windows(4)
            .any(|window| window == [0x50, 0x4b, 0x05, 0x06])
    );
}

#[tokio::test]
pub(crate) async fn cors_response_allows_platform_organization_header() {
    let _guard = ENV_LOCK.lock().await;

    let response = with_response_cors_origin("http://localhost:3000".to_string(), async {
        response(204, "text/plain; charset=utf-8", &[])
    })
    .await;
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.contains("x-organization-id"));
    assert!(response.contains("x-a2a-notification-token"));
}

#[test]
pub(crate) fn head_response_keeps_get_content_length_without_body() {
    let _guard = ENV_LOCK.blocking_lock();

    let response = response_with_extra_headers_and_length(
        200,
        "text/plain; charset=utf-8",
        &[],
        "Cache-Control: no-store, no-cache, must-revalidate\r\n",
        "hello".len(),
    );
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.contains("Content-Length: 5\r\n"));
    assert!(response.ends_with("\r\n\r\n"));
}

#[test]
#[should_panic(expected = "response Content-Length 10 does not match body length 5")]
pub(crate) fn response_with_length_rejects_nonempty_body_length_mismatch() {
    let _response = response_with_extra_headers_and_length(200, "text/plain", b"hello", "", 10);
}

#[test]
pub(crate) fn parse_git_status_counts_both_porcelain_columns() {
    let status = parse_git_status(
        " M unstaged.txt\nM  staged.txt\nMM both.txt\nAM added_modified.txt\nD  staged_delete.txt\n D worktree_delete.txt\n R renamed.txt\n C copied.txt\nUU conflicted.txt\n?? new.txt\n",
    );

    assert_eq!(status.modified, 5);
    assert_eq!(status.added, 3);
    assert_eq!(status.deleted, 2);
    assert_eq!(status.untracked, 1);
    assert_eq!(status.total, 10);
}

#[test]
pub(crate) fn json_response_has_header_body_separator() {
    let _guard = ENV_LOCK.blocking_lock();

    let response = json_response(200, &serde_json::json!({ "ok": true }));

    assert!(response.windows(4).any(|window| window == b"\r\n\r\n"));
    let response = String::from_utf8(response).expect("response should be utf-8");
    assert!(response.contains("x-composer-csrf"));
    assert!(response.contains("x-maestro-artifact-access"));
}

#[test]
pub(crate) fn json_response_uses_conflict_reason_phrase() {
    let _guard = ENV_LOCK.blocking_lock();

    let response = json_response(409, &serde_json::json!({ "error": "Conflict" }));
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.starts_with("HTTP/1.1 409 Conflict\r\n"));
}

#[test]
pub(crate) fn rejects_missing_request_target() {
    let request = b"GET\r\nHost: localhost\r\n\r\n";

    assert!(parse_request_head(request).is_err());
}

// Items 1, 2, 3, 4 (tenant binding): on origin/main none of these paths were in
// `runtime_tenant_resource_path`, so `authorize` returned Ok for a bare
// write-scoped principal with no tenant. This asserts they now fail closed.
#[test]
pub(crate) fn extended_admin_mcp_and_pending_paths_require_tenant_binding_on_remote() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = security_fix_remote_env_snapshot();

    let mut remote_config = auth_test_config();
    remote_config.listen_host = "0.0.0.0".to_string();

    let write_no_tenant = security_fix_token(serde_json::json!({
        "sub": "writer",
        "scope": "maestro:write",
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
        "exp": now_secs() + 60,
    }));

    for (method, path) in [
        ("POST", "/api/mcp"),
        ("GET", "/api/automations"),
        ("POST", "/api/automations"),
        ("GET", "/api/workspace-configs"),
        ("POST", "/api/admin/enterprise-policy/publish"),
        ("GET", "/api/admin/enterprise-policy/status"),
        ("POST", "/api/pending-requests/req-1/resume"),
        ("GET", "/api/hosted-threads/thread:one"),
        ("POST", "/api/hosted-threads/thread:one/messages"),
    ] {
        assert!(
            authorize(&auth_head(method, path, &write_no_tenant), &remote_config).is_err(),
            "{method} {path} must require a tenant-bound principal"
        );
    }

    let tenant_write = security_fix_token(serde_json::json!({
        "sub": "writer",
        "organization_id": "org-a",
        "workspace_id": "workspace-a",
        "scope": "maestro:write",
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
        "exp": now_secs() + 60,
    }));
    assert!(
        authorize(
            &auth_head("GET", "/api/automations", &tenant_write),
            &remote_config
        )
        .is_ok(),
        "tenant-bound write principal retains extended read access"
    );

    restore_env(snapshot);
}
