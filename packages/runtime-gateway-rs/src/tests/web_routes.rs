#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn query_flag_treats_present_valueless_param_as_true() {
    let head = parse_request_head(
        b"GET /artifact?download&standalone=0 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .expect("request should parse");

    assert!(query_flag(&head, "download"));
    assert!(!query_flag(&head, "standalone"));
    assert!(!query_flag(&head, "missing"));
}

#[test]
pub(crate) fn detects_local_control_plane_routes() {
    let request = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let head = parse_request_head(request).expect("request should parse");

    assert!(is_local_endpoint(&head));
}

#[test]
pub(crate) fn detects_migrated_web_api_routes() {
    for target in [
        "/api/model",
        "/api/files",
        "/api/commands",
        "/api/config",
        "/api/usage",
        "/api/approvals",
        "/api/telemetry",
        "/api/training",
        "/api/sessions",
        "/api/sessions/session-1",
        "/api/sessions/session-1/timeline",
        "/api/sessions/session-1/artifacts",
        "/api/sessions/session-1/artifact-access",
        "/api/sessions/session-1/artifacts/report.html",
        "/api/sessions/session-1/attachments/file-1",
        "/api/sessions/shared/session-1",
        "/api/sessions/shared/session-1/attachments/file-1",
    ] {
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let head = parse_request_head(request.as_bytes()).expect("request should parse");
        assert!(is_local_endpoint(&head), "{target} should be local");
    }
}

#[test]
pub(crate) fn detects_api_options_preflight_as_local() {
    let _guard = ENV_LOCK.blocking_lock();

    let head = parse_request_head(
            b"OPTIONS /api/chat HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost:4173\r\nAccess-Control-Request-Method: POST\r\n\r\n",
        )
        .expect("request should parse");
    assert!(is_local_endpoint(&head));

    let response = response(204, "text/plain; charset=utf-8", &[]);
    let text = String::from_utf8(response).expect("response should be utf-8");
    assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
    assert!(text.contains("Access-Control-Allow-Methods: GET,POST,PATCH,DELETE,OPTIONS\r\n"));
}

#[test]
pub(crate) fn artifact_access_actions_decode_and_filter_query_actions() {
    let actions = artifact_access_actions(Some(&"view%2Cfile%2Cbad%2Czip%2Cfile".to_string()))
        .expect("valid actions should be extracted");
    assert_eq!(actions, vec!["view", "file", "zip"]);
}

#[test]
pub(crate) fn artifact_view_wraps_html_in_sandboxed_viewer() {
    let mut session = create_session_record(Some("Artifacts".to_string()), None);
    session.messages.push(composer_assistant_message_with_tools(
        "created",
        "",
        None,
        &[serde_json::json!({
            "id": "tool-1",
            "name": "artifacts",
            "status": "completed",
            "args": {
                "command": "create",
                "filename": "report.html",
                "content": "<script>window.top.location='https://example.com'</script>"
            },
            "result": { "isError": false }
        })],
    ));

    let head = RequestHead {
        method: "GET".to_string(),
        path: "/api/sessions/session-1/artifacts/report.html/view".to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
    };
    let response = serve_session_artifact(&head, &session, "artifacts/report.html/view");
    let text = String::from_utf8(response).expect("response should be utf-8");

    assert!(text.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8"));
    assert!(text.contains("Content-Security-Policy: default-src 'none'"));
    assert!(text.contains("sandbox=\"allow-scripts allow-forms allow-popups allow-downloads\""));
    assert!(text.contains(
        "srcdoc=\"&lt;script&gt;window.top.location=&#39;https://example.com&#39;&lt;/script&gt;\""
    ));
    assert!(!text.contains("<script>window.top.location"));
}

#[tokio::test]
pub(crate) async fn usage_buckets_include_contract_breakdown_fields() {
    let path = env::temp_dir().join(format!(
        "maestro-usage-{}-{}.json",
        process::id(),
        ATTACHMENT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let usage = serde_json::json!([{
        "provider": "openai",
        "model": "gpt-5.1-codex-max",
        "tokensInput": 10,
        "tokensOutput": 20,
        "tokensCacheRead": 3,
        "tokensCacheWrite": 4,
        "cost": 0.12
    }]);
    tokio::fs::write(&path, usage.to_string())
        .await
        .expect("usage file should be written");

    let snapshot = usage_snapshot(&path).await;
    let provider = snapshot
        .pointer("/summary/byProvider/openai")
        .expect("provider bucket should exist");
    let model = snapshot
        .pointer("/summary/byModel")
        .and_then(|models| models.get("openai/gpt-5.1-codex-max"))
        .expect("model bucket should exist");

    assert_eq!(provider.get("calls").and_then(Value::as_u64), Some(1));
    assert_eq!(
        provider.get("cachedTokens").and_then(Value::as_u64),
        Some(7)
    );
    assert_eq!(model.get("calls").and_then(Value::as_u64), Some(1));
    assert_eq!(model.get("cachedTokens").and_then(Value::as_u64), Some(7));

    let _ = tokio::fs::remove_file(path).await;
}

#[test]
pub(crate) fn onboarding_snapshot_honors_seen_count_limit() {
    let first_seen = compute_onboarding_snapshot(false, false, 1, false);
    assert!(first_seen.should_show);
    assert_eq!(first_seen.seen_count, 1);

    let capped =
        compute_onboarding_snapshot(false, false, MAX_PROJECT_ONBOARDING_IMPRESSIONS, false);
    assert!(!capped.should_show);
    assert_eq!(capped.seen_count, MAX_PROJECT_ONBOARDING_IMPRESSIONS);
}

#[test]
pub(crate) fn share_options_honor_expiry_and_access_limits() {
    let options = share_options_from_value(&serde_json::json!({
        "expiresInHours": 999,
        "maxAccesses": 0
    }));

    assert_eq!(options.expires_in_hours, 168);
    assert_eq!(options.max_accesses, Some(1));
    assert!(!options.allow_sensitive_content);

    let unlimited = share_options_from_value(&serde_json::json!({
        "maxAccesses": Value::Null,
        "allowSensitiveContent": true
    }));
    assert_eq!(unlimited.max_accesses, None);
    assert!(unlimited.allow_sensitive_content);
}

#[test]
pub(crate) fn export_options_require_explicit_sensitive_confirmation() {
    let default_options = export_options_from_body(&[]).expect("empty body should parse");
    assert_eq!(default_options.format, "json");
    assert!(!default_options.allow_sensitive_content);

    let confirmed = export_options_from_value(&serde_json::json!({
        "format": "markdown",
        "allowSensitiveContent": true
    }));
    assert_eq!(confirmed.format, "markdown");
    assert!(confirmed.allow_sensitive_content);
}

#[test]
pub(crate) fn timeline_items_use_run_timeline_schema() {
    let mut session = test_session_record("session-1");
    session.messages.push(serde_json::json!({
        "role": "user",
        "timestamp": "2026-04-27T00:00:00Z",
        "content": "hello"
    }));

    let timeline = session_timeline_value(&session);
    assert_eq!(timeline["sessionId"], "session-1");
    assert_eq!(timeline["items"][0]["sessionId"], "session-1");
    assert_eq!(timeline["items"][0]["type"], "message.user");
    assert_eq!(timeline["items"][0]["visibility"], "user");
}

#[test]
pub(crate) fn openrouter_catalog_requires_explicit_configuration() {
    let _guard = ENV_LOCK.blocking_lock();
    let vars = [
        "MAESTRO_LLM_GATEWAY_MODELS_URL",
        "MAESTRO_LLM_GATEWAY_URL",
        "MAESTRO_OPENROUTER_MODELS_URL",
        "MAESTRO_ENABLE_OPENROUTER_MODELS",
        "MAESTRO_OPENROUTER_API_KEY",
        "OPENROUTER_API_KEY",
    ];
    let previous = vars.map(|name| (name, env::var_os(name)));
    for name in vars {
        env::remove_var(name);
    }

    assert!(llm_gateway_models_url().is_none());

    env::set_var("MAESTRO_ENABLE_OPENROUTER_MODELS", "1");
    assert_eq!(
        llm_gateway_models_url().as_deref(),
        Some("https://openrouter.ai/api/v1/models")
    );

    env::set_var("MAESTRO_ENABLE_OPENROUTER_MODELS", "0");
    assert!(llm_gateway_models_url().is_none());

    env::set_var("MAESTRO_OPENROUTER_API_KEY", "key");
    assert_eq!(
        llm_gateway_models_url().as_deref(),
        Some("https://openrouter.ai/api/v1/models")
    );

    for (name, value) in previous {
        if let Some(value) = value {
            env::set_var(name, value);
        } else {
            env::remove_var(name);
        }
    }
}

#[test]
pub(crate) fn action_body_rejects_missing_or_unknown_actions() {
    assert_eq!(
        parse_action_body(br#"{"action":"reset"}"#, &["on", "off", "reset"])
            .expect("reset should parse"),
        "reset"
    );
    assert!(parse_action_body(br#"{}"#, &["on", "off", "reset"]).is_err());
    assert!(parse_action_body(br#"{"action":"maybe"}"#, &["on", "off", "reset"]).is_err());
}

#[test]
pub(crate) fn training_on_maps_to_opted_in() {
    let status = training_status(Some(false));

    assert_eq!(
        status.get("preference").and_then(Value::as_str),
        Some("opted-in")
    );
    assert_eq!(status.get("optOut").and_then(Value::as_bool), Some(false));
}

#[test]
pub(crate) fn telemetry_flag_accepts_common_truthy_values() {
    for value in ["1", "true", "TRUE", "True", "yes", "YES", "on", "On"] {
        assert!(
            telemetry_enabled(None, Some(value), false, false),
            "{value} should enable telemetry"
        );
    }
}

#[test]
pub(crate) fn telemetry_explicit_false_overrides_endpoint_and_file_configuration() {
    for value in ["0", "false", "FALSE", "False", "no", "NO", "off", "Off"] {
        assert!(
            !telemetry_enabled(None, Some(value), true, true),
            "{value} should disable telemetry"
        );
    }
}

#[test]
pub(crate) fn native_run_event_route_is_implemented() {
    let head = csrf_head_for_path("POST", "/api/run/event", None);
    assert!(is_extended_endpoint(&head));
}

#[test]
pub(crate) fn enterprise_policy_admin_routes_are_implemented() {
    for (method, path) in [
        ("GET", "/api/admin/enterprise-policy/status"),
        ("POST", "/api/admin/enterprise-policy/refresh"),
        ("POST", "/api/admin/enterprise-policy/publish"),
        ("GET", "/api/admin/enterprise-policy/audit"),
    ] {
        let head = csrf_head_for_path(method, path, None);
        assert!(is_extended_endpoint(&head));
    }
}

// --- Security fix: gateway tenant scope + administrator role ---------------
//
// These tests pin the authorization boundary for the extended API, MCP config,
// admin/enterprise-policy, pending-request resume, and platform A2A push. Each
// asserts the fixed behavior; the origin/main behavior it replaces is noted per
// test.

pub(crate) fn security_fix_remote_env_snapshot() -> Vec<(&'static str, Option<std::ffi::OsString>)>
{
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
    env::set_var("MAESTRO_JWT_ALG", "HS256");
    env::set_var("MAESTRO_JWT_AUD", "evalops.maestro");
    env::set_var("MAESTRO_JWT_ISS", "evalops.platform");
    env::remove_var("MAESTRO_PROFILE");
    env::remove_var("MAESTRO_WEB_PROFILE");
    env::remove_var("MAESTRO_HOSTED_RUNNER_MODE");
    env::remove_var("MAESTRO_HOSTED_RUNNER");
    env::remove_var("MAESTRO_RUNNER_KIND");
    env::remove_var("MAESTRO_WEB_API_KEY_SCOPES");
    snapshot
}

// Items 1 and 4 (administrator role): a tenant-bound write principal could
// publish the process-wide managed safety policy and rewrite MCP server config
// on origin/main. `authorize_extended` now requires the administrator role for
// `/api/admin/*` and MCP-config mutations.
#[test]
pub(crate) fn extended_admin_and_mcp_mutations_require_admin_role_on_remote() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = security_fix_remote_env_snapshot();

    let mut remote_config = auth_test_config();
    remote_config.listen_host = "0.0.0.0".to_string();

    let tenant_write = security_fix_token(serde_json::json!({
        "sub": "writer",
        "organization_id": "org-a",
        "workspace_id": "workspace-a",
        "scope": "maestro:write",
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
        "exp": now_secs() + 60,
    }));
    let tenant_admin = security_fix_token(serde_json::json!({
        "sub": "admin",
        "organization_id": "org-a",
        "workspace_id": "workspace-a",
        "scope": "maestro:write maestro:admin",
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
        "exp": now_secs() + 60,
    }));

    for (method, path) in [
        ("POST", "/api/mcp"),
        ("POST", "/api/admin/enterprise-policy/publish"),
        ("GET", "/api/admin/enterprise-policy/status"),
    ] {
        assert!(
            authorize_extended(&auth_head(method, path, &tenant_write), &remote_config).is_err(),
            "{method} {path} must require the administrator role"
        );
        assert!(
            authorize_extended(&auth_head(method, path, &tenant_admin), &remote_config).is_ok(),
            "{method} {path} accepts the administrator role"
        );
    }

    // A non-admin extended data path is still reachable by a tenant-bound write
    // principal; the role gate is scoped to admin/MCP-config routes only.
    assert!(
        authorize_extended(
            &auth_head("GET", "/api/automations", &tenant_write),
            &remote_config
        )
        .is_ok(),
        "tenant write principal keeps non-admin extended access"
    );

    restore_env(snapshot);
}
