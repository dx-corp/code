use super::*;

#[tokio::test]
async fn authenticated_drain_changes_readiness_and_rejects_new_work() {
    let mut state = test_app_state_with_sessions(HashMap::new());
    let mut config = (*state.config).clone();
    config.api_key = Some("lifecycle-test-key".into());
    config.require_key = true;
    state.config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move { handle_connection(socket, state).await.unwrap() });
        }
    });
    let client = reqwest::Client::new();
    let url = format!("http://{addr}");
    assert_eq!(
        client
            .get(format!("{url}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .post(format!("{url}/.well-known/evalops/remote-runner/drain"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{url}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let drained = client
        .post(format!("{url}/.well-known/evalops/remote-runner/drain"))
        .header("x-maestro-api-key", "lifecycle-test-key")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(drained.status(), 200);
    assert_eq!(
        client
            .get(format!("{url}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{url}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        client
            .post(format!("{url}/api/chat"))
            .header("x-maestro-api-key", "lifecycle-test-key")
            .json(&serde_json::json!({"message":"new work"}))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    server.abort();
}

#[tokio::test]
async fn drain_allows_an_admitted_http_mutation_to_finish() {
    let mut state = test_app_state_with_sessions(HashMap::new());
    let mut config = (*state.config).clone();
    config.api_key = Some("lifecycle-test-key".into());
    config.require_key = true;
    state.config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owner = state.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(crate::lifecycle::serve_connections(
        listener,
        state.clone(),
        async {
            stopped.await.unwrap();
            "test-owner-ended".to_string()
        },
    ));
    let body = r#"{"action":"on"}"#;
    let mut admitted = TcpStream::connect(addr).await.unwrap();
    admitted.write_all(format!("POST /api/telemetry HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: lifecycle-test-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",body.len()).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while owner.lifecycle().active() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}{}", crate::lifecycle::DRAIN_PATH))
        .header("x-maestro-api-key", "lifecycle-test-key")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["active"], 1);
    admitted.write_all(body.as_bytes()).await.unwrap();
    let mut response = String::new();
    admitted.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(*owner.telemetry_override.lock().await, Some(true));
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn accepted_background_owner_remains_counted_after_http_returns() {
    let lifecycle = Arc::new(crate::lifecycle::Lifecycle::default());
    let admission = lifecycle.admit().unwrap();
    let (release, released) = tokio::sync::oneshot::channel();
    let (owner,) = crate::lifecycle::scope(Some(admission), async {
        let child = lifecycle.admit().unwrap();
        (crate::lifecycle::spawn(child, async {
            released.await.unwrap();
        }),)
    })
    .await;
    lifecycle.begin_drain();
    assert_eq!(lifecycle.active(), 1);
    assert!(lifecycle.admit().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(10), lifecycle.idle())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    owner.await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), lifecycle.idle())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_websocket_codex_turn_cannot_enter_after_drain() {
    let state = test_app_state_with_sessions(HashMap::new());
    state.lifecycle().begin_drain();
    let (_client, mut writer) = tcp_stream_pair().await;
    let chat = serde_json::from_value(serde_json::json!({
        "model":"openai-codex/gpt-5.5", "messages":[{"role":"user","content":"must not run"}]
    }))
    .unwrap();
    let result =
        crate::chat::execute_chat_websocket_turn(&mut writer, chat, AuthContext::default(), state)
            .await;
    assert_eq!(result.unwrap_err(), "Runtime is draining");
}

#[tokio::test]
async fn process_shutdown_cancels_a_direct_codex_provider_wait() {
    let _lock = ENV_LOCK.lock().await;
    let root = TestDir::new("drain-codex-provider");
    let cli = root.path().join("cli.js");
    let marker = root.path().join("provider-started");
    fs::write(
        &cli,
        format!(
            r#"
const fs = require('fs');
require('readline').createInterface({{input:process.stdin}}).on('line', line => {{
 if(JSON.parse(line).type === 'prompt') fs.writeFileSync({}, String(process.pid));
}});
"#,
            serde_json::to_string(&marker).unwrap()
        ),
    )
    .unwrap();
    const KEYS: &[&str] = &[
        "MAESTRO_CODEX_APP_SERVER_CLI",
        "MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS",
    ];
    let _environment = SavedEnv::capture(KEYS);
    env::set_var(KEYS[0], &cli);
    env::set_var(KEYS[1], "5000");
    let state = test_app_state_with_sessions(HashMap::new());
    let (_client, mut writer) = tcp_stream_pair().await;
    let started = Instant::now();
    let (output, ()) = tokio::join!(
        run_codex_app_server_headless_cli(
            &mut writer,
            CodexBridgeTransport::Sse,
            &state,
            None,
            root.path(),
            "gpt-5.5",
            "wait",
            &[]
        ),
        async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            state.lifecycle().cancel.cancel();
        }
    );
    assert!(
        output
            .err()
            .expect("process cancellation must interrupt the provider")
            .contains("stopped")
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    #[cfg(unix)]
    {
        let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
        // SAFETY: signal zero only checks the recorded, already waited child PID.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
}

#[tokio::test]
async fn signal_shutdown_closes_late_cancellation_admission_before_mutation() {
    let state = test_app_state_with_sessions(HashMap::new());
    state.a2a_tasks.lock().await.insert(
        "task".to_owned(),
        serde_json::json!({"id":"task", "status":{"state":"TASK_STATE_WORKING"}}),
    );
    let accepted_owner = state.lifecycle().admit().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(crate::lifecycle::serve_connections(
        listener,
        state.clone(),
        async {
            stopped.await.unwrap();
            "test-owner-ended".to_string()
        },
    ));
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !state.lifecycle().is_draining() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // A handler that finishes authorizing after the listener closes must not
    // create a new owner, even though this existing task remains cancellable
    // during the earlier preStop drain phase.
    let authorized = AuthContext {
        unrestricted: true,
        ..AuthContext::default()
    };
    let denied = crate::a2a::cancel_a2a_task(&state, "task", &authorized)
        .await
        .unwrap_err();
    assert!(denied.starts_with(b"HTTP/1.1 503"));
    assert_eq!(state.lifecycle().active(), 1);
    assert_eq!(
        state.a2a_tasks.lock().await["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    drop(accepted_owner);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
