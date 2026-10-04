use super::*;

#[tokio::test]
async fn delete_session_subpath_returns_404_without_removing_session() {
    let root = TestDir::new("session-delete-subpath");
    let session_id = "session-1".to_string();
    let now = now_rfc3339();
    let session = SessionRecord {
        id: session_id.clone(),
        owner: None,
        organization_id: None,
        workspace_id: None,
        title: "Test Session".to_string(),
        created_at: now.clone(),
        updated_at: now,
        message_count: 0,
        favorite: None,
        tags: Vec::new(),
        log_group_id: None,
        background_read_cursor: 0,
        messages: Vec::new(),
        last_turn_error: None,
        native_context: None,
        forked_from: None,
    };
    let state = AppState {
        config: Arc::new(Config {
            listen_host: "127.0.0.1".to_string(),
            listen_port: 8080,
            api_key: Some("api-key".to_string()),
            allowed_hosts: Vec::new(),
            require_key: true,
            require_key_explicitly_disabled: false,
            csrf_token: None,
            require_csrf: false,
            cwd: PathBuf::from("."),
            session_store_path: root.path().join("sessions.json"),
            session_messages_path: root.path().join("session-messages.json"),
            command_prefs_path: root.path().join("command-prefs.json"),
            usage_file_path: root.path().join("usage.json"),
            a2a_tasks_file_path: root.path().join("a2a-tasks.json"),
            automation_file_path: root.path().join("automations.json"),
            llm_gateway_models_url: None,
            llm_gateway_token: None,
            llm_gateway_org_id: None,
            llm_gateway_timeout_ms: 2_500,
        }),
        started_at: Instant::now(),
        selected_model: Arc::new(Mutex::new(emergency_default_model())),
        telemetry_override: Arc::new(Mutex::new(None)),
        training_override: Arc::new(Mutex::new(None)),
        background_settings: Arc::new(Mutex::new(BackgroundSettings::default())),
        framework_preference: Arc::new(Mutex::new(None)),
        command_prefs: Arc::new(Mutex::new(CommandPrefs {
            favorites: Vec::new(),
            recents: Vec::new(),
        })),
        sessions: Arc::new(Mutex::new(SessionStore {
            sessions: HashMap::from([(session_id.clone(), session)]),
            shared_sessions: HashMap::new(),
        })),
        session_store_persist_enabled: true,
        session_persist_lock: Arc::new(Mutex::new(())),
        session_messages: Arc::new(Mutex::new(MessageStore::default())),
        session_messages_persist_enabled: true,
        session_messages_persist_lock: Arc::new(Mutex::new(())),
        usage_persist_lock: Arc::new(Mutex::new(())),
        shared_sessions: Arc::new(Mutex::new(HashMap::new())),
        approval_modes: Arc::new(Mutex::new(HashMap::new())),
        pending_tool_responses: Arc::new(Mutex::new(HashMap::new())),
        native_snapshot_registry: Arc::new(turn_diffs::NativeSnapshotRegistry::default()),
        pull_request_watches: Arc::new(pull_request_watch::WatchRuntime::default()),
        native_turns: Arc::new(native_turns::NativeTurnRuntime::default()),
        pending_tool_response_sessions: Arc::new(Mutex::new(HashMap::new())),
        completed_client_tool_results: Arc::new(Mutex::new(HashMap::new())),
        extended_api: Arc::new(Mutex::new(ExtendedApiState::default())),
        a2a_tasks: Arc::new(Mutex::new(HashMap::new())),
        a2a_task_persist_lock: Arc::new(Mutex::new(())),
        a2a_task_events: broadcast::channel(256).0,
        a2a_task_event_history: Arc::new(Mutex::new(HashMap::new())),
        a2a_cancel_senders: Arc::new(Mutex::new(HashMap::new())),
    };
    let (_client, mut server) = tcp_stream_pair().await;
    let head = RequestHead {
        method: "DELETE".to_string(),
        path: format!("/api/sessions/{session_id}/share"),
        query: HashMap::new(),
        headers: HashMap::from([("x-maestro-api-key".to_string(), "api-key".to_string())]),
    };

    let response = handle_session_endpoint(&mut server, &mut Vec::new(), &head, &state).await;
    let response = String::from_utf8(response).expect("response should be utf-8");

    assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
    assert!(
        state
            .sessions
            .lock()
            .await
            .sessions
            .contains_key(&session_id)
    );
}

#[tokio::test]
async fn invalid_session_store_is_left_untouched_and_future_writes_are_blocked() {
    let root = TestDir::new("invalid-session-store");
    let session_store_path = root.path().join("sessions.json");
    tokio::fs::write(&session_store_path, br#"{"sessions":"invalid"}"#)
        .await
        .expect("fixture should be written");

    let (store, persist_enabled) = load_session_store(&session_store_path).await;
    assert!(store.sessions.is_empty());
    assert!(!persist_enabled);

    let state = AppState {
        config: Arc::new(Config {
            listen_host: "127.0.0.1".to_string(),
            listen_port: 8080,
            api_key: None,
            allowed_hosts: Vec::new(),
            require_key: false,
            require_key_explicitly_disabled: false,
            csrf_token: None,
            require_csrf: false,
            cwd: PathBuf::from("."),
            session_store_path: session_store_path.clone(),
            session_messages_path: session_store_path
                .parent()
                .map(|parent| parent.join("session-messages.json"))
                .unwrap_or_else(|| PathBuf::from("session-messages.json")),
            command_prefs_path: root.path().join("command-prefs.json"),
            usage_file_path: root.path().join("usage.json"),
            a2a_tasks_file_path: root.path().join("a2a-tasks.json"),
            automation_file_path: root.path().join("automations.json"),
            llm_gateway_models_url: None,
            llm_gateway_token: None,
            llm_gateway_org_id: None,
            llm_gateway_timeout_ms: 2_500,
        }),
        started_at: Instant::now(),
        selected_model: Arc::new(Mutex::new(emergency_default_model())),
        telemetry_override: Arc::new(Mutex::new(None)),
        training_override: Arc::new(Mutex::new(None)),
        background_settings: Arc::new(Mutex::new(BackgroundSettings::default())),
        framework_preference: Arc::new(Mutex::new(None)),
        command_prefs: Arc::new(Mutex::new(CommandPrefs {
            favorites: Vec::new(),
            recents: Vec::new(),
        })),
        sessions: Arc::new(Mutex::new(SessionStore {
            sessions: HashMap::from([("session-1".to_string(), test_session_record("session-1"))]),
            shared_sessions: HashMap::new(),
        })),
        session_store_persist_enabled: persist_enabled,
        session_persist_lock: Arc::new(Mutex::new(())),
        session_messages: Arc::new(Mutex::new(MessageStore::default())),
        session_messages_persist_enabled: true,
        session_messages_persist_lock: Arc::new(Mutex::new(())),
        usage_persist_lock: Arc::new(Mutex::new(())),
        shared_sessions: Arc::new(Mutex::new(HashMap::new())),
        approval_modes: Arc::new(Mutex::new(HashMap::new())),
        pending_tool_responses: Arc::new(Mutex::new(HashMap::new())),
        native_snapshot_registry: Arc::new(turn_diffs::NativeSnapshotRegistry::default()),
        pull_request_watches: Arc::new(pull_request_watch::WatchRuntime::default()),
        native_turns: Arc::new(native_turns::NativeTurnRuntime::default()),
        pending_tool_response_sessions: Arc::new(Mutex::new(HashMap::new())),
        completed_client_tool_results: Arc::new(Mutex::new(HashMap::new())),
        extended_api: Arc::new(Mutex::new(ExtendedApiState::default())),
        a2a_tasks: Arc::new(Mutex::new(HashMap::new())),
        a2a_task_persist_lock: Arc::new(Mutex::new(())),
        a2a_task_events: broadcast::channel(256).0,
        a2a_task_event_history: Arc::new(Mutex::new(HashMap::new())),
        a2a_cancel_senders: Arc::new(Mutex::new(HashMap::new())),
    };

    persist_session_store(&state).await;

    let bytes = tokio::fs::read(&session_store_path)
        .await
        .expect("session store should still exist");
    assert_eq!(bytes, br#"{"sessions":"invalid"}"#);
}
