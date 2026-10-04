use super::*;

#[test]
fn watch_owner_future_sizes_stay_bounded() {
    // Infer future types without invoking their constructors: constructing a
    // large diagnostic future would itself consume the test thread's stack.
    fn future_size<A, F>(_: impl FnOnce(A) -> F) -> usize {
        std::mem::size_of::<F>()
    }
    let state = crate::tests::test_app_state_with_sessions(HashMap::new());
    let key: (String, String) = (
        "s".into(),
        "https://github.com/dx-corp/mono/pull/123".into(),
    );
    let reference = PullRequestRef::parse(&key.1).unwrap();
    let reader = future_size(|()| read_snapshot(&state.config.cwd, &reference));
    let poll = future_size(|()| poll_watch(&state, &key));
    let wake = future_size(|()| wake_native(&state, "s", AuthContext::default(), String::new()));
    let chat = future_size(|stream| {
        crate::chat::run_authorized_chat(
            stream,
            ChatRequest {
                interaction_mode: crate::chat::InteractionMode::Implement,
                model: None,
                thinking_level: None,
                session_id: Some("s".into()),
                tools: Vec::new(),
                messages: Vec::new(),
            },
            AuthContext::default(),
            state.clone(),
            true,
        )
    });
    let connection = future_size(|stream| crate::handle_connection(stream, state.clone()));
    let listener = future_size(|(listener, config)| crate::serve_listener(listener, config));
    eprintln!(
        "gateway future sizes: reader={reader}, poll={poll}, wake={wake}, chat={chat}, connection={connection}, listener={listener}"
    );
    assert!(
        reader < 1024,
        "watch reader embeds its buffer state: {reader}"
    );
    assert!(
        wake < 32 * 1024,
        "watch wake embeds a large child future: {wake}"
    );
}

#[test]
fn watch_wake_waits_for_its_session_without_blocking_user_turns() {
    let runtime = Arc::new(WatchRuntime::default());
    let first = runtime.enter(Some("a")).unwrap();
    let second = runtime.enter(Some("a")).unwrap();
    assert!(runtime.claim_idle("a").is_none());
    let other = runtime.claim_idle("b").unwrap();
    drop(first);
    assert!(runtime.claim_idle("a").is_none());
    drop(second);
    let wake = runtime.claim_idle("a").unwrap();
    assert!(runtime.claim_idle("a").is_none());
    drop(wake);
    assert!(runtime.claim_idle("a").is_some());
    drop(other);
}

#[test]
fn same_id_with_a_new_generation_is_a_different_watch_owner() {
    let session: SessionRecord = serde_json::from_value(serde_json::json!({
        "id":"s", "owner":"u", "organizationId":"o", "workspaceId":"w",
        "title":"t", "createdAt":"first", "updatedAt":"now", "messageCount":0,
    }))
    .unwrap();
    let original = Binding::from_session(&session);
    let mut reused = session.clone();
    reused.created_at = "second".into();
    assert_ne!(original, Binding::from_session(&reused));
    reused = session.clone();
    reused.workspace_id = Some("other".into());
    assert_ne!(original, Binding::from_session(&reused));
    reused = session;
    reused.owner = Some("another-user".into());
    assert_ne!(original, Binding::from_session(&reused));
}

#[test]
fn client_cannot_shadow_gateway_watch_tools() {
    for name in [START, STOP, LIST] {
        let chat = ChatRequest {
            interaction_mode: crate::chat::InteractionMode::Implement,
            model: None,
            thinking_level: None,
            session_id: Some("s".into()),
            messages: Vec::new(),
            tools: vec![crate::chat::ClientToolDefinition {
                name: name.to_uppercase(),
                description: "shadow".into(),
                parameters: Value::Null,
            }],
        };
        assert!(crate::chat::validate_client_tool_names(&chat).is_err());
    }
}

#[test]
fn watch_reader_limits_count_live_reads_until_guard_drop() {
    let runtime = Arc::new(WatchRuntime::default());
    let first: Vec<_> = (0..4).map(|_| runtime.claim_reader("a").unwrap()).collect();
    assert!(runtime.claim_reader("a").is_none());
    let second: Vec<_> = (0..4).map(|_| runtime.claim_reader("b").unwrap()).collect();
    assert!(runtime.claim_reader("c").is_none());
    drop(first);
    assert!(runtime.claim_reader("c").is_some());
    drop(second);
}

#[test]
fn watch_sse_reader_preserves_fragmented_errors_and_requires_completion() {
    let mut outcome = SseOutcome::default();
    outcome.feed(b"data: {\"type\":\"message_").unwrap();
    outcome
        .feed(b"end\"}\n\ndata: {\"type\":\"done\"}\n\n")
        .unwrap();
    assert!(outcome.result().is_ok());
    let mut outcome = SseOutcome::default();
    outcome.feed(b"data: {\"type\":\"error\",\"message\":\"provider failed\"}\n\ndata: {\"type\":\"done\"}\n\n").unwrap();
    assert_eq!(outcome.result().unwrap_err(), "provider failed");
    let mut outcome = SseOutcome::default();
    outcome.feed(b"data: {\"type\":\"done\"}\n\n").unwrap();
    assert!(outcome.result().is_err());
    assert!(
        SseOutcome::default()
            .feed(&vec![b'x'; 1024 * 1024 + 1])
            .is_err()
    );
}

fn owned_session() -> SessionRecord {
    serde_json::from_value(serde_json::json!({"id":"s","owner":"u","organizationId":"o","workspaceId":"w","title":"t","createdAt":"generation-one","updatedAt":"now","messageCount":0})).unwrap()
}

fn watch(owner: Binding, generation: u64) -> Watch {
    Watch {
        generation,
        binding: owner,
        auth: AuthContext::default(),
        reference: PullRequestRef::parse("https://github.com/dx-corp/mono/pull/123").unwrap(),
        baseline: None,
        expires: Instant::now() + LEASE,
        pending: vec!["material change".into()],
        stopped: false,
        error: None,
        in_flight: false,
        wake_accepted: true,
    }
}

#[tokio::test]
async fn cancelled_watch_cannot_persist_startup_error_into_rearmed_session() {
    let session = owned_session();
    let owner = Binding::from_session(&session);
    let state = crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session)]));
    let key = (
        "s".into(),
        "https://github.com/dx-corp/mono/pull/123".into(),
    );
    state
        .pull_request_watches
        .watches
        .lock()
        .await
        .insert(key.clone(), watch(owner.clone(), 2));
    WATCH_ADMISSION
        .scope(
            AdmissionTicket {
                key,
                generation: 1,
                binding: owner,
            },
            async {
                crate::chat::record_chat_error(&state, Some("s"), "stale provider failure".into())
                    .await;
            },
        )
        .await;
    assert!(
        state.sessions.lock().await.sessions["s"]
            .last_turn_error
            .is_none()
    );
    crate::chat::record_chat_error(&state, Some("s"), "Run `maestro login`".into()).await;
    assert_eq!(
        state.sessions.lock().await.sessions["s"]
            .last_turn_error
            .as_deref(),
        Some("Run `maestro login`")
    );
}

#[tokio::test]
async fn watch_admission_rejects_reused_sessions_stale_generations_and_expired_leases() {
    let session = owned_session();
    let owner = Binding::from_session(&session);
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session.clone())]));
    let key = (
        "s".into(),
        "https://github.com/dx-corp/mono/pull/123".into(),
    );
    state
        .pull_request_watches
        .watches
        .lock()
        .await
        .insert(key.clone(), watch(owner.clone(), 1));
    let ticket = AdmissionTicket {
        key: key.clone(),
        generation: 1,
        binding: owner.clone(),
    };
    WATCH_ADMISSION
        .scope(ticket, async {
            assert!(validate_append(&state, Some(&session), "s").await.is_ok());
            assert!(validate_append(&state, None, "s").await.is_err());
            let mut reused = session.clone();
            reused.created_at = "generation-two".into();
            assert!(validate_append(&state, Some(&reused), "s").await.is_err());
            state
                .pull_request_watches
                .watches
                .lock()
                .await
                .get_mut(&key)
                .unwrap()
                .generation = 2;
            assert!(validate_append(&state, Some(&session), "s").await.is_err());
            state
                .pull_request_watches
                .watches
                .lock()
                .await
                .get_mut(&key)
                .unwrap()
                .generation = 1;
            state
                .pull_request_watches
                .watches
                .lock()
                .await
                .get_mut(&key)
                .unwrap()
                .expires = Instant::now() - Duration::from_secs(1);
            assert!(validate_append(&state, Some(&session), "s").await.is_err());
        })
        .await;
    // Ordinary authorized chat retains its existing admission path.
    assert!(validate_append(&state, None, "unwatched").await.is_ok());
}

#[tokio::test]
async fn stopped_generation_cannot_remove_rearmed_watch() {
    let session = owned_session();
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session.clone())]));
    let key = (
        "s".into(),
        "https://github.com/dx-corp/mono/pull/123".into(),
    );
    state
        .pull_request_watches
        .watches
        .lock()
        .await
        .insert(key.clone(), watch(Binding::from_session(&session), 2));
    remove_generation(&state, &key, 1).await;
    assert_eq!(
        state.pull_request_watches.watches.lock().await[&key].generation,
        2
    );
    remove_generation(&state, &key, 2).await;
    assert!(
        !state
            .pull_request_watches
            .watches
            .lock()
            .await
            .contains_key(&key)
    );
}

#[tokio::test]
async fn watch_failure_persists_only_to_exact_authorized_session_generation() {
    let directory = tempfile::tempdir().unwrap();
    let session = owned_session();
    let owner = Binding::from_session(&session);
    let mut state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session)]));
    let mut config = (*state.config).clone();
    config.session_store_path = directory.path().join("sessions.json");
    state.config = Arc::new(config);
    let auth = AuthContext {
        subject: Some("u".into()),
        organization_id: Some("o".into()),
        workspace_id: Some("w".into()),
        ..AuthContext::default()
    };
    persist_wake_failure(
        &state,
        &owner,
        &auth,
        "https://github.com/dx-corp/mono/pull/123",
        None,
        "provider failed",
    )
    .await
    .unwrap();
    let persisted: Value = serde_json::from_slice(
        &tokio::fs::read(&state.config.session_store_path)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        persisted["sessions"]["s"]["messages"][0]["watchError"],
        true
    );
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("s")
        .unwrap()
        .created_at = "recreated".into();
    assert!(
        persist_wake_failure(&state, &owner, &auth, "url", None, "failure")
            .await
            .is_err()
    );
    assert_eq!(state.sessions.lock().await.sessions["s"].messages.len(), 1);
}

#[tokio::test]
async fn scoped_stop_cannot_cancel_rearmed_watch_or_recreated_session() {
    let session = owned_session();
    let owner = Binding::from_session(&session);
    let state = crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session)]));
    let url = "https://github.com/dx-corp/mono/pull/123";
    let key = ("s".into(), url.into());
    let auth = AuthContext {
        subject: Some("u".into()),
        organization_id: Some("o".into()),
        workspace_id: Some("w".into()),
        ..AuthContext::default()
    };
    let args = serde_json::json!({"url":url});
    state
        .pull_request_watches
        .watches
        .lock()
        .await
        .insert(key.clone(), watch(owner.clone(), 1));
    let scope = capture_action_scope(&state, &auth, Some("s"), STOP, &args)
        .await
        .unwrap();
    state
        .pull_request_watches
        .watches
        .lock()
        .await
        .insert(key.clone(), watch(owner, 2));
    assert!(
        !handle_scoped_tool(&state, &auth, Some("s"), STOP, &args, scope.clone())
            .await
            .success
    );
    assert_eq!(
        state.pull_request_watches.watches.lock().await[&key].generation,
        2
    );
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("s")
        .unwrap()
        .created_at = "recreated".into();
    assert!(
        !handle_scoped_tool(&state, &auth, Some("s"), STOP, &args, scope)
            .await
            .success
    );
    assert_eq!(
        state.pull_request_watches.watches.lock().await[&key].generation,
        2
    );
}

#[tokio::test]
async fn cancelled_or_rearmed_watch_does_not_persist_old_wake_failure() {
    let session = owned_session();
    let owner = Binding::from_session(&session);
    let state = crate::tests::test_app_state_with_sessions(HashMap::from([("s".into(), session)]));
    let auth = AuthContext {
        subject: Some("u".into()),
        organization_id: Some("o".into()),
        workspace_id: Some("w".into()),
        ..AuthContext::default()
    };
    let url = "https://github.com/dx-corp/mono/pull/123";
    persist_wake_failure(&state, &owner, &auth, url, Some(1), "cancelled")
        .await
        .unwrap();
    assert!(
        state.sessions.lock().await.sessions["s"]
            .messages
            .is_empty()
    );
    state
        .pull_request_watches
        .watches
        .lock()
        .await
        .insert(("s".into(), url.into()), watch(owner.clone(), 2));
    persist_wake_failure(&state, &owner, &auth, url, Some(1), "old failure")
        .await
        .unwrap();
    assert!(
        state.sessions.lock().await.sessions["s"]
            .messages
            .is_empty()
    );
}
