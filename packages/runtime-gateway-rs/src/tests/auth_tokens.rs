#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn control_plane_defaults_to_loopback_bind() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);

    let config = Config::from_env();

    assert_eq!(config.listen_host, "127.0.0.1");
    assert!(!config.require_key);
    assert!(config.validate_startup().is_ok());
    restore_env(snapshot);
}

#[test]
pub(crate) fn control_plane_defaults_to_auth_on_non_loopback_bind() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);
    env::set_var("MAESTRO_CONTROL_HOST", "0.0.0.0");

    let config = Config::from_env();

    assert!(config.require_key);
    assert!(config.validate_startup().is_err());
    restore_env(snapshot);
}

#[test]
pub(crate) fn require_key_kill_switch_is_ignored_on_non_loopback_bind() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);
    env::set_var("MAESTRO_CONTROL_HOST", "0.0.0.0");
    env::set_var("MAESTRO_WEB_REQUIRE_KEY", "0");

    let config = Config::from_env();

    assert!(
        config.require_key,
        "MAESTRO_WEB_REQUIRE_KEY=0 must not disable auth on a non-loopback bind"
    );
    let error = config
        .validate_startup()
        .expect_err("non-loopback bind with the kill switch must refuse to start");
    assert!(
        error
            .to_string()
            .contains("only honored for loopback binds"),
        "unexpected startup error: {error}"
    );

    // An API key does not make the combination acceptable either.
    env::set_var("MAESTRO_WEB_API_KEY", "secret");
    let config = Config::from_env();
    assert!(config.require_key);
    assert!(config.validate_startup().is_err());

    restore_env(snapshot);
}

#[test]
pub(crate) fn require_key_kill_switch_still_works_on_loopback_binds() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);

    for host in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
        clear_env(RUNTIME_GATEWAY_ENV_NAMES);
        env::set_var("MAESTRO_CONTROL_HOST", host);
        env::set_var("MAESTRO_WEB_REQUIRE_KEY", "0");

        let config = Config::from_env();

        assert!(
            config.listen_host_is_loopback(),
            "{host} should be loopback"
        );
        assert!(!config.require_key, "{host} should honor the kill switch");
        assert!(config.validate_startup().is_ok(), "{host} should start");
    }

    restore_env(snapshot);
}

#[test]
pub(crate) fn control_plane_requires_api_key_when_auth_required() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);

    env::set_var("MAESTRO_WEB_REQUIRE_KEY", "1");
    let config = Config::from_env();

    assert_eq!(
        config.validate_startup().unwrap_err().to_string(),
        "web auth is required because MAESTRO_WEB_REQUIRE_KEY is enabled; set MAESTRO_WEB_API_KEY, MAESTRO_JWT_SECRET, MAESTRO_JWT_JWKS_URL, or MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN"
    );

    env::set_var("MAESTRO_WEB_API_KEY", "secret");
    let config = Config::from_env();
    assert!(config.validate_startup().is_ok());
    restore_env(snapshot);
}

#[test]
pub(crate) fn header_auth_matches_accepts_right_key_and_rejects_wrong_same_length_key() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(RUNTIME_GATEWAY_ENV_NAMES);
    clear_env(RUNTIME_GATEWAY_ENV_NAMES);

    env::set_var("MAESTRO_WEB_API_KEY", "correct-horse-battery");
    let config = Config::from_env();

    // Right key accepted via the Authorization bearer position.
    assert!(crate::auth::header_auth_matches(
        &config,
        Some("correct-horse-battery"),
        None
    ));
    // Right key accepted via the x-maestro-api-key header position.
    assert!(crate::auth::header_auth_matches(
        &config,
        None,
        Some("correct-horse-battery")
    ));
    // A wrong key of the same byte length is rejected in both positions.
    assert!(!crate::auth::header_auth_matches(
        &config,
        Some("correct-horse-batterX"),
        None
    ));
    assert!(!crate::auth::header_auth_matches(
        &config,
        None,
        Some("correct-horse-batterX")
    ));
    // A wrong key of a different length is rejected.
    assert!(!crate::auth::header_auth_matches(
        &config,
        Some("short"),
        None
    ));
    // A missing header is a clean deny.
    assert!(!crate::auth::header_auth_matches(&config, None, None));

    restore_env(snapshot);
}

#[test]
pub(crate) fn env_bool_requires_affirmative_token() {
    let _guard = ENV_LOCK.blocking_lock();
    const NAME: &str = "MAESTRO_TEST_ENV_BOOL_STRICT";
    const NAMES: &[&str] = &[NAME];
    let snapshot = snapshot_env(NAMES);
    env::remove_var(NAME);

    // Unset reads as None; callers default it to false.
    assert_eq!(env_bool(NAME), None);

    // Only the affirmative tokens are true, case-insensitively.
    for value in ["1", "true", "TRUE", "yes", "Yes", "on", "ON"] {
        env::set_var(NAME, value);
        assert_eq!(env_bool(NAME), Some(true), "value {value:?} should be true");
    }

    // Explicit negative tokens are false.
    for value in ["0", "false", "no", "off"] {
        env::set_var(NAME, value);
        assert_eq!(
            env_bool(NAME),
            Some(false),
            "value {value:?} should be false"
        );
    }

    // Empty, whitespace, and unknown values are None, never true. The old
    // truthy_env treated every one of these except "" as true, silently
    // enabling gated switches on a typo or an intended-off value.
    for value in ["", "  ", "disabled", "none", "flase", "2", "enabled"] {
        env::set_var(NAME, value);
        assert_eq!(env_bool(NAME), None, "value {value:?} should be None");
        assert!(
            !env_bool(NAME).unwrap_or(false),
            "value {value:?} must not be true"
        );
    }

    restore_env(snapshot);
}

pub(crate) fn auth_test_config() -> Config {
    Config {
        native_code_hosted: false,
        listen_host: "127.0.0.1".to_string(),
        listen_port: 0,
        api_key: Some("api-key".to_string()),
        allowed_hosts: Vec::new(),
        require_key: true,
        require_key_explicitly_disabled: false,
        csrf_token: None,
        require_csrf: false,
        cwd: PathBuf::from("."),
        session_store_path: PathBuf::from("sessions.json"),
        session_messages_path: unique_test_dir("maestro-session-messages")
            .join("session-messages.json"),
        command_prefs_path: PathBuf::from("command-prefs.json"),
        usage_file_path: PathBuf::from("usage.jsonl"),
        a2a_tasks_file_path: unique_test_dir("maestro-a2a-tasks").join("tasks.json"),
        automation_file_path: unique_test_dir("maestro-automations").join("automations.json"),
        llm_gateway_models_url: None,
        llm_gateway_token: None,
        llm_gateway_org_id: None,
        llm_gateway_timeout_ms: 2_500,
    }
}

pub(crate) fn bearer_head(token: &str) -> RequestHead {
    RequestHead {
        method: "GET".to_string(),
        path: "/api/status".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([("authorization".to_string(), format!("Bearer {token}"))]),
    }
}

pub(crate) fn identity_jwt_bearer_token(secret: &[u8], user_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .saturating_add(60);
    let payload = URL_SAFE_NO_PAD.encode(format!(
        r#"{{"sub":"{user_id}","organization_id":"org_test","workspace_id":"ws_test","scope":"llm_gateway:invoke agent:register","exp":{expires_at}}}"#
    ));
    let signing_input = format!("{header}.{payload}");
    let signature = hmac_sha256_base64url(secret, signing_input.as_bytes());
    format!("{signing_input}.{signature}")
}

pub(crate) fn auth_head(method: &str, path: &str, token: &str) -> RequestHead {
    RequestHead {
        method: method.to_string(),
        path: path.to_string(),
        query: HashMap::new(),
        headers: HashMap::from([("authorization".to_string(), format!("Bearer {token}"))]),
    }
}

pub(crate) fn tenant_auth(subject: &str, organization_id: &str, workspace_id: &str) -> AuthContext {
    AuthContext {
        subject: Some(subject.to_string()),
        organization_id: Some(organization_id.to_string()),
        workspace_id: Some(workspace_id.to_string()),
        ..AuthContext::default()
    }
}

#[tokio::test]
pub(crate) async fn wildcard_cors_origin_never_reflects_a_requesting_origin() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    env::set_var("MAESTRO_WEB_ORIGIN", "*");

    let head = parse_request_head(
        b"GET /api/status HTTP/1.1\r\nHost: localhost:8080\r\nOrigin: https://evil.example\r\n\r\n",
    )
    .expect("request should parse");

    assert_eq!(
        requested_cors_origin(&head),
        "*",
        "a wildcard configuration must not reflect the caller's origin"
    );

    let response = with_response_cors_origin(requested_cors_origin(&head), async {
        String::from_utf8(json_response(200, &serde_json::json!({ "ok": true })))
            .expect("response should be utf-8")
    })
    .await;

    assert!(response.contains("Access-Control-Allow-Origin: *\r\n"));
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("access-control-allow-credentials"),
        "credentials must never accompany a wildcard origin: {response}"
    );
    assert!(!response.contains("evil.example"));

    restore_env(snapshot);
}

#[tokio::test]
pub(crate) async fn non_allowlisted_origin_never_receives_allow_credentials() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(CORS_ENV_NAMES);

    // Both the wildcard configuration and a concrete configuration must refuse
    // to hand `Access-Control-Allow-Credentials: true` to an unknown origin.
    for configured in ["*", "chrome-extension://abcdefghijklmnopabcdefghijklmnop"] {
        env::set_var("MAESTRO_WEB_ORIGIN", configured);

        let head = parse_request_head(
            b"POST /api/chat HTTP/1.1\r\nHost: localhost:8080\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .expect("request should parse");

        let reflected = requested_cors_origin(&head);
        assert_ne!(
            reflected, "https://evil.example",
            "configured={configured} must not reflect an unknown origin"
        );

        let response = with_response_cors_origin(reflected, async {
            String::from_utf8(json_response(200, &serde_json::json!({ "ok": true })))
                .expect("response should be utf-8")
        })
        .await;

        let credentialed = response
            .to_ascii_lowercase()
            .contains("access-control-allow-credentials: true");
        let reflects_attacker = response.contains("https://evil.example");
        assert!(
            !(credentialed && reflects_attacker),
            "configured={configured} produced a credentialed reflection: {response}"
        );
        assert!(!reflects_attacker, "configured={configured}: {response}");
    }

    restore_env(snapshot);
}

#[test]
pub(crate) fn configured_web_origins_allow_multiple_first_party_hosts_only() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    clear_env(CORS_ENV_NAMES);
    env::set_var(
        "MAESTRO_WEB_ORIGINS",
        "https://app.deixic.com, https://orb.deixic.com",
    );

    for origin in ["https://app.deixic.com", "https://orb.deixic.com"] {
        let request = format!(
            "GET /api/chat/ws HTTP/1.1\r\nHost: app.deixic.com\r\nOrigin: {origin}\r\n\r\n"
        );
        let head = parse_request_head(request.as_bytes()).expect("request should parse");
        assert!(
            origin_allowed(&head),
            "configured origin should be allowed: {origin}"
        );
        assert_eq!(requested_cors_origin(&head), origin);
    }

    let rejected = parse_request_head(
        b"GET /api/chat/ws HTTP/1.1\r\nHost: app.deixic.com\r\nOrigin: https://evil.example\r\n\r\n",
    )
    .expect("request should parse");
    assert!(!origin_allowed(&rejected));
    assert_eq!(requested_cors_origin(&rejected), "https://app.deixic.com");

    restore_env(snapshot);
}

#[tokio::test]
pub(crate) async fn allowlisted_origin_still_receives_allow_credentials() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    env::set_var(
        "MAESTRO_WEB_ORIGIN",
        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
    );

    for origin in [
        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
        "http://localhost:8080",
    ] {
        let request =
            format!("GET /api/status HTTP/1.1\r\nHost: localhost:8080\r\nOrigin: {origin}\r\n\r\n");
        let head = parse_request_head(request.as_bytes()).expect("request should parse");
        let reflected = requested_cors_origin(&head);
        assert_eq!(reflected, origin);

        let response = with_response_cors_origin(reflected, async {
            String::from_utf8(json_response(200, &serde_json::json!({ "ok": true })))
                .expect("response should be utf-8")
        })
        .await;

        assert!(response.contains(&format!("Access-Control-Allow-Origin: {origin}\r\n")));
        assert!(response.contains("Access-Control-Allow-Credentials: true\r\n"));
    }

    restore_env(snapshot);
}

#[test]
pub(crate) fn loopback_bind_accepts_its_own_host_header_values() {
    let config = Config::test_default();
    assert_eq!(config.listen_host, "127.0.0.1");

    // The web UI is served same-origin, so the browser sends whichever loopback
    // spelling the user typed. The JetBrains plugin uses OkHttp against
    // `http://localhost:8080` (ComposerSettings.apiEndpoint), which emits
    // `Host: localhost:8080`.
    for host in [
        "localhost",
        "localhost:8080",
        "LOCALHOST:8080",
        "127.0.0.1",
        "127.0.0.1:8080",
        "127.0.0.1:3000",
        "[::1]",
        "[::1]:8080",
        "::1",
        "127.0.0.2:8080",
    ] {
        assert!(
            host_header_allowed(&head_with_host(Some(host)), &config),
            "Host: {host} should be accepted on a loopback bind"
        );
    }

    // A client that sends no Host at all is not a browser and cannot be steered
    // by DNS rebinding.
    assert!(host_header_allowed(&head_with_host(None), &config));
}

#[test]
pub(crate) fn loopback_bind_rejects_rebound_host_headers() {
    let config = Config::test_default();

    for host in [
        "evil.example",
        "evil.example:8080",
        "attacker.localhost",
        "localhost.evil.example",
        "127.0.0.1.evil.example",
        "10.0.0.5:8080",
        "maestro.internal.example",
        // Duplicate Host headers are joined with ", " by parse_request_head.
        "localhost, evil.example",
    ] {
        assert!(
            !host_header_allowed(&head_with_host(Some(host)), &config),
            "Host: {host} should be rejected on a loopback bind"
        );
    }
}

#[test]
pub(crate) fn non_loopback_bind_does_not_validate_the_host_header() {
    let mut config = Config::test_default();
    config.listen_host = "0.0.0.0".to_string();

    // A network-facing deployment is reached through names this process cannot
    // enumerate; auth is the control there, not Host.
    for host in ["maestro.example.com", "evil.example", "10.0.0.5"] {
        assert!(host_header_allowed(&head_with_host(Some(host)), &config));
    }
}

#[test]
pub(crate) fn allowed_hosts_extends_the_loopback_host_allowlist() {
    let mut config = Config::test_default();
    config.allowed_hosts = parse_allowed_hosts(Some(
        " maestro.tunnel.example , Dev.Box.Internal ".to_string(),
    ));

    assert_eq!(
        config.allowed_hosts,
        vec!["maestro.tunnel.example", "dev.box.internal"]
    );
    assert!(host_header_allowed(
        &head_with_host(Some("maestro.tunnel.example")),
        &config
    ));
    assert!(host_header_allowed(
        &head_with_host(Some("dev.box.internal:9000")),
        &config
    ));
    assert!(!host_header_allowed(
        &head_with_host(Some("evil.example")),
        &config
    ));
}

#[test]
pub(crate) fn authorizes_identity_jwt_bearer_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous = env::var_os("MAESTRO_JWT_SECRET");
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");

    let token = identity_jwt_bearer_token(b"shared-secret", "user-123");

    assert!(authorize(&bearer_head(&token), &auth_test_config()).is_ok());
    let context = auth_context(&bearer_head(&token), &auth_test_config()).expect("jwt");
    assert_eq!(context.subject.as_deref(), Some("user-123"));
    assert_eq!(context.organization_id.as_deref(), Some("org_test"));
    assert_eq!(context.workspace_id.as_deref(), Some("ws_test"));
    assert_eq!(
        context.scopes,
        vec![
            "llm_gateway:invoke".to_string(),
            "agent:register".to_string()
        ]
    );
    assert_eq!(context.source, crate::auth::AuthSource::IdentityJwt);
    assert!(authorize(&bearer_head("bad-token"), &auth_test_config()).is_err());

    if let Some(previous) = previous {
        env::set_var("MAESTRO_JWT_SECRET", previous);
    } else {
        env::remove_var("MAESTRO_JWT_SECRET");
    }
}

#[test]
pub(crate) fn scoped_auth_is_required_for_mutating_runtime_routes() {
    let _guard = ENV_LOCK.blocking_lock();
    let names: &[&str] = &[
        "MAESTRO_JWT_SECRET",
        "MAESTRO_JWT_ALG",
        "MAESTRO_JWT_AUD",
        "MAESTRO_JWT_ISS",
        "MAESTRO_PROFILE",
        "MAESTRO_WEB_PROFILE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_HOSTED_RUNNER",
        "MAESTRO_RUNNER_KIND",
        "MAESTRO_WEB_API_KEY_SCOPES",
    ];
    let snapshot = snapshot_env(names);
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    env::remove_var("MAESTRO_JWT_ALG");
    env::remove_var("MAESTRO_JWT_AUD");
    env::remove_var("MAESTRO_JWT_ISS");
    env::remove_var("MAESTRO_PROFILE");
    env::remove_var("MAESTRO_WEB_PROFILE");
    env::remove_var("MAESTRO_HOSTED_RUNNER_MODE");
    env::remove_var("MAESTRO_HOSTED_RUNNER");
    env::remove_var("MAESTRO_RUNNER_KIND");

    let limited = hs256_signed_claims(
        b"shared-secret",
        serde_json::json!({
            "sub": "limited-user",
            "scope": "llm_gateway:invoke",
            "exp": now_secs() + 60,
        }),
    );
    let write = hs256_signed_claims(
        b"shared-secret",
        serde_json::json!({
            "sub": "writer-user",
            "scope": "maestro:write",
            "exp": now_secs() + 60,
        }),
    );

    for (method, path) in [
        ("POST", "/api/config"),
        ("POST", "/api/run"),
        ("POST", "/api/approvals"),
        ("POST", "/api/undo"),
        ("POST", "/api/chat"),
        ("GET", "/api/chat/ws"),
        ("POST", "/message:send"),
        ("POST", "/.well-known/evalops/remote-runner/drain"),
    ] {
        assert!(
            authorize(&auth_head(method, path, &limited), &auth_test_config()).is_err(),
            "limited scopes must not mutate {path}"
        );
        assert!(
            authorize(&auth_head(method, path, &write), &auth_test_config()).is_ok(),
            "maestro:write must mutate {path}"
        );
    }
    assert!(
        authorize(
            &auth_head("GET", "/api/status", &limited),
            &auth_test_config()
        )
        .is_ok(),
        "limited scopes retain authenticated reads"
    );

    let mut remote_config = auth_test_config();
    remote_config.listen_host = "0.0.0.0".to_string();
    let static_key_head = RequestHead {
        method: "POST".to_string(),
        path: "/api/config".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([("x-maestro-api-key".to_string(), "api-key".to_string())]),
    };
    env::remove_var("MAESTRO_WEB_API_KEY_SCOPES");
    assert!(
        authorize(&static_key_head, &remote_config).is_err(),
        "remote static keys are read-only until scopes are configured"
    );
    env::set_var("MAESTRO_WEB_API_KEY_SCOPES", "maestro:write");
    assert!(authorize(&static_key_head, &remote_config).is_ok());
    for path in ["/api/sessions", "/tasks"] {
        let tenant_read = RequestHead {
            method: "GET".to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::from([("x-maestro-api-key".to_string(), "api-key".to_string())]),
        };
        assert!(
            authorize(&tenant_read, &remote_config).is_err(),
            "remote static keys must not impersonate a tenant on {path}"
        );
    }
    env::set_var("MAESTRO_JWT_ALG", "HS256");
    env::set_var("MAESTRO_JWT_AUD", "evalops.maestro");
    env::set_var("MAESTRO_JWT_ISS", "evalops.platform");
    let tenant_reader = hs256_signed_claims(
        b"shared-secret",
        serde_json::json!({
            "sub": "tenant-reader",
            "organization_id": "org-a",
            "workspace_id": "workspace-a",
            "aud": "evalops.maestro",
            "iss": "evalops.platform",
            "exp": now_secs() + 60,
        }),
    );
    let remote_sessions = auth_head("GET", "/api/sessions", &tenant_reader);
    assert!(
        authorize(&remote_sessions, &remote_config).is_ok(),
        "tenant-bound identity with read scope should access sessions"
    );

    restore_env(snapshot);
}

#[test]
pub(crate) fn authorizes_hs256_jwt_bearer_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_secret = env::var_os("MAESTRO_JWT_SECRET");
    let previous_alg = env::var_os("MAESTRO_JWT_ALG");
    let previous_aud = env::var_os("MAESTRO_JWT_AUD");
    let previous_iss = env::var_os("MAESTRO_JWT_ISS");
    env::set_var("MAESTRO_JWT_SECRET", "jwt-secret");
    env::remove_var("MAESTRO_JWT_ALG");
    env::remove_var("MAESTRO_JWT_AUD");
    env::remove_var("MAESTRO_JWT_ISS");

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .saturating_add(60);
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"user-123","exp":{expires_at}}}"#));
    let signing_input = format!("{header}.{payload}");
    let signature = hmac_sha256_base64url(b"jwt-secret", signing_input.as_bytes());
    let token = format!("{signing_input}.{signature}");

    assert!(authorize(&bearer_head(&token), &auth_test_config()).is_ok());
    assert_eq!(
        auth_context(&bearer_head(&token), &auth_test_config()).and_then(|auth| auth.subject),
        Some("user-123".to_string())
    );

    if let Some(previous) = previous_secret {
        env::set_var("MAESTRO_JWT_SECRET", previous);
    } else {
        env::remove_var("MAESTRO_JWT_SECRET");
    }
    if let Some(previous) = previous_alg {
        env::set_var("MAESTRO_JWT_ALG", previous);
    }
    if let Some(previous) = previous_aud {
        env::set_var("MAESTRO_JWT_AUD", previous);
    }
    if let Some(previous) = previous_iss {
        env::set_var("MAESTRO_JWT_ISS", previous);
    }
}

#[tokio::test]
pub(crate) async fn jwks_auth_check_does_not_panic_inside_tokio_runtime() {
    let _guard = ENV_LOCK.lock().await;
    let previous_url = env::var_os("MAESTRO_JWT_JWKS_URL");
    let previous_alg = env::var_os("MAESTRO_JWT_ALG");
    let previous_secret = env::var_os("MAESTRO_JWT_SECRET");
    env::set_var("MAESTRO_JWT_JWKS_URL", "http://127.0.0.1:1/jwks.json");
    env::set_var("MAESTRO_JWT_ALG", "RS256");
    env::remove_var("MAESTRO_JWT_SECRET");

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(r#"{"sub":"user-123","exp":4102444800}"#);
    let token = format!("{header}.{payload}.signature");

    let result = std::panic::catch_unwind(|| authorize(&bearer_head(&token), &auth_test_config()));
    assert!(
        result.is_ok(),
        "authorize should not panic inside a Tokio runtime"
    );
    assert!(result.expect("authorize should return a result").is_err());

    if let Some(previous) = previous_url {
        env::set_var("MAESTRO_JWT_JWKS_URL", previous);
    } else {
        env::remove_var("MAESTRO_JWT_JWKS_URL");
    }
    if let Some(previous) = previous_alg {
        env::set_var("MAESTRO_JWT_ALG", previous);
    } else {
        env::remove_var("MAESTRO_JWT_ALG");
    }
    if let Some(previous) = previous_secret {
        env::set_var("MAESTRO_JWT_SECRET", previous);
    } else {
        env::remove_var("MAESTRO_JWT_SECRET");
    }
}

pub(crate) fn set_jwks_test_env() {
    env::set_var("MAESTRO_JWT_JWKS", TEST_JWKS_JSON);
    env::set_var("MAESTRO_JWT_ALG", "RS256");
    env::remove_var("MAESTRO_JWT_JWKS_URL");
    env::remove_var("MAESTRO_JWT_AUD");
    env::remove_var("MAESTRO_JWT_ISS");
    env::remove_var("MAESTRO_JWT_SECRET");
    env::remove_var("MAESTRO_PROFILE");
    env::remove_var("MAESTRO_WEB_PROFILE");
    env::remove_var("MAESTRO_HOSTED_RUNNER_MODE");
    env::remove_var("MAESTRO_HOSTED_RUNNER");
    env::remove_var("MAESTRO_RUNNER_KIND");
}

pub(crate) fn rs256_signed_token(kid: &str, sub: &str, exp: u64) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.to_string());
    let claims = serde_json::json!({"sub": sub, "exp": exp});
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY_PEM.as_bytes())
            .expect("test RSA key should parse"),
    )
    .expect("test token should sign")
}

pub(crate) fn rs256_signed_claims(kid: &str, claims: Value) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY_PEM.as_bytes())
            .expect("test RSA key should parse"),
    )
    .expect("test token should sign")
}

#[test]
pub(crate) fn jwks_rs256_accepts_valid_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(JWKS_TEST_ENV_NAMES);
    set_jwks_test_env();

    let token = rs256_signed_token("test-key-1", "user-123", now_secs() + 3600);

    assert_eq!(
        jwks_jwt_principal(&token, jsonwebtoken::Algorithm::RS256).and_then(|auth| auth.subject),
        Some("user-123".to_string())
    );
    assert_eq!(
        jwt_principal(&token).and_then(|auth| auth.subject),
        Some("user-123".to_string()),
        "jwt_principal should route RS256 tokens through the JWKS path"
    );
    assert!(authorize(&bearer_head(&token), &auth_test_config()).is_ok());

    restore_env(snapshot);
}

#[test]
pub(crate) fn jwks_rs256_rejects_unknown_kid() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(JWKS_TEST_ENV_NAMES);
    set_jwks_test_env();

    // Valid signature, but the kid does not match any key in the JWKS.
    let token = rs256_signed_token("unknown-key", "user-123", now_secs() + 3600);

    assert!(jwks_jwt_principal(&token, jsonwebtoken::Algorithm::RS256).is_none());
    assert!(authorize(&bearer_head(&token), &auth_test_config()).is_err());

    restore_env(snapshot);
}

#[test]
pub(crate) fn jwks_rs256_rejects_expired_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(JWKS_TEST_ENV_NAMES);
    set_jwks_test_env();

    let token = rs256_signed_token("test-key-1", "user-123", now_secs() - 3600);

    assert!(jwks_jwt_principal(&token, jsonwebtoken::Algorithm::RS256).is_none());

    restore_env(snapshot);
}

#[test]
pub(crate) fn hosted_jwks_requires_audience_and_issuer_validation() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(JWKS_TEST_ENV_NAMES);
    set_jwks_test_env();
    env::set_var("MAESTRO_PROFILE", "production");
    env::set_var("MAESTRO_JWT_AUD", "evalops.maestro");
    env::set_var("MAESTRO_JWT_ISS", "evalops.platform");

    let base = serde_json::json!({
        "sub": "hosted-user",
        "exp": now_secs() + 3600,
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
    });
    let valid = rs256_signed_claims("test-key-1", base.clone());
    assert!(jwks_jwt_principal(&valid, jsonwebtoken::Algorithm::RS256).is_some());

    let mut wrong_audience = base.clone();
    wrong_audience["aud"] = serde_json::json!("other-service");
    let mut wrong_issuer = base.clone();
    wrong_issuer["iss"] = serde_json::json!("other-issuer");
    for claims in [wrong_audience, wrong_issuer] {
        assert!(
            jwks_jwt_principal(
                &rs256_signed_claims("test-key-1", claims),
                jsonwebtoken::Algorithm::RS256,
            )
            .is_none(),
            "hosted JWKS must reject incorrect claims"
        );
    }

    let mut missing_audience = base;
    missing_audience
        .as_object_mut()
        .expect("claims object")
        .remove("aud");
    assert!(
        jwks_jwt_principal(
            &rs256_signed_claims("test-key-1", missing_audience),
            jsonwebtoken::Algorithm::RS256,
        )
        .is_none()
    );

    env::remove_var("MAESTRO_JWT_AUD");
    assert!(
        jwks_jwt_principal(&valid, jsonwebtoken::Algorithm::RS256).is_none(),
        "hosted JWKS must not disable audience validation when unset"
    );

    restore_env(snapshot);
}

#[test]
pub(crate) fn jwks_rs256_rejects_hs256_alg_confusion_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(JWKS_TEST_ENV_NAMES);
    set_jwks_test_env();

    // Classic algorithm-confusion attack: HS256 token signed with the public
    // JWKS material as the HMAC secret. Must be rejected on the RS256 path.
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(format!(
        r#"{{"sub":"user-123","exp":{}}}"#,
        now_secs() + 3600
    ));
    let signing_input = format!("{header}.{payload}");
    let signature = hmac_sha256_base64url(TEST_JWKS_JSON.as_bytes(), signing_input.as_bytes());
    let token = format!("{signing_input}.{signature}");

    assert!(jwks_jwt_principal(&token, jsonwebtoken::Algorithm::RS256).is_none());
    assert!(jwt_principal(&token).is_none());
    assert!(authorize(&bearer_head(&token), &auth_test_config()).is_err());

    restore_env(snapshot);
}

#[test]
pub(crate) fn subject_auth_only_sees_owned_sessions() {
    let mut session = test_session_record("session-1");
    session.owner = Some("user-123".to_string());
    let owner = AuthContext {
        subject: Some("user-123".to_string()),
        unrestricted: false,
        ..AuthContext::default()
    };
    let other_user = AuthContext {
        subject: Some("user-456".to_string()),
        unrestricted: false,
        ..AuthContext::default()
    };
    let admin = AuthContext {
        subject: None,
        unrestricted: true,
        ..AuthContext::default()
    };

    assert!(session_visible_to_auth(&session, &owner));
    assert!(!session_visible_to_auth(&session, &other_user));
    assert!(session_visible_to_auth(&session, &admin));
}

#[tokio::test]
pub(crate) async fn shared_session_get_uses_share_token_without_api_auth() {
    let mut session = test_session_record("session-1");
    session.messages.push(serde_json::json!({
        "role": "user",
        "content": "shared hello"
    }));
    let state =
        test_app_state_with_sessions(HashMap::from([(session.id.clone(), session.clone())]));
    state.shared_sessions.lock().await.insert(
        "share-token".to_string(),
        SharedSessionGrant {
            session_id: session.id.clone(),
            expires_at: now_millis().saturating_add(60_000),
            max_accesses: Some(2),
            access_count: 0,
        },
    );
    let mut server = tcp_stream_pair().await.0;
    let head = parse_request_head(
        b"GET /api/sessions/shared/share-token HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .expect("request should parse");

    let response = handle_session_endpoint(&mut server, &mut Vec::new(), &head, &state).await;
    let text = String::from_utf8(response).expect("response should be utf-8");

    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.contains("shared hello"));
}

#[test]
pub(crate) fn generated_share_tokens_are_opaque() {
    let session_id = "session-123";
    let token = generate_share_token().expect("share token should be generated");

    assert_ne!(token, session_id);
    assert!(!token.contains(session_id));
    assert!(token.len() >= 32);
}

#[test]
pub(crate) fn websocket_origin_check_allows_local_and_rejects_cross_site() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    clear_env(CORS_ENV_NAMES);

    let allowed = parse_request_head(
        b"GET /api/chat/ws HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost:4173\r\n\r\n",
    )
    .expect("request should parse");
    assert!(origin_allowed(&allowed));

    let packaged_origin = parse_request_head(
            b"GET /api/chat/ws HTTP/1.1\r\nHost: localhost:8080\r\nOrigin: http://localhost:8080\r\n\r\n",
        )
        .expect("request should parse");
    assert!(origin_allowed(&packaged_origin));

    let rejected = parse_request_head(
        b"GET /api/chat/ws HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\n\r\n",
    )
    .expect("request should parse");
    assert!(!origin_allowed(&rejected));

    restore_env(snapshot);
}

#[tokio::test]
pub(crate) async fn allowed_request_origin_is_echoed_in_cors_response_headers() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    clear_env(CORS_ENV_NAMES);

    let head = parse_request_head(
        b"GET /api/status HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost:3000\r\n\r\n",
    )
    .expect("request should parse");

    let response = with_response_cors_origin(requested_cors_origin(&head), async {
        response(200, "application/json", b"{}")
    })
    .await;
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.contains("Access-Control-Allow-Origin: http://localhost:3000\r\n"));
    assert!(!response.contains("Access-Control-Allow-Origin: http://localhost:4173\r\n"));

    restore_env(snapshot);
}

#[tokio::test]
pub(crate) async fn response_cors_origin_matches_allowed_request_origin() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    clear_env(CORS_ENV_NAMES);

    let head = parse_request_head(
        b"GET /api/status HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\n\r\n",
    )
    .expect("request should parse");
    let response = with_response_cors_origin(requested_cors_origin(&head), async {
        json_response(200, &serde_json::json!({ "ok": true }))
    })
    .await;
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.contains("Access-Control-Allow-Origin: http://127.0.0.1:5173\r\n"));
    assert!(response.contains("Access-Control-Allow-Credentials: true\r\n"));
    assert!(response.contains("Vary: Origin\r\n"));

    restore_env(snapshot);
}

#[tokio::test]
pub(crate) async fn sse_headers_vary_by_request_origin() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    clear_env(CORS_ENV_NAMES);

    let head = parse_request_head(
        b"GET /api/chat HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\n\r\n",
    )
    .expect("request should parse");
    let response =
        with_response_cors_origin(requested_cors_origin(&head), async { sse_headers() }).await;

    assert!(response.contains("Access-Control-Allow-Origin: http://127.0.0.1:5173\r\n"));
    assert!(response.contains("Vary: Origin\r\n"));

    restore_env(snapshot);
}

#[tokio::test]
pub(crate) async fn wildcard_cors_origin_omits_credentials_header() {
    let response = with_response_cors_origin("*".to_string(), async {
        json_response(200, &serde_json::json!({ "ok": true }))
    })
    .await;
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.contains("Access-Control-Allow-Origin: *\r\n"));
    assert!(!response.contains("Access-Control-Allow-Credentials: true\r\n"));
}

#[test]
pub(crate) fn wildcard_web_origin_allows_websocket_origins() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(CORS_ENV_NAMES);
    clear_env(CORS_ENV_NAMES);
    env::set_var("MAESTRO_WEB_ORIGIN", "*");
    let head = parse_request_head(
        b"GET /api/chat/ws HTTP/1.1\r\nHost: localhost\r\nOrigin: https://app.example.com\r\n\r\n",
    )
    .expect("request should parse");

    assert!(origin_allowed(&head));

    restore_env(snapshot);
}

#[test]
pub(crate) fn loopback_static_key_remains_unrestricted_for_mutations() {
    let _guard = ENV_LOCK.blocking_lock();
    let config = auth_test_config();
    let head = RequestHead {
        method: "POST".to_string(),
        path: "/api/run".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([("x-maestro-api-key".to_string(), "api-key".to_string())]),
    };

    assert!(authorize(&head, &config).is_ok());
}

#[test]
pub(crate) fn hosted_loopback_static_key_requires_write_scope_for_mutations() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(&[
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_PROFILE",
        "MAESTRO_WEB_PROFILE",
        "MAESTRO_WEB_API_KEY_SCOPES",
    ]);
    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    env::remove_var("MAESTRO_PROFILE");
    env::remove_var("MAESTRO_WEB_PROFILE");
    env::remove_var("MAESTRO_WEB_API_KEY_SCOPES");

    let config = auth_test_config();
    let head = RequestHead {
        method: "POST".to_string(),
        path: "/api/run".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([("x-maestro-api-key".to_string(), "api-key".to_string())]),
    };
    assert!(authorize(&head, &config).is_err());

    env::set_var("MAESTRO_WEB_API_KEY_SCOPES", "maestro:write");
    assert!(authorize(&head, &config).is_ok());

    restore_env(snapshot);
}

#[test]
pub(crate) fn trusted_proxy_auth_requires_shared_proxy_token() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous = env::var_os("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN");
    env::set_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", "proxy-secret");
    let config = auth_test_config();
    let spoofed = RequestHead {
        method: "GET".to_string(),
        path: "/api/status".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([(
            "x-auth-request-email".to_string(),
            "jonathan@evalops.dev".to_string(),
        )]),
    };
    let trusted = RequestHead {
        method: "GET".to_string(),
        path: "/api/status".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([
            (
                "x-auth-request-email".to_string(),
                "jonathan@evalops.dev".to_string(),
            ),
            (
                "x-maestro-proxy-auth".to_string(),
                "proxy-secret".to_string(),
            ),
            (
                "x-auth-request-scope".to_string(),
                "maestro:write".to_string(),
            ),
            ("x-organization-id".to_string(), "org-proxy".to_string()),
            (
                "x-evalops-workspace-id".to_string(),
                "workspace-proxy".to_string(),
            ),
        ]),
    };

    assert!(auth_context(&spoofed, &config).is_none());
    let context = auth_context(&trusted, &config).expect("proxy token should authorize");
    assert_eq!(context.subject.as_deref(), Some("jonathan@evalops.dev"));
    assert_eq!(context.organization_id.as_deref(), Some("org-proxy"));
    assert_eq!(context.workspace_id.as_deref(), Some("workspace-proxy"));
    assert_eq!(context.scopes, vec!["maestro:write"]);
    assert!(!context.unrestricted);
    let mut mutation = trusted;
    mutation.method = "POST".to_string();
    mutation.path = "/api/config".to_string();
    assert!(authorize(&mutation, &config).is_ok());

    if let Some(previous) = previous {
        env::set_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", previous);
    } else {
        env::remove_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN");
    }
}

#[test]
pub(crate) fn trusted_proxy_token_counts_as_configured_auth() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous = env::var_os("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN");
    env::set_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", "proxy-secret");
    let mut config = auth_test_config();
    config.api_key = None;
    config.require_key = false;
    let head = RequestHead {
        method: "GET".to_string(),
        path: "/api/status".to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
    };

    assert!(auth_context(&head, &config).is_none());

    if let Some(previous) = previous {
        env::set_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", previous);
    } else {
        env::remove_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN");
    }
}

#[test]
pub(crate) fn auth_mode_matrix_pins_control_plane_access() {
    let _guard = ENV_LOCK.blocking_lock();
    let preserved_env: Vec<_> = [
        "MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN",
        "MAESTRO_JWT_SECRET",
        "MAESTRO_JWT_SECRET",
        "MAESTRO_JWT_JWKS_URL",
    ]
    .iter()
    .map(|name| (*name, env::var_os(name)))
    .collect();
    for (name, _) in &preserved_env {
        env::remove_var(name);
    }

    let api_key_config = auth_test_config();
    let mut open_dev_config = auth_test_config();
    open_dev_config.api_key = None;
    open_dev_config.require_key = false;
    struct AuthMatrixCase {
        name: &'static str,
        config: Config,
        headers: HashMap<String, String>,
        expected_subject: Option<&'static str>,
        expected_unrestricted: Option<bool>,
    }

    let cases = vec![
        AuthMatrixCase {
            name: "open local dev when no auth is configured",
            config: open_dev_config.clone(),
            headers: HashMap::new(),
            expected_subject: None,
            expected_unrestricted: Some(true),
        },
        AuthMatrixCase {
            name: "api key configured rejects anonymous requests",
            config: api_key_config.clone(),
            headers: HashMap::new(),
            expected_subject: None,
            expected_unrestricted: None,
        },
        AuthMatrixCase {
            name: "api key header grants unrestricted loopback access",
            config: api_key_config.clone(),
            headers: HashMap::from([("x-maestro-api-key".to_string(), "api-key".to_string())]),
            expected_subject: None,
            expected_unrestricted: Some(true),
        },
        AuthMatrixCase {
            name: "bearer api key grants unrestricted loopback access",
            config: api_key_config.clone(),
            headers: HashMap::from([("authorization".to_string(), "Bearer api-key".to_string())]),
            expected_subject: None,
            expected_unrestricted: Some(true),
        },
    ];

    for case in cases {
        let head = RequestHead {
            method: "GET".to_string(),
            path: "/api/status".to_string(),
            query: HashMap::new(),
            headers: case.headers,
        };
        let context = auth_context(&head, &case.config);
        match case.expected_unrestricted {
            Some(unrestricted) => {
                let context = context.unwrap_or_else(|| {
                    panic!("{} should authorize", case.name);
                });
                assert_eq!(
                    context.subject.as_deref(),
                    case.expected_subject,
                    "{} subject",
                    case.name
                );
                assert_eq!(
                    context.unrestricted, unrestricted,
                    "{} unrestricted",
                    case.name
                );
            }
            None => assert!(context.is_none(), "{} should reject", case.name),
        }
    }

    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    let bearer_user = "bearer-user";
    let bearer = identity_jwt_bearer_token(b"shared-secret", bearer_user);
    let bearer_head = RequestHead {
        method: "GET".to_string(),
        path: "/api/status".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([("authorization".to_string(), format!("Bearer {bearer}"))]),
    };
    let bearer_context =
        auth_context(&bearer_head, &open_dev_config).expect("bearer should authorize");
    assert_eq!(bearer_context.subject.as_deref(), Some(bearer_user));
    assert!(!bearer_context.unrestricted);

    env::remove_var("MAESTRO_JWT_SECRET");
    env::set_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", "proxy-secret");
    let proxy_head = RequestHead {
        method: "GET".to_string(),
        path: "/api/status".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([
            (
                "x-auth-request-email".to_string(),
                "proxy-user@evalops.dev".to_string(),
            ),
            (
                "x-maestro-proxy-auth".to_string(),
                "proxy-secret".to_string(),
            ),
        ]),
    };
    let proxy_context =
        auth_context(&proxy_head, &open_dev_config).expect("proxy should authorize");
    assert_eq!(
        proxy_context.subject.as_deref(),
        Some("proxy-user@evalops.dev")
    );
    assert!(!proxy_context.unrestricted);

    for (name, value) in preserved_env {
        if let Some(value) = value {
            env::set_var(name, value);
        } else {
            env::remove_var(name);
        }
    }
}

pub(crate) fn messaging_auth(
    subject: &str,
    organization_id: Option<&str>,
    workspace_id: Option<&str>,
) -> AuthContext {
    AuthContext {
        subject: Some(subject.to_string()),
        organization_id: organization_id.map(str::to_string),
        workspace_id: workspace_id.map(str::to_string),
        unrestricted: false,
        ..AuthContext::default()
    }
}

pub(crate) fn security_fix_token(claims: Value) -> String {
    hs256_signed_claims(b"shared-secret", claims)
}
