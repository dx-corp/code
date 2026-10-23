use super::*;

#[test]
fn hosted_server_capabilities_fail_closed_without_executor_state() {
    let executor = TransportOnlyHostedRunnerMessageExecutor;

    let capabilities = server_capabilities_for_executor(&executor)
        .expect("transport-only executor preserves baseline server capabilities");
    assert!(!capabilities.server_requests.is_empty());
    assert!(!capabilities.native_tools.is_empty());
    assert!(capabilities.governed_tool_grant_algorithms.is_empty());
    assert!(governed_grant_algorithms_for_executor(&executor).is_empty());

    let workspace = tempdir().expect("transport-only workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshot_capabilities = shared
        .public_snapshot(&state)
        .state
        .server_capabilities
        .expect("snapshot preserves baseline server capabilities");
    drop(state);
    assert!(!snapshot_capabilities.server_requests.is_empty());
    assert!(!snapshot_capabilities.native_tools.is_empty());
    assert!(
        snapshot_capabilities
            .governed_tool_grant_algorithms
            .is_empty()
    );

    let mut formerly_capable =
        native_server_capabilities(&maestro_local_host::agent::tool_host::BuiltInToolHost);
    formerly_capable.governed_tool_grant_algorithms = vec!["ed25519".to_string()];
    let restored_workspace = tempdir().expect("restored child workspace");
    let restored_shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(restored_workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(AgentState {
            server_capabilities: Some(formerly_capable),
            ..AgentState::default()
        })),
        None,
    );
    let restored_snapshot = {
        let state = restored_shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        restored_shared.public_snapshot(&state)
    };
    let restored_snapshot_capabilities = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.restored_snapshot = Some(restored_snapshot);
        shared
            .public_snapshot(&state)
            .state
            .server_capabilities
            .expect("restored snapshot preserves baseline server capabilities")
    };
    assert!(!restored_snapshot_capabilities.server_requests.is_empty());
    assert!(!restored_snapshot_capabilities.native_tools.is_empty());
    assert!(
        restored_snapshot_capabilities
            .governed_tool_grant_algorithms
            .is_empty()
    );
}

#[test]
fn connection_and_subscription_advertise_only_configured_ed25519_verifier() {
    use base64::Engine as _;

    const ENV: &str = "MAESTRO_PLATFORM_TOOL_GRANT_ED25519_PUBLIC_KEYS";
    let _guard = crate::headless_server::GOVERNED_GRANT_ENV_LOCK
        .lock()
        .unwrap();
    let previous = std::env::var_os(ENV);
    let configured = serde_json::json!({
        "active": {
            "algorithm": "ed25519",
            "public_key": "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
            "state": "active"
        }
    })
    .to_string();
    // SAFETY: this test holds the shared lock for this dedicated env key and
    // restores its prior value before releasing the lock.
    unsafe { std::env::set_var(ENV, configured) };

    let workspace = tempdir().expect("workspace");
    let current_agent_state = AgentState {
        server_capabilities: Some(native_server_capabilities(
            &maestro_local_host::agent::tool_host::BuiltInToolHost,
        )),
        ..AgentState::default()
    };
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(current_agent_state)),
        None,
    );
    let connection = handle_connection_create(
        shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_capable".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("capable connection response");
    let ResponseBody::Json {
        body: connection, ..
    } = connection
    else {
        panic!("connection response must be JSON")
    };
    assert_eq!(
        connection["governed_tool_grant_algorithms"],
        serde_json::json!(["ed25519"])
    );
    let capability = connection["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();

    let subscription = handle_subscribe(
        shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_capable".to_string()),
            subscription_id: None,
            connection_capability: Some(capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("capable subscription response");
    let ResponseBody::Json {
        body: subscription, ..
    } = subscription
    else {
        panic!("subscription response must be JSON")
    };
    assert_eq!(
        subscription["governed_tool_grant_algorithms"],
        serde_json::json!(["ed25519"])
    );

    // A newer hosted wrapper must report the exact attached child's verifier
    // capability. An older child deserializes without the new field, so its
    // empty default keeps both hosted response boundaries on v1 even though
    // the wrapper environment itself contains a valid public key.
    let legacy_capabilities: ServerCapabilities = serde_json::from_value(serde_json::json!({
        "server_requests": [],
        "native_tools": []
    }))
    .expect("older child capabilities remain wire compatible");
    let legacy_agent_state = AgentState {
        server_capabilities: Some(legacy_capabilities),
        ..AgentState::default()
    };
    let workspace = tempdir().expect("older child workspace");
    let legacy_child_shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(legacy_agent_state)),
        None,
    );
    let legacy_child_connection = handle_connection_create(
        legacy_child_shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_older_child".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("older child connection response");
    let ResponseBody::Json {
        body: legacy_child_connection,
        ..
    } = legacy_child_connection
    else {
        panic!("older child connection response must be JSON")
    };
    assert_eq!(
        legacy_child_connection["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );
    let legacy_child_capability = legacy_child_connection["connection_capability"]
        .as_str()
        .expect("older child connection capability")
        .to_string();
    let legacy_child_subscription = handle_subscribe(
        legacy_child_shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_older_child".to_string()),
            subscription_id: None,
            connection_capability: Some(legacy_child_capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("older child subscription response");
    let ResponseBody::Json {
        body: legacy_child_subscription,
        ..
    } = legacy_child_subscription
    else {
        panic!("older child subscription response must be JSON")
    };
    assert_eq!(
        legacy_child_subscription["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );

    // A blank signing key id cannot be selected by any valid grant and must
    // never advertise governed v2 on either response boundary.
    unsafe {
        std::env::set_var(
            ENV,
            serde_json::json!({
                "  ": {
                    "algorithm": "ed25519",
                    "public_key": base64::engine::general_purpose::STANDARD.encode([8_u8; 32]),
                    "state": "active"
                }
            })
            .to_string(),
        );
    };
    let workspace = tempdir().expect("legacy workspace");
    let legacy_shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let legacy = handle_connection_create(
        legacy_shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_legacy".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("legacy-compatible response");
    let ResponseBody::Json { body: legacy, .. } = legacy else {
        panic!("legacy response must be JSON")
    };
    assert_eq!(
        legacy["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );
    let legacy_capability = legacy["connection_capability"]
        .as_str()
        .expect("legacy connection capability")
        .to_string();
    let legacy_subscription = handle_subscribe(
        legacy_shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_legacy".to_string()),
            subscription_id: None,
            connection_capability: Some(legacy_capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("legacy-compatible subscription response");
    let ResponseBody::Json {
        body: legacy_subscription,
        ..
    } = legacy_subscription
    else {
        panic!("legacy subscription response must be JSON")
    };
    assert_eq!(
        legacy_subscription["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );

    // Length and canonical point decoding are not enough: a weak small-order
    // point must fail at load time and cannot be advertised by either response.
    let mut weak_public_key = [0_u8; 32];
    weak_public_key[0] = 1;
    unsafe {
        std::env::set_var(
            ENV,
            serde_json::json!({
                "active": {
                    "algorithm": "ed25519",
                    "public_key": base64::engine::general_purpose::STANDARD.encode(weak_public_key),
                    "state": "active"
                }
            })
            .to_string(),
        );
    };
    let workspace = tempdir().expect("weak point workspace");
    let weak_point_shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let weak_point = handle_connection_create(
        weak_point_shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_weak_point".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("weak point remains legacy compatible");
    let ResponseBody::Json {
        body: weak_point, ..
    } = weak_point
    else {
        panic!("weak point response must be JSON")
    };
    assert_eq!(
        weak_point["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );
    let weak_point_capability = weak_point["connection_capability"]
        .as_str()
        .expect("weak point connection capability")
        .to_string();
    let weak_point_subscription = handle_subscribe(
        weak_point_shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_weak_point".to_string()),
            subscription_id: None,
            connection_capability: Some(weak_point_capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("weak point subscription remains legacy compatible");
    let ResponseBody::Json {
        body: weak_point_subscription,
        ..
    } = weak_point_subscription
    else {
        panic!("weak point subscription response must be JSON")
    };
    assert_eq!(
        weak_point_subscription["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );

    unsafe {
        match previous {
            Some(value) => std::env::set_var(ENV, value),
            None => std::env::remove_var(ENV),
        }
    }
}

#[test]
fn supervisor_hello_capabilities_prefer_attached_agent_state() {
    let attached = ServerCapabilities {
        native_tools: vec![NativeToolCapability {
            name: "child-build-tool".to_string(),
            requires_approval: true,
            version: Some("child-build".to_string()),
        }],
        ..ServerCapabilities::default()
    };
    let mut supervisor = AgentSupervisor::new(crate::headless::SupervisorConfig::default());
    supervisor.restore_session_replay(crate::headless::SessionReplay {
        system_prompt_parts: Vec::new(),
        state: AgentState {
            server_capabilities: Some(attached.clone()),
            ..AgentState::default()
        },
        last_init: None,
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    });
    let executor =
        AgentSupervisorHostedRunnerMessageExecutor::new(Arc::new(Mutex::new(supervisor)));

    assert_eq!(executor.negotiated_server_capabilities(), Some(attached));
}
