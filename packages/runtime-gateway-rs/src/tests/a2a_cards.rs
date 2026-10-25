#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn a2a_platform_registration_defaults_to_hosted_mode() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    assert!(!a2a_platform_registration_enabled());
    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    assert!(a2a_platform_registration_enabled());
    env::set_var("MAESTRO_A2A_PLATFORM_REGISTER", "0");
    assert!(!a2a_platform_registration_enabled());
    env::set_var("MAESTRO_A2A_PLATFORM_REGISTER", "1");
    assert!(a2a_platform_registration_enabled());

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_registration_config_uses_platform_env_and_stable_endpoint() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    env::set_var(
        "AGENT_REGISTRY_SERVICE_URL",
        "https://platform.test/agents.v1.AgentService/Register",
    );
    env::set_var("AGENT_REGISTRY_SERVICE_TOKEN", "registry-token");
    env::set_var("AGENT_REGISTRY_ORGANIZATION_ID", "org_1");
    env::set_var("AGENT_REGISTRY_WORKSPACE_ID", "ws_1");
    env::set_var("MAESTRO_A2A_PUBLIC_URL", "https://maestro.example/a2a/");
    env::set_var("MAESTRO_A2A_INTERNAL_URL", "http://maestro.mesh/a2a");
    env::set_var("MAESTRO_A2A_AGENT_ID", "maestro-peer-1");
    env::set_var(
        "MAESTRO_A2A_TRACEPARENT",
        "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
    );
    env::set_var("MAESTRO_A2A_TRACESTATE", "evalops=maestro");
    env::set_var("MAESTRO_A2A_CURRENT_OBJECTIVE_IDS", "obj_1, obj_2");
    env::set_var("MAESTRO_A2A_PLATFORM_HEARTBEAT_INTERVAL_MS", "1234");
    env::set_var(
        "TRACEPARENT",
        "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01",
    );
    env::set_var("TRACESTATE", "evalops=maestro-a2a");
    env::set_var("MAESTRO_RUNNER_SESSION_ID", "mrs_123");
    env::set_var("PORT", "18787");

    let config = Config::from_env();
    let registration = resolve_a2a_platform_registration_config(&config)
        .expect("registration config should resolve")
        .expect("hosted mode should enable registration");

    assert_eq!(registration.base_url, "https://platform.test");
    assert_eq!(registration.token, "registry-token");
    assert_eq!(registration.organization_id, "org_1");
    assert_eq!(registration.workspace_id, "ws_1");
    assert_eq!(registration.agent_id, "maestro-peer-1");
    assert_eq!(
        registration.public_endpoint_url,
        "https://maestro.example/a2a/"
    );
    assert_eq!(
        registration.internal_endpoint_url.as_deref(),
        Some("http://maestro.mesh/a2a")
    );
    assert_eq!(
        registration.current_objective_ids,
        vec!["obj_1".to_string(), "obj_2".to_string()]
    );
    assert_eq!(
        registration.traceparent.as_deref(),
        Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01")
    );
    assert_eq!(registration.tracestate.as_deref(), Some("evalops=maestro"));
    assert_eq!(registration.heartbeat_interval_ms, 1234);
    assert_eq!(
        registration.remote_runner_session_id.as_deref(),
        Some("mrs_123")
    );

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_registration_binds_tracestate_to_traceparent_namespace() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    env::set_var("AGENT_REGISTRY_SERVICE_URL", "https://platform.test");
    env::set_var("AGENT_REGISTRY_SERVICE_TOKEN", "registry-token");
    env::set_var("AGENT_REGISTRY_ORGANIZATION_ID", "org_1");
    env::set_var("AGENT_REGISTRY_WORKSPACE_ID", "ws_1");
    env::set_var("MAESTRO_A2A_PUBLIC_URL", "https://maestro.example/a2a");
    env::set_var(
        "MAESTRO_PLATFORM_TRACEPARENT",
        "00-11111111111111111111111111111111-2222222222222222-01",
    );
    env::set_var("MAESTRO_A2A_TRACESTATE", "evalops=stale-a2a");
    env::set_var("MAESTRO_PLATFORM_TRACESTATE", "evalops=platform");
    env::set_var("TRACESTATE", "evalops=stale-process");

    let config = Config::from_env();
    let registration = resolve_a2a_platform_registration_config(&config)
        .expect("registration config should resolve")
        .expect("hosted mode should enable registration");

    assert_eq!(
        registration.traceparent.as_deref(),
        Some("00-11111111111111111111111111111111-2222222222222222-01")
    );
    assert_eq!(registration.tracestate.as_deref(), Some("evalops=platform"));

    env::remove_var("MAESTRO_PLATFORM_TRACESTATE");
    let config = Config::from_env();
    let registration = resolve_a2a_platform_registration_config(&config)
        .expect("registration config should resolve")
        .expect("hosted mode should enable registration");

    assert_eq!(
        registration.traceparent.as_deref(),
        Some("00-11111111111111111111111111111111-2222222222222222-01")
    );
    assert!(registration.tracestate.is_none());

    env::remove_var("MAESTRO_PLATFORM_TRACEPARENT");
    env::set_var(
        "TRACEPARENT",
        "00-33333333333333333333333333333333-4444444444444444-01",
    );
    let config = Config::from_env();
    let registration = resolve_a2a_platform_registration_config(&config)
        .expect("registration config should resolve")
        .expect("hosted mode should enable registration");

    assert_eq!(
        registration.traceparent.as_deref(),
        Some("00-33333333333333333333333333333333-4444444444444444-01")
    );
    assert_eq!(
        registration.tracestate.as_deref(),
        Some("evalops=stale-process")
    );

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_registration_ignores_orphan_tracestate_env() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    env::set_var("AGENT_REGISTRY_SERVICE_URL", "https://platform.test");
    env::set_var("AGENT_REGISTRY_SERVICE_TOKEN", "registry-token");
    env::set_var("AGENT_REGISTRY_ORGANIZATION_ID", "org_1");
    env::set_var("AGENT_REGISTRY_WORKSPACE_ID", "ws_1");
    env::set_var("MAESTRO_A2A_PUBLIC_URL", "https://maestro.example/a2a");
    env::set_var("TRACESTATE", "evalops=stale-process-state");

    let config = Config::from_env();
    let registration = resolve_a2a_platform_registration_config(&config)
        .expect("registration config should resolve")
        .expect("hosted mode should enable registration");

    assert!(registration.traceparent.is_none());
    assert!(registration.tracestate.is_none());

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_registration_requires_routable_endpoint_in_hosted_default() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    env::set_var("AGENT_REGISTRY_SERVICE_URL", "https://platform.test");
    env::set_var("AGENT_REGISTRY_SERVICE_TOKEN", "registry-token");
    env::set_var("AGENT_REGISTRY_ORGANIZATION_ID", "org_1");
    env::set_var("AGENT_REGISTRY_WORKSPACE_ID", "ws_1");

    let config = Config::from_env();
    let error = resolve_a2a_platform_registration_config(&config)
        .expect_err("hosted default should require a routable public endpoint");
    assert!(error.contains("routable A2A public URL or public host"));

    env::set_var("MAESTRO_A2A_PLATFORM_REGISTER", "1");
    assert!(
        resolve_a2a_platform_registration_config(&config)
            .expect("explicit local registration should resolve")
            .is_some()
    );

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_registration_requires_explicit_workspace_binding() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    env::set_var("MAESTRO_HOSTED_RUNNER_MODE", "1");
    env::set_var("AGENT_REGISTRY_SERVICE_URL", "https://platform.test");
    env::set_var("AGENT_REGISTRY_SERVICE_TOKEN", "registry-token");
    env::set_var("AGENT_REGISTRY_ORGANIZATION_ID", "org_1");
    env::set_var("MAESTRO_A2A_PUBLIC_URL", "https://maestro.example/a2a");

    let config = Config::from_env();
    let error = resolve_a2a_platform_registration_config(&config)
        .expect_err("registration must fail closed without workspace scope");
    assert!(error.contains("EvalOps workspace id"), "{error}");
    assert!(error.contains("MAESTRO_A2A_PLATFORM_REGISTER=0"), "{error}");

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_payload_projects_governed_agent_card_without_drift_fields() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    let config = Config::from_env();
    let registration = A2APlatformRegistrationConfig {
        base_url: "https://platform.test".to_string(),
        token: "registry-token".to_string(),
        organization_id: "org_1".to_string(),
        workspace_id: "ws_1".to_string(),
        agent_id: "maestro-peer-1".to_string(),
        name: "Maestro Peer".to_string(),
        description: "Peer".to_string(),
        agent_type: "maestro".to_string(),
        owner_id: None,
        public_endpoint_url: "https://maestro.example/a2a".to_string(),
        internal_endpoint_url: Some("http://maestro.mesh/a2a".to_string()),
        agent_card_url: None,
        heartbeat_interval_ms: 60_000,
        timeout_ms: 2_500,
        current_objective_ids: vec!["obj_1".to_string()],
        max_concurrent_objectives: "4".to_string(),
        surface: "a2a".to_string(),
        surface_type: "SURFACE_MAESTRO".to_string(),
        traceparent: Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".to_string()),
        tracestate: Some("evalops=maestro-a2a".to_string()),
        remote_runner_session_id: Some("mrs_123".to_string()),
    };

    let payload = a2a_platform_register_payload(&registration, &config);
    assert_eq!(payload["id"], "maestro-peer-1");
    assert_eq!(
        payload["a2a"]["publicEndpointUrl"],
        "https://maestro.example/a2a"
    );
    assert_eq!(
        payload["a2a"]["agentCardUrl"],
        "https://maestro.example/a2a/.well-known/agent-card.json"
    );
    assert_eq!(payload["a2a"]["attributes"]["maxConcurrentObjectives"], "4");
    assert_eq!(payload["a2a"]["attributes"]["organizationId"], "org_1");
    assert_eq!(payload["a2a"]["attributes"]["workspaceId"], "ws_1");
    assert_eq!(payload["a2a"]["attributes"]["agentId"], "maestro-peer-1");
    assert_eq!(
        payload["a2a"]["attributes"]["traceparent"],
        "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01"
    );
    assert_eq!(
        payload["a2a"]["attributes"]["tracestate"],
        "evalops=maestro-a2a"
    );
    assert_eq!(
        payload["a2a"]["attributes"]["remoteRunnerSessionId"],
        "mrs_123"
    );
    assert!(
        payload["capabilities"]
            .as_array()
            .expect("capabilities")
            .contains(&Value::String("code:review".to_string()))
    );
    assert!(
        !payload["capabilities"]
            .as_array()
            .expect("capabilities")
            .contains(&Value::String("browser:qa".to_string()))
    );

    let skills = payload["a2a"]["skills"].as_array().expect("skills");
    let review = skills
        .iter()
        .find(|skill| skill["id"] == "maestro.subagent.code-review")
        .expect("review skill should be advertised");
    assert_eq!(review["requiredContextGrants"][0], "repo:read");
    assert_eq!(review["allowedTaskClasses"][0], "code.review");
    assert!(review.get("metadata").is_none());
    assert!(review.get("examples").is_none());
    assert!(
        skills
            .iter()
            .all(|skill| skill["id"] != "maestro.subagent.browser-qa")
    );

    let heartbeat = a2a_platform_heartbeat_payload(&registration, &config);
    assert_eq!(heartbeat["agentId"], "maestro-peer-1");
    assert_eq!(heartbeat["currentObjectiveIds"][0], "obj_1");
    assert_eq!(heartbeat["surface"], "a2a");
    assert_eq!(heartbeat["surfaceType"], "SURFACE_MAESTRO");

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_agent_card_skills_and_hosted_capabilities_share_executable_lanes() {
    let config = auth_test_config();
    let registration = A2APlatformRegistrationConfig {
        base_url: "https://registry.example".to_string(),
        token: "token".to_string(),
        organization_id: "org_1".to_string(),
        workspace_id: "ws_1".to_string(),
        agent_id: "maestro-peer-1".to_string(),
        name: "Maestro Peer".to_string(),
        description: "Peer".to_string(),
        agent_type: "maestro".to_string(),
        owner_id: None,
        public_endpoint_url: "https://maestro.example/a2a".to_string(),
        internal_endpoint_url: None,
        agent_card_url: None,
        heartbeat_interval_ms: 60_000,
        timeout_ms: 2_500,
        current_objective_ids: Vec::new(),
        max_concurrent_objectives: "4".to_string(),
        surface: "a2a".to_string(),
        surface_type: "SURFACE_MAESTRO".to_string(),
        traceparent: None,
        tracestate: None,
        remote_runner_session_id: None,
    };

    let agent_skills = a2a_agent_skills();
    let card_lanes = agent_skills
        .as_array()
        .expect("agent-card skills should be an array")
        .iter()
        .filter_map(|skill| skill["attributes"]["subagentLaneId"].as_str())
        .collect::<Vec<_>>();
    let payload = a2a_platform_register_payload(&registration, &config);
    let capabilities = payload["capabilities"]
        .as_array()
        .expect("hosted capabilities should be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let hosted_lanes = capabilities
        .iter()
        .filter_map(|capability| capability.strip_prefix("maestro:"))
        .filter(|capability| !matches!(*capability, "a2a" | "cli" | "subagents"))
        .collect::<Vec<_>>();

    assert_eq!(card_lanes, vec!["code-writer", "code-review"]);
    assert_eq!(hosted_lanes, card_lanes);
    assert_eq!(
        capabilities,
        vec![
            "maestro:a2a",
            "maestro:cli",
            "maestro:subagents",
            "maestro:code-writer",
            "code:write",
            "code:edit",
            "code:implement",
            "maestro:code-review",
            "code:review",
        ]
    );
}

#[test]
pub(crate) fn a2a_platform_registration_posts_update_after_conflict_and_heartbeat() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let server = thread::spawn(move || {
        let register =
            capture_http_request(&listener, "409 Conflict", r#"{"message":"already exists"}"#);
        let update =
            capture_http_request(&listener, "200 OK", r#"{"agent":{"id":"maestro-peer-1"}}"#);
        let heartbeat = capture_http_request(
            &listener,
            "200 OK",
            r#"{"nextHeartbeatBy":"2026-05-20T10:05:00Z"}"#,
        );
        vec![register, update, heartbeat]
    });

    let config = Config::from_env();
    let registration = A2APlatformRegistrationConfig {
        base_url: format!("http://{addr}"),
        token: "registry-token".to_string(),
        organization_id: "org_1".to_string(),
        workspace_id: "ws_1".to_string(),
        agent_id: "maestro-peer-1".to_string(),
        name: "Maestro Peer".to_string(),
        description: "Peer".to_string(),
        agent_type: "maestro".to_string(),
        owner_id: Some("user_1".to_string()),
        public_endpoint_url: "https://maestro.example/a2a".to_string(),
        internal_endpoint_url: Some("http://maestro.mesh/a2a".to_string()),
        agent_card_url: None,
        heartbeat_interval_ms: 60_000,
        timeout_ms: 2_500,
        current_objective_ids: vec!["obj_1".to_string()],
        max_concurrent_objectives: "4".to_string(),
        surface: "a2a".to_string(),
        surface_type: "SURFACE_MAESTRO".to_string(),
        traceparent: Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".to_string()),
        tracestate: Some("evalops=maestro-a2a".to_string()),
        remote_runner_session_id: Some("mrs_123".to_string()),
    };

    register_or_update_a2a_platform_agent(&registration, &config)
        .expect("conflict should fall back to update");
    send_a2a_platform_heartbeat(&registration, &config)
        .expect("heartbeat should post after update");

    let requests = server.join().expect("test server should finish");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.request_line.as_str())
            .collect::<Vec<_>>(),
        vec![
            "POST /agents.v1.AgentService/Register HTTP/1.1",
            "POST /agents.v1.AgentService/Update HTTP/1.1",
            "POST /agents.v1.AgentService/Heartbeat HTTP/1.1"
        ]
    );
    for request in &requests {
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer registry-token")
        );
        assert_eq!(
            request
                .headers
                .get("connect-protocol-version")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            request.headers.get("x-organization-id").map(String::as_str),
            Some("org_1")
        );
        assert_eq!(
            request.headers.get("x-workspace-id").map(String::as_str),
            Some("ws_1")
        );
        assert_eq!(
            request
                .headers
                .get("x-evalops-agent-id")
                .map(String::as_str),
            Some("maestro-peer-1")
        );
        assert_eq!(
            request
                .headers
                .get("x-evalops-actor-id")
                .map(String::as_str),
            Some("user_1")
        );
        let traceparent = request
            .headers
            .get("traceparent")
            .expect("internal callback carries a child traceparent");
        let parts = traceparent.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_ne!(parts[2], "bbbbbbbbbbbbbbbb");
        assert_eq!(parts[3], "01");
        assert_eq!(
            request.headers.get("tracestate").map(String::as_str),
            Some("evalops=maestro-a2a")
        );
    }
    assert_eq!(requests[0].body["workspaceId"], "ws_1");
    assert_eq!(requests[0].body["ownerId"], "user_1");
    assert_eq!(
        requests[0].body["a2a"]["publicEndpointUrl"],
        "https://maestro.example/a2a"
    );
    assert!(requests[1].body.get("workspaceId").is_none());
    assert_eq!(requests[1].body["id"], "maestro-peer-1");
    assert_eq!(requests[2].body["agentId"], "maestro-peer-1");
    assert_eq!(requests[2].body["currentObjectiveIds"][0], "obj_1");
    for request in &requests {
        assert_eq!(
            request.body["a2a"]["attributes"]["traceparent"],
            "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01"
        );
        assert_eq!(
            request.body["a2a"]["attributes"]["tracestate"],
            "evalops=maestro-a2a"
        );
        assert_eq!(request.body["a2a"]["attributes"]["workspaceId"], "ws_1");
    }

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_registration_does_not_forward_orphan_tracestate() {
    let _guard = ENV_LOCK.blocking_lock();
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(A2A_PLATFORM_ENV_NAMES);

    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let server = thread::spawn(move || {
        capture_http_request(&listener, "200 OK", r#"{"agent":{"id":"maestro-peer-1"}}"#)
    });

    let config = Config::from_env();
    let registration = A2APlatformRegistrationConfig {
        base_url: format!("http://{addr}"),
        token: "registry-token".to_string(),
        organization_id: "org_1".to_string(),
        workspace_id: "ws_1".to_string(),
        agent_id: "maestro-peer-1".to_string(),
        name: "Maestro Peer".to_string(),
        description: "Peer".to_string(),
        agent_type: "maestro".to_string(),
        owner_id: None,
        traceparent: None,
        tracestate: Some("evalops=stale-process-state".to_string()),
        public_endpoint_url: "https://maestro.example/a2a".to_string(),
        internal_endpoint_url: Some("http://maestro.mesh/a2a".to_string()),
        agent_card_url: None,
        heartbeat_interval_ms: 60_000,
        timeout_ms: 2_500,
        current_objective_ids: vec!["obj_1".to_string()],
        max_concurrent_objectives: "4".to_string(),
        surface: "a2a".to_string(),
        surface_type: "SURFACE_MAESTRO".to_string(),
        remote_runner_session_id: None,
    };

    register_or_update_a2a_platform_agent(&registration, &config)
        .expect("registration should post");

    let request = server.join().expect("test server should finish");
    assert!(!request.headers.contains_key("traceparent"));
    assert!(!request.headers.contains_key("tracestate"));
    let attributes = request.body["a2a"]["attributes"]
        .as_object()
        .expect("attributes");
    assert!(!attributes.contains_key("traceparent"));
    assert!(!attributes.contains_key("tracestate"));

    restore_env(snapshot);
}

#[test]
pub(crate) fn a2a_platform_base_url_strips_any_agent_service_method_suffix() {
    assert_eq!(
        normalize_platform_base_url("https://platform.test/agents.v1.AgentService/Register"),
        "https://platform.test"
    );
    assert_eq!(
        normalize_platform_base_url("https://platform.test/agents.v1.AgentService/Delegate"),
        "https://platform.test"
    );
    assert_eq!(
        normalize_platform_base_url("https://platform.test/prefix/agents.v1.AgentService/List"),
        "https://platform.test/prefix"
    );
}

#[test]
pub(crate) fn every_advertised_a2a_subagent_lane_builds_an_honest_execution_policy() {
    let root = TestDir::new("advertised-subagent-policies");
    fs::create_dir_all(root.path().join("packages/runtime-gateway-rs/src/a2a"))
        .expect("scope should be created");
    let advertised = crate::a2a_skill_catalog::a2a_subagent_skills("urn:test:operating-plane");
    let skill_ids = advertised
        .iter()
        .filter_map(|skill| skill["id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        skill_ids,
        vec![
            "maestro.subagent.code-writer",
            "maestro.subagent.code-review"
        ]
    );

    for (skill_id, request) in [
        ("maestro.subagent.code-writer", valid_code_writer_capsule()),
        ("maestro.subagent.code-review", valid_code_review_capsule()),
    ] {
        let capsule = crate::a2a::validate_subagent_capsule(&request, skill_id)
            .expect("advertised capsule should validate");
        let policy = crate::a2a::build_a2a_subagent_execution_policy(
            &capsule,
            root.path(),
            Duration::from_secs(300),
            chrono::Utc::now(),
        )
        .expect("advertised capsule should build an execution policy");
        if skill_id == "maestro.subagent.code-review" {
            assert!(!policy.allowed_tools.contains("write"));
            assert!(!policy.allowed_tools.contains("edit"));
        }
    }
}

#[test]
pub(crate) fn a2a_agent_card_advertises_http_json_interface() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_a2a_url = env::var_os("MAESTRO_A2A_PUBLIC_URL");
    let previous_a2a_host = env::var_os("MAESTRO_A2A_PUBLIC_HOST");
    let previous_control_url = env::var_os("MAESTRO_CONTROL_PUBLIC_URL");
    let previous_control_host = env::var_os("MAESTRO_CONTROL_PUBLIC_HOST");
    env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    let head = parse_request_head(
            b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: attacker.test\r\nx-forwarded-proto: https\r\n\r\n",
        )
        .expect("request should parse");
    let card = a2a_agent_card(&head, &auth_test_config());

    assert_eq!(card["protocolVersion"], A2A_PROTOCOL_VERSION);
    assert_eq!(card["url"], "http://127.0.0.1:0");
    assert_eq!(card["supportedInterfaces"][0]["url"], "http://127.0.0.1:0");
    assert_eq!(
        card["supportedInterfaces"][0]["protocolBinding"],
        "HTTP+JSON"
    );
    assert_eq!(card["capabilities"]["streaming"], true);
    assert_eq!(card["capabilities"]["pushNotifications"], true);
    assert_eq!(card["capabilities"]["extendedAgentCard"], true);
    assert_eq!(
        card["capabilities"]["extensions"][0]["uri"],
        EVALOPS_A2A_EXTENSION_URI
    );
    assert_eq!(
        card["securitySchemes"]["maestroApiKey"]["name"],
        "x-maestro-api-key"
    );
    assert_eq!(card["skills"][0]["id"], "maestro-tui-turn");
    let skill_ids = card["skills"]
        .as_array()
        .expect("skills should be an array")
        .iter()
        .filter_map(|skill| skill.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(skill_ids.contains(&"maestro.subagent.code-review"));
    assert!(skill_ids.contains(&"maestro.subagent.code-writer"));
    assert!(!skill_ids.contains(&"maestro.subagent.test-runner"));
    assert!(!skill_ids.contains(&"maestro.subagent.repo-explorer"));
    assert!(!skill_ids.contains(&"maestro.subagent.browser-qa"));
    assert!(!skill_ids.contains(&"maestro.subagent.release-shepherd"));
    let code_review_skill = card["skills"]
        .as_array()
        .expect("skills should be an array")
        .iter()
        .find(|skill| skill["id"] == "maestro.subagent.code-review")
        .expect("code review subagent skill should be advertised");
    assert_eq!(
        code_review_skill["metadata"]["requestMetadataPath"],
        "evalops.subagentRequest"
    );
    assert_eq!(
        code_review_skill["attributes"]["subagentLaneId"],
        "code-review"
    );
    assert_eq!(
        code_review_skill["attributes"]["requestMetadataPath"],
        "evalops.subagentRequest"
    );
    assert_eq!(
        code_review_skill["requiredContextGrants"],
        serde_json::json!(["repo:read"])
    );
    assert_eq!(
        code_review_skill["approvalPolicyRef"],
        "maestro.subagent.code-review.target-policy"
    );
    assert_eq!(code_review_skill["maxAutonomy"], "bounded");
    assert_eq!(
        code_review_skill["requiredArtifactKinds"],
        serde_json::json!(["review.summary"])
    );
    assert_eq!(
        code_review_skill["allowedTaskClasses"],
        serde_json::json!(["code.review", "risk.analysis"])
    );
    assert_eq!(
        code_review_skill["deniedTaskClasses"],
        serde_json::json!([
            "credential.materialization",
            "secret.exfiltration",
            "unbounded.repository.write"
        ])
    );
    let code_writer_skill = card["skills"]
        .as_array()
        .expect("skills should be an array")
        .iter()
        .find(|skill| skill["id"] == "maestro.subagent.code-writer")
        .expect("code writer subagent skill should be advertised");
    assert_eq!(
        code_writer_skill["metadata"]["taskCapsuleVersion"],
        "evalops.maestro.task-capsule.v1"
    );
    assert_eq!(
        code_writer_skill["metadata"]["taskCapsuleMetadataPath"],
        "evalops.subagentRequest.capsule"
    );
    assert_eq!(
        code_writer_skill["metadata"]["reviewRequirement"],
        "independent-code-review-for-material-lanes"
    );
    assert_eq!(
        code_writer_skill["metadata"]["completionPolicy"],
        "server-generated-from-observed-outcome-and-artifacts"
    );
    if let Some(previous_a2a_url) = previous_a2a_url {
        env::set_var("MAESTRO_A2A_PUBLIC_URL", previous_a2a_url);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    }
    if let Some(previous_a2a_host) = previous_a2a_host {
        env::set_var("MAESTRO_A2A_PUBLIC_HOST", previous_a2a_host);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    }
    if let Some(previous_control_url) = previous_control_url {
        env::set_var("MAESTRO_CONTROL_PUBLIC_URL", previous_control_url);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    }
    if let Some(previous_control_host) = previous_control_host {
        env::set_var("MAESTRO_CONTROL_PUBLIC_HOST", previous_control_host);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    }
}

#[test]
pub(crate) fn a2a_agent_card_uses_configured_public_host_for_wildcard_binds() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_a2a_host = env::var_os("MAESTRO_A2A_PUBLIC_HOST");
    let previous_a2a_url = env::var_os("MAESTRO_A2A_PUBLIC_URL");
    let previous_control_url = env::var_os("MAESTRO_CONTROL_PUBLIC_URL");
    env::set_var("MAESTRO_A2A_PUBLIC_HOST", "mini.example.test");
    env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    let mut config = auth_test_config();
    config.listen_host = "0.0.0.0".to_string();
    config.listen_port = 18787;
    let head = parse_request_head(
        b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: attacker.test\r\n\r\n",
    )
    .expect("request should parse");
    let card = a2a_agent_card(&head, &config);

    assert_eq!(card["url"], "http://mini.example.test:18787");
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        "http://mini.example.test:18787"
    );

    if let Some(previous_a2a_host) = previous_a2a_host {
        env::set_var("MAESTRO_A2A_PUBLIC_HOST", previous_a2a_host);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    }
    if let Some(previous_a2a_url) = previous_a2a_url {
        env::set_var("MAESTRO_A2A_PUBLIC_URL", previous_a2a_url);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    }
    if let Some(previous_control_url) = previous_control_url {
        env::set_var("MAESTRO_CONTROL_PUBLIC_URL", previous_control_url);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    }
}

#[test]
pub(crate) fn a2a_agent_card_formats_ipv6_listen_hosts() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_hostname = env::var_os("HOSTNAME");
    let previous_computername = env::var_os("COMPUTERNAME");
    let previous_a2a_url = env::var_os("MAESTRO_A2A_PUBLIC_URL");
    let previous_a2a_host = env::var_os("MAESTRO_A2A_PUBLIC_HOST");
    let previous_control_url = env::var_os("MAESTRO_CONTROL_PUBLIC_URL");
    let previous_control_host = env::var_os("MAESTRO_CONTROL_PUBLIC_HOST");
    env::remove_var("HOSTNAME");
    env::remove_var("COMPUTERNAME");
    env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    let mut config = auth_test_config();
    config.listen_host = "::1".to_string();
    config.listen_port = 18787;
    let head = parse_request_head(
        b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: attacker.test\r\n\r\n",
    )
    .expect("request should parse");
    let card = a2a_agent_card(&head, &config);

    assert_eq!(card["url"], "http://[::1]:18787");
    assert_eq!(card["supportedInterfaces"][0]["url"], "http://[::1]:18787");

    if let Some(previous_hostname) = previous_hostname {
        env::set_var("HOSTNAME", previous_hostname);
    } else {
        env::remove_var("HOSTNAME");
    }
    if let Some(previous_computername) = previous_computername {
        env::set_var("COMPUTERNAME", previous_computername);
    } else {
        env::remove_var("COMPUTERNAME");
    }
    if let Some(previous_a2a_url) = previous_a2a_url {
        env::set_var("MAESTRO_A2A_PUBLIC_URL", previous_a2a_url);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    }
    if let Some(previous_a2a_host) = previous_a2a_host {
        env::set_var("MAESTRO_A2A_PUBLIC_HOST", previous_a2a_host);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    }
    if let Some(previous_control_url) = previous_control_url {
        env::set_var("MAESTRO_CONTROL_PUBLIC_URL", previous_control_url);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    }
    if let Some(previous_control_host) = previous_control_host {
        env::set_var("MAESTRO_CONTROL_PUBLIC_HOST", previous_control_host);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    }
}

#[test]
pub(crate) fn a2a_agent_card_formats_configured_ipv6_public_host() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_a2a_host = env::var_os("MAESTRO_A2A_PUBLIC_HOST");
    let previous_a2a_url = env::var_os("MAESTRO_A2A_PUBLIC_URL");
    let previous_control_url = env::var_os("MAESTRO_CONTROL_PUBLIC_URL");
    let previous_control_host = env::var_os("MAESTRO_CONTROL_PUBLIC_HOST");
    env::set_var("MAESTRO_A2A_PUBLIC_HOST", "::1");
    env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    let mut config = auth_test_config();
    config.listen_host = "0.0.0.0".to_string();
    config.listen_port = 18787;
    let head = parse_request_head(
        b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: attacker.test\r\n\r\n",
    )
    .expect("request should parse");
    let card = a2a_agent_card(&head, &config);

    assert_eq!(card["url"], "http://[::1]:18787");
    assert_eq!(card["supportedInterfaces"][0]["url"], "http://[::1]:18787");

    if let Some(previous_a2a_host) = previous_a2a_host {
        env::set_var("MAESTRO_A2A_PUBLIC_HOST", previous_a2a_host);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    }
    if let Some(previous_a2a_url) = previous_a2a_url {
        env::set_var("MAESTRO_A2A_PUBLIC_URL", previous_a2a_url);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    }
    if let Some(previous_control_url) = previous_control_url {
        env::set_var("MAESTRO_CONTROL_PUBLIC_URL", previous_control_url);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    }
    if let Some(previous_control_host) = previous_control_host {
        env::set_var("MAESTRO_CONTROL_PUBLIC_HOST", previous_control_host);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    }
}

#[test]
pub(crate) fn a2a_agent_card_percent_encodes_scoped_ipv6_public_host() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_a2a_host = env::var_os("MAESTRO_A2A_PUBLIC_HOST");
    let previous_a2a_url = env::var_os("MAESTRO_A2A_PUBLIC_URL");
    let previous_control_url = env::var_os("MAESTRO_CONTROL_PUBLIC_URL");
    let previous_control_host = env::var_os("MAESTRO_CONTROL_PUBLIC_HOST");
    env::set_var("MAESTRO_A2A_PUBLIC_HOST", "fe80::1%en0");
    env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    let mut config = auth_test_config();
    config.listen_host = "0.0.0.0".to_string();
    config.listen_port = 18787;
    let head = parse_request_head(
        b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: attacker.test\r\n\r\n",
    )
    .expect("request should parse");
    let card = a2a_agent_card(&head, &config);

    assert_eq!(card["url"], "http://[fe80::1%25en0]:18787");
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        "http://[fe80::1%25en0]:18787"
    );

    if let Some(previous_a2a_host) = previous_a2a_host {
        env::set_var("MAESTRO_A2A_PUBLIC_HOST", previous_a2a_host);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_HOST");
    }
    if let Some(previous_a2a_url) = previous_a2a_url {
        env::set_var("MAESTRO_A2A_PUBLIC_URL", previous_a2a_url);
    } else {
        env::remove_var("MAESTRO_A2A_PUBLIC_URL");
    }
    if let Some(previous_control_url) = previous_control_url {
        env::set_var("MAESTRO_CONTROL_PUBLIC_URL", previous_control_url);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_URL");
    }
    if let Some(previous_control_host) = previous_control_host {
        env::set_var("MAESTRO_CONTROL_PUBLIC_HOST", previous_control_host);
    } else {
        env::remove_var("MAESTRO_CONTROL_PUBLIC_HOST");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_completion_attaches_subagent_work_graph_metadata() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "review complete");
    let state = test_app_state_with_sessions(HashMap::new());
    let user_message = a2a_user_message_value(
        &A2AMessageBody {
            message_id: Some("msg-1".to_string()),
            context_id: Some("ctx-1".to_string()),
            task_id: None,
            role: Some("ROLE_USER".to_string()),
            parts: vec![A2APartBody {
                text: Some("review this".to_string()),
                url: None,
                data: None,
                metadata: None,
                filename: None,
                media_type: Some("text/plain".to_string()),
            }],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        },
        "ctx-1",
    );
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let mut metadata = Map::new();
    metadata.insert(
        A2A_SUBAGENT_REQUEST_METADATA_PATH.to_string(),
        serde_json::json!({
            "skillId": "maestro.subagent.code-review",
            "role": "reviewer",
            "taskId": "alpha-review",
            "swarmId": "swarm-1"
        }),
    );

    let task = complete_a2a_task(
        &state,
        "review this".to_string(),
        "maestro-task-1".to_string(),
        "ctx-1".to_string(),
        vec![user_message],
        Value::Object(metadata),
        cancel_rx,
    )
    .await;

    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(
        task["metadata"]["workGraph"]["schemaVersion"],
        CODEX_SUBAGENT_WORK_GRAPH_SCHEMA
    );
    assert_eq!(task["metadata"]["workGraph"]["childRunCount"], 1);
    assert_eq!(
        task["metadata"]["workGraph"]["toolExecutionIds"][0],
        "a2a-subagent-dispatch:maestro-task-1"
    );
    assert_eq!(
        task["metadata"]["workGraph"]["codexSubagents"]["edges"][0]["role"],
        "reviewer"
    );
    assert_eq!(
        task["metadata"]["workGraph"]["correlationPath"],
        "maestro-swarm/swarm-1/alpha-review/a2a/maestro-task-1"
    );
    assert_eq!(
        task["metadata"]["workGraph"]["codexSubagents"]["schemaVersion"],
        CODEX_SUBAGENT_WORK_GRAPH_SCHEMA
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_subagent_completion_requires_independent_review_disposition() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "implementation complete");
    let state = test_app_state_with_sessions(HashMap::new());
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let metadata = serde_json::json!({
        "evalops.subagentRequest": valid_code_writer_capsule(),
        "evalops.subagentCompletion": {
            "verified": true,
            "verificationDisposition": "verified"
        },
        "workGraph": "caller-owned"
    });

    let task = complete_a2a_task(
        &state,
        "implement this".to_string(),
        "maestro-task-code-writer".to_string(),
        "ctx-1".to_string(),
        Vec::new(),
        metadata,
        cancel_rx,
    )
    .await;

    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["verificationDisposition"],
        "pending_independent_review"
    );
    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["serverGenerated"],
        true
    );
    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["observedOutcome"],
        "completed"
    );
    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["missingArtifactKinds"],
        serde_json::json!(["patch.summary", "test.report"])
    );
    assert!(
        task["metadata"]["evalops.subagentCompletion"]["verified"].is_null(),
        "caller-controlled verified field must not survive"
    );
    assert_eq!(task["metadata"]["workGraph"]["status"], "awaiting_review");
    assert_eq!(task["metadata"]["workGraph"]["state"], "awaiting_review");
    assert_eq!(
        task["metadata"]["workGraph"]["codexSubagents"]["edges"][0]["workItemId"],
        "task-1"
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_non_material_subagent_completion_records_server_disposition() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "review complete");
    let state = test_app_state_with_sessions(HashMap::new());
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let metadata = serde_json::json!({
        "evalops.subagentRequest": valid_code_review_capsule()
    });

    let task = complete_a2a_task(
        &state,
        "review this".to_string(),
        "maestro-task-code-review".to_string(),
        "ctx-1".to_string(),
        Vec::new(),
        metadata,
        cancel_rx,
    )
    .await;

    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["verificationDisposition"],
        "completed"
    );
    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["serverGenerated"],
        true
    );
    assert_eq!(
        task["metadata"]["evalops.subagentCompletion"]["missingArtifactKinds"],
        serde_json::json!([])
    );
    assert_eq!(task["metadata"]["workGraph"]["status"], "completed");
    assert!(
        task["artifacts"]
            .as_array()
            .expect("artifacts should be an array")
            .iter()
            .any(|artifact| artifact["artifactKind"] == "review.summary")
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}
