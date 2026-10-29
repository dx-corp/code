use super::*;

#[test]
fn merge_login_scopes_appends_without_duplicates() {
    assert_eq!(
        merge_login_scopes("a:read b:write", "b:write c:read  c:read"),
        "a:read b:write c:read"
    );
    assert_eq!(
        merge_login_scopes(REQUIRED_LOGIN_SCOPES, ""),
        REQUIRED_LOGIN_SCOPES
    );
}

#[test]
fn production_identity_admission_rejects_caller_selected_authorities() {
    for authority in [
        "http://127.0.0.1:8080",
        "https://identity.attacker.example",
        "https://identity.evalops.dev.attacker.example",
        "https://identity.evalops.dev:8443",
        "https://identity.evalops.dev/tenant-controlled",
        "https://identity.deixic.com:8443",
        "https://identity.deixic.com/tenant-controlled",
        "https://app.evalops.dev",
        "https://identity.deixic.com.attacker.example",
    ] {
        let error = validate_identity_authority(authority, false)
            .expect_err("caller-selected authority must fail closed");
        assert!(
            error.to_string().contains("Identity authority"),
            "{authority}: {error:#}"
        );
    }
}

#[test]
fn production_identity_admission_accepts_only_known_first_party_origins() {
    assert_eq!(
        validate_identity_authority("https://identity.deixic.com/", false)
            .expect("production Identity"),
        "https://identity.deixic.com"
    );
    assert_eq!(
        validate_identity_authority("https://identity.evalops.dev/", false)
            .expect("production Identity under its retiring name"),
        "https://identity.evalops.dev"
    );
    assert_eq!(
        validate_identity_authority("https://api.staging.evalops.dev", false)
            .expect("staging Identity"),
        "https://api.staging.evalops.dev"
    );
}

#[test]
fn identity_authority_ignores_platform_base_urls() {
    let env = std::collections::HashMap::from([
        (
            "EVALOPS_BASE_URL".to_owned(),
            "https://app.evalops.dev".to_owned(),
        ),
        (
            "MAESTRO_PLATFORM_BASE_URL".to_owned(),
            "https://app.evalops.dev".to_owned(),
        ),
        (
            "MAESTRO_EVALOPS_BASE_URL".to_owned(),
            "https://identity.attacker.example".to_owned(),
        ),
    ]);
    assert_eq!(
        evalops_identity_base_url(None, &env).expect("default Identity"),
        "https://identity.deixic.com"
    );
}

#[test]
fn explicit_untrusted_identity_url_fails_closed() {
    let env = std::collections::HashMap::from([(
        "MAESTRO_IDENTITY_URL".to_owned(),
        "https://app.evalops.dev".to_owned(),
    )]);
    let error = evalops_identity_base_url(None, &env)
        .expect_err("platform URL is not an Identity authority");
    assert!(
        error
            .to_string()
            .contains("untrusted EvalOps Identity authority")
    );
}

#[test]
fn stored_untrusted_identity_snapshot_fails_closed() {
    let snapshot = EvalOpsCredentialSnapshot {
        access: "tok".to_owned(),
        refresh: "refresh".to_owned(),
        expires: 1,
        email: None,
        organization_id: Some("org".to_owned()),
        workspace_id: None,
        user_id: None,
        identity_base_url: Some("https://identity.attacker.example".to_owned()),
        provider_ref: None,
        agent_mcp: None,
    };
    let error = evalops_identity_base_url(Some(&snapshot), &std::collections::HashMap::new())
        .expect_err("stored attacker Identity URL must fail closed");
    assert!(
        error
            .to_string()
            .contains("untrusted EvalOps Identity authority")
    );
}

#[test]
fn login_requests_session_history_authority() {
    let scopes = REQUIRED_LOGIN_SCOPES.split_whitespace().collect::<Vec<_>>();
    assert!(scopes.contains(&"llm_gateway:invoke"));
    assert!(scopes.contains(&"product_issues:write"));
    assert!(scopes.contains(&"sessions:read"));
    assert!(scopes.contains(&"sessions:write"));
}

#[test]
fn stored_login_hint_preserves_login_only_scope_and_registration_precedence() {
    let mut metadata = json!({
        "organizationId": "org-1",
        "workspaceId": "login-workspace"
    })
    .as_object()
    .unwrap()
    .clone();
    assert_eq!(
        stored_login_tenant_hint(&metadata),
        Some(("org-1".to_owned(), "login-workspace".to_owned()))
    );
    metadata.insert(
        "agentMcp".to_owned(),
        json!({"workspaceId": "registered-workspace"}),
    );
    assert_eq!(
        stored_login_tenant_hint(&metadata),
        Some(("org-1".to_owned(), "registered-workspace".to_owned()))
    );
    metadata.insert("agentMcp".to_owned(), json!({"workspaceId": " "}));
    assert_eq!(
        stored_login_tenant_hint(&metadata),
        Some(("org-1".to_owned(), "login-workspace".to_owned()))
    );
    metadata.remove("organizationId");
    assert_eq!(stored_login_tenant_hint(&metadata), None);
    metadata.insert("organizationId".to_owned(), json!("org-1"));
    metadata.remove("workspaceId");
    assert_eq!(stored_login_tenant_hint(&metadata), None);
}

#[test]
fn login_tenant_hint_requires_complete_identity_scope() {
    assert_eq!(complete_login_tenant_hint(None, None), None);
    assert_eq!(
        complete_login_tenant_hint(Some("org-1".to_owned()), None),
        None
    );
    assert_eq!(
        complete_login_tenant_hint(None, Some("workspace-1".to_owned())),
        None
    );
    assert_eq!(
        complete_login_tenant_hint(Some(" org-1 ".to_owned()), Some(" workspace-1 ".to_owned())),
        Some(("org-1".to_owned(), "workspace-1".to_owned()))
    );
}

#[test]
fn provider_ref_defaults_to_canonical_gateway_tuple() {
    let value = provider_ref_from(None, None, None, None);
    assert_eq!(value["provider"], "openai");
    assert_eq!(value["environment"], "production");
    assert_eq!(value["credential_name"], "default");
    assert!(value.get("team_id").is_none());
}

#[test]
fn provider_ref_normalizes_legacy_prod_and_keeps_explicit_values() {
    let value = provider_ref_from(
        Some("anthropic".to_owned()),
        Some("prod".to_owned()),
        Some("scoped-key".to_owned()),
        Some("team_9".to_owned()),
    );
    assert_eq!(value["provider"], "anthropic");
    assert_eq!(value["environment"], "production");
    assert_eq!(value["credential_name"], "scoped-key");
    assert_eq!(value["team_id"], "team_9");

    let staging = provider_ref_from(None, Some("staging".to_owned()), None, None);
    assert_eq!(staging["environment"], "staging");
}

#[test]
fn parses_aliases_repeatable_lists_and_positive_numbers() {
    let args = [
        "--mcp-url",
        "https://app.example.test",
        "--capability",
        "mcp,shell",
        "--capabilities",
        "git",
        "--key-scope",
        "agent:register,llm_gateway:invoke",
        "--ttl-seconds",
        "60",
        "--json",
    ]
    .map(str::to_owned);
    let options = parse_args(&args).unwrap();
    assert_eq!(options.mcp_url.as_deref(), Some("https://app.example.test"));
    assert_eq!(options.capabilities, ["mcp", "shell", "git"]);
    assert_eq!(options.api_key_scopes.len(), 2);
    assert_eq!(options.ttl_seconds, Some(60));
    assert!(options.json);
}

#[test]
fn rejects_unknown_options_and_non_positive_numbers() {
    assert!(parse_args(&["--wat".to_owned()]).is_err());
    assert!(parse_args(&["--ttl-seconds".to_owned(), "0".to_owned()]).is_err());
}

#[test]
fn applies_legacy_registration_defaults() {
    let options = InitOptions::default();
    assert_eq!(integration_profile(&options), "managed_runtime");
    assert_eq!(shim_type(&options), "sdk");
    assert_eq!(trace_mode(&options), "otlp");
    assert_eq!(memory_mode(&options), "durable");
    assert_eq!(runtime_owner(&options), "evalops");
    assert!(capabilities(&options).contains(&"maestro:init".to_owned()));
}

#[test]
fn stored_registration_workspace_requires_an_explicit_workspace_option() {
    assert_eq!(stored_registration_workspace(&InitOptions::default()), None);
    let options = InitOptions {
        workspace_id: Some("workspace-1".to_owned()),
        ..InitOptions::default()
    };
    assert_eq!(
        stored_registration_workspace(&options).as_deref(),
        Some("workspace-1")
    );
}

#[test]
fn public_agent_mcp_default_uses_deixic_origin() {
    assert_eq!(DEFAULT_AGENT_MCP_BASE_URL, "https://app.deixic.com");
}

#[test]
fn deixic_agent_mcp_origin_uses_the_canonical_identity_authority() {
    assert_eq!(
        identity_from_mcp("https://app.deixic.com/mcp")
            .unwrap()
            .as_deref(),
        Some("https://identity.deixic.com")
    );
    assert_eq!(
        identity_from_mcp("https://app.evalops.dev/mcp")
            .unwrap()
            .as_deref(),
        Some("https://identity.deixic.com")
    );
}

#[test]
fn stored_deixic_agent_mcp_keeps_the_canonical_identity_authority() {
    let _guard = crate::config::test_process_env_lock();
    let env_names = [
        "MAESTRO_IDENTITY_URL",
        "EVALOPS_IDENTITY_URL",
        "MAESTRO_PLATFORM_BASE_URL",
        "MAESTRO_EVALOPS_BASE_URL",
        "EVALOPS_BASE_URL",
    ];
    let previous_env = env_names.map(|name| (name, std::env::var(name).ok()));
    for name in env_names {
        std::env::remove_var(name);
    }
    let endpoint = Endpoint {
        endpoint: "https://app.deixic.com/mcp".to_owned(),
        identity_base_url: None,
        manifest_url: None,
        prefer_derived_identity: false,
    };
    let mut credentials = OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: String::new(),
        access: "access".to_owned(),
        expires: 1,
        metadata: Map::new(),
    };
    credentials.metadata.insert(
        "identityBaseUrl".to_owned(),
        json!("https://identity.attacker.example"),
    );
    let resolved = resolve_identity_base_url(&endpoint, &credentials).unwrap();
    for (name, previous) in previous_env {
        restore_env(name, previous);
    }
    assert_eq!(resolved, "https://identity.deixic.com");
}

#[test]
fn normalizes_manifest_and_mcp_urls() {
    assert_eq!(
        normalize_mcp_endpoint("https://app.evalops.dev").unwrap(),
        "https://app.evalops.dev/mcp"
    );
    assert_eq!(
        normalize_manifest_url("https://app.evalops.dev").unwrap(),
        "https://app.evalops.dev/.well-known/evalops/agent-mcp.json"
    );
    assert_eq!(
        normalize_mcp_endpoint("https://app.deixic.com").unwrap(),
        "https://app.deixic.com/mcp"
    );
    assert_eq!(
        normalize_manifest_url("https://app.deixic.com").unwrap(),
        "https://app.deixic.com/.well-known/evalops/agent-mcp.json"
    );
}

#[test]
fn deixic_console_url_uses_the_production_environment() {
    assert_eq!(
        console_url("https://app.deixic.com/mcp").unwrap(),
        "https://app.deixic.com/overview?env=production"
    );
}

#[test]
fn parses_json_and_sse_mcp_payloads() {
    let json_body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
    let sse_body = format!("event: message\ndata: {json_body}\n\n");
    let json_value: Value = serde_json::from_str(json_body).unwrap();
    let sse_value: Value = sse_body
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .map(serde_json::from_str)
        .unwrap()
        .unwrap();
    assert_eq!(json_value, sse_value);
}

#[test]
fn stored_agent_metadata_requires_runtime_secret_fields() {
    let mut credentials = OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: String::new(),
        access: "access".to_owned(),
        expires: 1,
        metadata: Map::new(),
    };
    credentials.metadata.insert(
        "agentMcp".to_owned(),
        json!({
            "type": "agent-mcp",
            "apiKey": "key",
            "createdAt": "2026-01-01T00:00:00Z",
            "endpoint": "https://app.evalops.dev/mcp",
            "registeredAt": "2026-01-01T00:00:00Z",
            "surface": "cli"
        }),
    );
    assert!(stored_agent_mcp(&credentials).is_some());
    credentials.metadata["agentMcp"]["apiKey"] = Value::String(String::new());
    assert!(stored_agent_mcp(&credentials).is_none());
}

#[test]
fn stored_agent_credentials_skip_expired_oauth_unless_rotation_is_requested() {
    let mut credentials = OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: "refresh".to_owned(),
        access: "expired".to_owned(),
        expires: 1,
        metadata: Map::new(),
    };
    credentials.metadata.insert(
        "agentMcp".to_owned(),
        json!({
            "type": "agent-mcp",
            "apiKey": "agent-key",
            "createdAt": "2026-01-01T00:00:00Z",
            "endpoint": "https://app.evalops.dev/mcp",
            "registeredAt": "2026-01-01T00:00:00Z",
            "surface": "cli"
        }),
    );

    let options = InitOptions::default();
    assert!(can_reuse_stored_agent(&options, &credentials));
    assert!(!can_reuse_stored_agent(
        &InitOptions {
            rotate_key: true,
            ..InitOptions::default()
        },
        &credentials
    ));
    assert!(!can_reuse_stored_agent(
        &InitOptions {
            force_login: true,
            ..InitOptions::default()
        },
        &credentials
    ));
}

#[test]
fn oauth_callback_requires_matching_state_and_authorization_code() {
    let valid = BTreeMap::from([
        ("state".to_owned(), "expected".to_owned()),
        ("code".to_owned(), "authorization-code".to_owned()),
    ]);
    assert_eq!(
        validated_callback_code(&valid, "expected").unwrap(),
        "authorization-code"
    );
    assert!(validated_callback_code(&valid, "different").is_err());
    assert!(validated_callback_code(&BTreeMap::new(), "expected").is_err());
}

#[test]
fn a_pasted_callback_address_is_held_to_the_listener_checks() {
    let _guard = crate::config::test_process_env_lock();
    let port = callback_port();
    let pasted = |address: String| pasted_callback(&address, "expected");
    let callback = pasted(format!(
        "http://127.0.0.1:{port}/auth/callback/evalops?code=abc&state=expected"
    ))
    .unwrap()
    .expect("the callback address is accepted");
    assert_eq!(callback.code, "abc");
    assert!(
        pasted(format!(
            "  http://localhost:{port}/auth/callback/evalops?code=abc&state=expected\n"
        ))
        .unwrap()
        .is_some(),
        "surrounding whitespace and localhost are fine"
    );
    // Another login's state is refused, as the listener refuses it.
    assert!(
        pasted(format!(
            "http://127.0.0.1:{port}/auth/callback/evalops?code=abc&state=other"
        ))
        .is_err()
    );
    // An identity error ends the login.
    assert!(
        pasted(format!(
            "http://127.0.0.1:{port}/auth/callback/evalops?error=access_denied&state=expected"
        ))
        .is_err()
    );
    // Anything that is not this callback is not a callback.
    for other in [
        "not a url".to_owned(),
        format!("https://evil.example:{port}/auth/callback/evalops?code=abc&state=expected"),
        format!(
            "http://127.0.0.1:{}/auth/callback/evalops?code=abc&state=expected",
            port + 1
        ),
        format!("http://127.0.0.1:{port}/elsewhere?code=abc&state=expected"),
    ] {
        assert!(pasted(other).unwrap().is_none());
    }
}

#[test]
fn oauth_error_detail_prefers_structured_description() {
    assert_eq!(
        response_detail(r#"{"error":"invalid_request","error_description":"redirect rejected"}"#),
        "redirect rejected"
    );
    assert_eq!(response_detail(""), "no response body");
}

#[test]
fn evalops_credential_load_is_cached_until_invalidated() {
    let home = tempfile::tempdir().expect("maestro home");
    let context = CredentialStorageContext {
        home: home.path().to_path_buf(),
        force_file: true,
        force_keychain: false,
    };
    let mut cache = CredentialCache::default();

    let first = OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: "refresh-one".to_owned(),
        access: "access-one".to_owned(),
        expires: 1,
        metadata: Map::new(),
    };
    let second = OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: "refresh-two".to_owned(),
        access: "access-two".to_owned(),
        expires: 2,
        metadata: Map::new(),
    };
    let path = home.path().join("oauth.json");
    atomic_private_write(
        &path,
        &serde_json::to_vec_pretty(&json!({"evalops": &first})).unwrap(),
    )
    .unwrap();

    let loaded = load_credentials_with_cache(&mut cache, context.clone())
        .unwrap()
        .expect("first credentials should load");
    assert_eq!(loaded.access, "access-one");

    atomic_private_write(
        &path,
        &serde_json::to_vec_pretty(&json!({"evalops": &second})).unwrap(),
    )
    .unwrap();
    let cached = load_credentials_with_cache(&mut cache, context.clone())
        .unwrap()
        .expect("cached credentials should remain available");
    assert_eq!(cached.access, "access-one");

    invalidate_credentials_cache(&mut cache);
    let refreshed = load_credentials_with_cache(&mut cache, context)
        .unwrap()
        .expect("invalidated credentials should reload");
    assert_eq!(refreshed.access, "access-two");
}

async fn revoke_against_identity_stub(status: u16) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Identity's JSON revoke contract uses `token`; refresh uses `refresh_token`.
    #[derive(Deserialize)]
    struct RevokeRequest {
        token: String,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let identity = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "POST /v1/tokens/revoke HTTP/1.1\r\n");
        let mut length = None;
        let mut content_type = None;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_owned());
            }
            assert!(!name.eq_ignore_ascii_case("authorization"));
        }
        assert_eq!(content_type.as_deref(), Some("application/json"));
        let length = length.expect("JSON request length");
        assert!(length < 4096);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
        let request = serde_json::from_slice::<RevokeRequest>(&body);
        let (response_status, response_body) = match request {
            Ok(request) => {
                assert_eq!(request.token, "refresh-fixture");
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"token": "refresh-fixture"})
                );
                (
                    status,
                    if status == 200 {
                        r#"{"revoked":true}"#
                    } else {
                        r#"{"error":"revocation_unavailable"}"#
                    },
                )
            }
            Err(_) => (422, "missing field token"),
        };
        let response = format!(
            "HTTP/1.1 {response_status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let credentials = OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: "refresh-fixture".to_owned(),
        access: "access-must-not-be-sent".to_owned(),
        expires: 0,
        metadata: Map::from_iter([("identityBaseUrl".to_owned(), json!(identity))]),
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let result = revoke_refresh_token(&credentials, &client).await;
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("issuer request completes")
        .expect("issuer contract assertions pass");
    result
}

#[tokio::test]
async fn revoke_refresh_token_matches_identity_json_contract() {
    revoke_against_identity_stub(200).await.unwrap();
}

#[tokio::test]
async fn revoke_refresh_token_preserves_issuer_errors() {
    for status in [400, 503] {
        let error = revoke_against_identity_stub(status).await.unwrap_err();
        assert!(error.to_string().contains("revocation_unavailable"));
    }
}

struct IdentityStub {
    challenge: Option<String>,
    requested_scopes: String,
    mismatch_state: bool,
}

async fn spawn_identity_stub(mismatch_state: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("identity bind");
    let addr = listener.local_addr().expect("identity addr");
    let stub = std::sync::Arc::new(std::sync::Mutex::new(IdentityStub {
        challenge: None,
        requested_scopes: String::new(),
        mismatch_state,
    }));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buffer = vec![0_u8; 16 * 1024];
            let Ok(size) = stream.read(&mut buffer).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&buffer[..size]);
            let first = request.lines().next().unwrap_or_default();
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or_default();
            let target = parts.next().unwrap_or("/");
            let (path, _query) = target.split_once('?').unwrap_or((target, ""));
            let (status, headers, body) = match (method, path) {
                ("POST", "/register") => (
                    201,
                    String::new(),
                    r#"{"client_id":"maestro-test-client"}"#.to_owned(),
                ),
                ("GET", "/authorize") => {
                    let parsed = Url::parse(&format!("http://identity.example{target}"))
                        .expect("authorize url");
                    let query = parsed
                        .query_pairs()
                        .into_owned()
                        .collect::<BTreeMap<_, _>>();
                    let challenge = query.get("code_challenge").cloned();
                    let state = query.get("state").cloned().unwrap_or_default();
                    let redirect = query.get("redirect_uri").cloned().unwrap_or_default();
                    if let Ok(mut stub) = stub.lock() {
                        stub.challenge = challenge;
                        stub.requested_scopes = query.get("scope").cloned().unwrap_or_default();
                    }
                    let callback_state = if stub
                        .lock()
                        .map(|value| value.mismatch_state)
                        .unwrap_or(false)
                    {
                        "mismatched-state".to_owned()
                    } else {
                        state
                    };
                    let location =
                        format!("{redirect}?code=auth-code-1&state={callback_state}");
                    (302, format!("Location: {location}\r\n"), String::new())
                }
                ("POST", "/token") => {
                    let body = request
                        .split("\r\n\r\n")
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    let form = url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect::<BTreeMap<_, _>>();
                    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                    let expected = stub
                        .lock()
                        .ok()
                        .and_then(|value| value.challenge.clone())
                        .unwrap_or_default();
                    let provided = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                    if expected.is_empty() || provided != expected {
                        (
                            400,
                            String::new(),
                            r#"{"error":"invalid_grant","error_description":"pkce mismatch"}"#
                                .to_owned(),
                        )
                    } else {
                        (
                            200,
                            String::new(),
                            json!({
                                "access_token": "access-from-stub",
                                "expires_in": 3600,
                                "refresh_token": "refresh-from-stub",
                                "scope": stub.lock().expect("identity stub").requested_scopes,
                                "organization_id": "org_from_stub",
                                "workspace_id": "workspace_from_stub",
                            }).to_string(),
                        )
                    }
                }
                ("POST", "/v1/tokens/refresh") => (
                    200,
                    String::new(),
                    r#"{"access_token":"access-refreshed","expires_in":3600,"refresh_token":"refresh-rotated","organization_id":"org_from_stub","workspace_id":"workspace_from_stub","scope":"llm_gateway:invoke sessions:read sessions:write"}"#.to_owned(),
                ),
                _ => (404, String::new(), r#"{"error":"not_found"}"#.to_owned()),
            };
            let reason = match status {
                200 => "OK",
                201 => "Created",
                302 => "Found",
                400 => "Bad Request",
                _ => "Not Found",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (format!("http://{addr}"), handle)
}

fn restore_env(name: &str, previous: Option<String>) {
    if let Some(previous) = previous {
        std::env::set_var(name, previous);
    } else {
        std::env::remove_var(name);
    }
}

#[test]
fn refresh_expiry_accepts_native_seconds_and_legacy_absolute_time() {
    let now = 1_000;
    assert_eq!(
        refresh_expiry(&json!({"expires_in": 3600}), now).unwrap(),
        3_601_000
    );
    assert_eq!(
        refresh_expiry(&json!({"expires_at": "1970-01-01T01:00:01Z"}), now).unwrap(),
        3_601_000
    );
    assert_eq!(
        refresh_expiry(
            &json!({"expires_in": 1, "expires_at": "ignored legacy value"}),
            now
        )
        .unwrap(),
        2_000
    );
}

#[test]
fn refresh_expiry_rejects_invalid_or_expired_values_without_legacy_fallback() {
    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("3600"),
        json!(true),
        Value::Null,
        json!(u64::MAX),
    ] {
        assert!(
            refresh_expiry(
                &json!({"expires_in": value, "expires_at": "2099-01-01T00:00:00Z"}),
                1_000
            )
            .is_err()
        );
    }
    for payload in [
        json!({}),
        json!({"expires_at": "invalid"}),
        json!({"expires_at": "1970-01-01T00:00:01Z"}),
    ] {
        assert!(refresh_expiry(&payload, 1_000).is_err());
    }
    assert!(refresh_expiry(&json!({"expires_in": 1}), i64::MAX).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_evalops_login_refreshes_without_browser() {
    let _guard = crate::config::test_process_env_lock_async().await;
    let home = tempfile::tempdir().expect("maestro home");
    let (identity, identity_task) = spawn_identity_stub(false).await;
    let previous_home = std::env::var("MAESTRO_HOME").ok();
    let previous_storage = std::env::var("MAESTRO_OAUTH_STORAGE_MODE").ok();
    let previous_keychain = std::env::var("MAESTRO_DISABLE_KEYCHAIN").ok();
    std::env::set_var("MAESTRO_HOME", home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");

    let mut metadata = Map::new();
    metadata.insert("identityBaseUrl".to_owned(), Value::String(identity));
    save_credentials(&OAuthCredentials {
        credential_type: "oauth".to_owned(),
        refresh: "refresh-from-stub".to_owned(),
        access: "access-expired".to_owned(),
        expires: Utc::now().timestamp_millis() - 1,
        metadata,
    })
    .expect("save expired credentials");

    let refresh_started_at = Utc::now().timestamp_millis();
    let snapshot = load_current_evalops_snapshot()
        .expect("refresh expired credentials")
        .expect("stored credentials");
    assert_eq!(snapshot.access, "access-refreshed");
    assert_eq!(snapshot.refresh, "refresh-rotated");
    assert_eq!(snapshot.organization_id.as_deref(), Some("org_from_stub"));
    assert!(
        (refresh_started_at + 3_600_000..=Utc::now().timestamp_millis() + 3_600_000)
            .contains(&snapshot.expires)
    );
    let stored = load_credentials()
        .expect("load refreshed credentials")
        .expect("credentials");
    assert_eq!(
        stored.metadata.get("scopes"),
        Some(&json!([
            "llm_gateway:invoke",
            "sessions:read",
            "sessions:write"
        ]))
    );
    // Identity re-issues the tenant binding on refresh. Dropping it here is
    // what left `workspaceId` absent from every stored credential.
    assert_eq!(
        stored.metadata.get("workspaceId"),
        Some(&json!("workspace_from_stub"))
    );

    delete_credentials().expect("delete test credentials");
    restore_env("MAESTRO_HOME", previous_home);
    restore_env("MAESTRO_OAUTH_STORAGE_MODE", previous_storage);
    restore_env("MAESTRO_DISABLE_KEYCHAIN", previous_keychain);
    identity_task.abort();
}

#[tokio::test]
async fn evalops_pkce_login_persists_a_platform_session() {
    let _guard = crate::config::test_process_env_lock_async().await;
    let home = tempfile::tempdir().expect("maestro home");
    let callback = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("callback probe")
        .local_addr()
        .expect("callback addr")
        .port();
    let (identity, identity_task) = spawn_identity_stub(false).await;
    let previous_home = std::env::var("MAESTRO_HOME").ok();
    let previous_storage = std::env::var("MAESTRO_OAUTH_STORAGE_MODE").ok();
    let previous_keychain = std::env::var("MAESTRO_DISABLE_KEYCHAIN").ok();
    let previous_identity = std::env::var("MAESTRO_IDENTITY_URL").ok();
    let previous_browser = std::env::var("MAESTRO_OAUTH_OPEN_BROWSER").ok();
    let previous_port = std::env::var("MAESTRO_OAUTH_CALLBACK_PORT").ok();
    let previous_token = std::env::var("MAESTRO_EVALOPS_ACCESS_TOKEN").ok();
    let previous_org = std::env::var("MAESTRO_EVALOPS_ORG_ID").ok();
    let previous_workspace = std::env::var("MAESTRO_EVALOPS_WORKSPACE_ID").ok();
    std::env::set_var("MAESTRO_HOME", home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var("MAESTRO_IDENTITY_URL", &identity);
    std::env::set_var("MAESTRO_OAUTH_OPEN_BROWSER", "0");
    std::env::set_var("MAESTRO_OAUTH_CALLBACK_PORT", callback.to_string());
    std::env::remove_var("MAESTRO_EVALOPS_ACCESS_TOKEN");
    std::env::remove_var("MAESTRO_EVALOPS_ORG_ID");
    std::env::set_var("MAESTRO_EVALOPS_WORKSPACE_ID", "workspace_from_stub");

    let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
    let result = perform_evalops_login_in_tui(url_tx).await;
    let authorization_url = url_rx.try_recv().expect("TUI receives the login link");
    assert!(authorization_url.starts_with(&format!("{identity}/authorize?")));
    identity_task.abort();
    let snapshot = match result {
        Ok(()) => load_evalops_snapshot()
            .expect("load snapshot")
            .expect("stored session"),
        Err(error) => {
            restore_env("MAESTRO_HOME", previous_home);
            restore_env("MAESTRO_OAUTH_STORAGE_MODE", previous_storage);
            restore_env("MAESTRO_DISABLE_KEYCHAIN", previous_keychain);
            restore_env("MAESTRO_IDENTITY_URL", previous_identity);
            restore_env("MAESTRO_OAUTH_OPEN_BROWSER", previous_browser);
            restore_env("MAESTRO_OAUTH_CALLBACK_PORT", previous_port);
            restore_env("MAESTRO_EVALOPS_ACCESS_TOKEN", previous_token);
            restore_env("MAESTRO_EVALOPS_ORG_ID", previous_org);
            restore_env("MAESTRO_EVALOPS_WORKSPACE_ID", previous_workspace);
            panic!("PKCE login should succeed against the identity stub: {error:#}");
        }
    };
    let stored = load_credentials()
        .expect("load PKCE credentials")
        .expect("stored PKCE credentials");
    restore_env("MAESTRO_HOME", previous_home);
    restore_env("MAESTRO_OAUTH_STORAGE_MODE", previous_storage);
    restore_env("MAESTRO_DISABLE_KEYCHAIN", previous_keychain);
    restore_env("MAESTRO_IDENTITY_URL", previous_identity);
    restore_env("MAESTRO_OAUTH_OPEN_BROWSER", previous_browser);
    restore_env("MAESTRO_OAUTH_CALLBACK_PORT", previous_port);
    restore_env("MAESTRO_EVALOPS_ACCESS_TOKEN", previous_token);
    restore_env("MAESTRO_EVALOPS_ORG_ID", previous_org);

    // The issuer fixture grants the scopes captured from the real authorize
    // request. Exact equality excludes console write/admin and wildcard grants.
    assert_eq!(
        stored.metadata.get("scopes"),
        Some(&json!([
            "llm_gateway:invoke",
            "sessions:read",
            "sessions:write",
            "product_issues:write",
            "console:read"
        ]))
    );
    assert_eq!(snapshot.access, "access-from-stub");
    assert_eq!(snapshot.refresh, "refresh-from-stub");
    assert_eq!(snapshot.organization_id.as_deref(), Some("org_from_stub"));
    // The workspace Identity returned is stored, so callers no longer have
    // to recover it from MAESTRO_EVALOPS_WORKSPACE_ID.
    assert_eq!(
        stored.metadata.get("workspaceId"),
        Some(&json!("workspace_from_stub"))
    );
    let mode = crate::credential_mode::detect_from(
        Some(&snapshot),
        &std::collections::HashMap::from([(
            "MAESTRO_EVALOPS_WORKSPACE_ID".to_owned(),
            "workspace_from_stub".to_owned(),
        )]),
    )
    .expect("mode");
    assert!(mode.is_platform());
    let crate::credential_mode::DetectedMode::Platform(session) = mode else {
        panic!("expected platform session");
    };
    let env = session
        .managed_env(
            "anthropic/claude-opus-4-6",
            &std::collections::HashMap::new(),
        )
        .expect("managed env");
    crate::ai::UnifiedClient::from_model_with_env(
        &session.managed_model_route("anthropic/claude-opus-4-6"),
        &env,
    )
    .expect("platform session must construct the llm-gateway client");
    restore_env("MAESTRO_EVALOPS_WORKSPACE_ID", previous_workspace);
}

#[tokio::test]
async fn evalops_login_rejects_callback_state_mismatch() {
    let _guard = crate::config::test_process_env_lock_async().await;
    let home = tempfile::tempdir().expect("maestro home");
    let callback = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("callback probe")
        .local_addr()
        .expect("callback addr")
        .port();
    let (identity, identity_task) = spawn_identity_stub(true).await;
    let previous_home = std::env::var("MAESTRO_HOME").ok();
    let previous_storage = std::env::var("MAESTRO_OAUTH_STORAGE_MODE").ok();
    let previous_keychain = std::env::var("MAESTRO_DISABLE_KEYCHAIN").ok();
    let previous_identity = std::env::var("MAESTRO_IDENTITY_URL").ok();
    let previous_browser = std::env::var("MAESTRO_OAUTH_OPEN_BROWSER").ok();
    let previous_port = std::env::var("MAESTRO_OAUTH_CALLBACK_PORT").ok();
    std::env::set_var("MAESTRO_HOME", home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var("MAESTRO_IDENTITY_URL", &identity);
    std::env::set_var("MAESTRO_OAUTH_OPEN_BROWSER", "0");
    std::env::set_var("MAESTRO_OAUTH_CALLBACK_PORT", callback.to_string());

    let result = perform_evalops_login().await;
    restore_env("MAESTRO_HOME", previous_home);
    restore_env("MAESTRO_OAUTH_STORAGE_MODE", previous_storage);
    restore_env("MAESTRO_DISABLE_KEYCHAIN", previous_keychain);
    restore_env("MAESTRO_IDENTITY_URL", previous_identity);
    restore_env("MAESTRO_OAUTH_OPEN_BROWSER", previous_browser);
    restore_env("MAESTRO_OAUTH_CALLBACK_PORT", previous_port);
    identity_task.abort();
    assert!(result.is_err(), "mismatched callback state must fail login");
    assert!(
        result.unwrap_err().to_string().contains("state"),
        "error should mention state"
    );
}
