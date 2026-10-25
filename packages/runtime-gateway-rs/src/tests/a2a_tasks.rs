#[allow(unused_imports)]
use super::*;

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_subagent_request_rejects_invalid_task_capsule_before_claim() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
    for return_immediately in [false, true] {
        let mut subagent_request = valid_code_writer_capsule();
        subagent_request["capsule"]["mutationBoundary"]["paths"] = serde_json::json!(["../deploy"]);
        let body = serde_json::json!({
            "message": {
                "messageId": "msg-1",
                "contextId": "ctx-1",
                "role": "ROLE_USER",
                "parts": [{
                    "text": "implement this",
                    "mediaType": "text/plain"
                }]
            },
            "configuration": {
                "returnImmediately": return_immediately
            },
            "metadata": {
                "evalops.subagentRequest": subagent_request
            }
        })
        .to_string();
        let request = format!(
            "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut initial = request.into_bytes();
        let head = parse_request_head(&initial).expect("request should parse");
        let (_client, mut server) = tcp_stream_pair().await;
        let state = test_app_state_with_sessions(HashMap::new());

        let response =
            response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

        assert_eq!(response["error"]["code"], "INVALID_REQUEST", "{response}");
        assert_eq!(
            state.a2a_tasks.lock().await.len(),
            0,
            "invalid capsule must reject before claim for returnImmediately={return_immediately}"
        );
        assert_eq!(state.a2a_cancel_senders.lock().await.len(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_runs_fake_turn_and_records_task() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "hello from fake native turn");

    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}],"metadata":{"agentId":"ts-tui"}}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nx-evalops-workspace-id: ws-1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let task = &response["task"];

    assert_eq!(task["contextId"], "ctx-1");
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(
        task["artifacts"][0]["parts"][0]["text"],
        "hello from fake native turn"
    );
    assert_eq!(task["metadata"]["workspaceId"], "ws-1");
    assert_eq!(state.a2a_tasks.lock().await.len(), 1);

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_tasks_list_only_returns_tasks_owned_by_subject() {
    let _guard = ENV_LOCK.lock().await;
    let previous_secret = env::var_os("MAESTRO_JWT_SECRET");
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    let state = test_app_state_with_sessions(HashMap::new());
    for (task_id, owner) in [
        ("owned-task", Some("user-123")),
        ("other-task", Some("user-456")),
        ("unowned-task", None),
    ] {
        let mut metadata = Map::new();
        if let Some(owner) = owner {
            metadata.insert("ownerSubject".to_string(), Value::String(owner.to_string()));
        }
        metadata.insert(
            "organizationId".to_string(),
            Value::String("org_test".to_string()),
        );
        metadata.insert(
            "workspaceId".to_string(),
            Value::String("ws_test".to_string()),
        );
        let task = a2a_task_value(
            task_id,
            "ctx-1",
            "TASK_STATE_COMPLETED",
            a2a_agent_message("ctx-1", "done"),
            Vec::new(),
            Vec::new(),
            Value::Object(metadata),
        );
        state
            .a2a_tasks
            .lock()
            .await
            .insert(task_id.to_string(), task);
    }
    let token = identity_jwt_bearer_token(b"shared-secret", "user-123");
    let request =
        format!("GET /tasks HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n");
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let tasks = response["tasks"]
        .as_array()
        .expect("tasks should be an array");

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "owned-task");

    if let Some(previous_secret) = previous_secret {
        env::set_var("MAESTRO_JWT_SECRET", previous_secret);
    } else {
        env::remove_var("MAESTRO_JWT_SECRET");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_tasks_list_supports_spec_filters_pagination_and_payload_trimming() {
    let state = test_app_state_with_sessions(HashMap::new());
    for (task_id, context_id, status, timestamp) in [
        (
            "task-a",
            "ctx-1",
            "TASK_STATE_COMPLETED",
            "2026-05-15T00:03:00+00:00",
        ),
        (
            "task-b",
            "ctx-1",
            "TASK_STATE_COMPLETED",
            "2026-05-15T00:02:00Z",
        ),
        (
            "task-c",
            "ctx-2",
            "TASK_STATE_WORKING",
            "2026-05-15T00:01:00Z",
        ),
        (
            "task-d",
            "ctx-1",
            "TASK_STATE_COMPLETED",
            "2026-05-14T19:04:00-05:00",
        ),
    ] {
        let mut task = a2a_task_value(
            task_id,
            context_id,
            status,
            a2a_agent_message(context_id, "done"),
            vec![
                a2a_agent_message(context_id, "first"),
                a2a_agent_message(context_id, "second"),
            ],
            vec![serde_json::json!({
                "artifactId": format!("{task_id}-artifact"),
                "parts": [{ "text": "artifact", "mediaType": "text/plain" }]
            })],
            serde_json::json!({}),
        );
        task["status"]["timestamp"] = Value::String(timestamp.to_string());
        state
            .a2a_tasks
            .lock()
            .await
            .insert(task_id.to_string(), task);
    }
    let request = "GET /tasks?contextId=ctx-1&status=completed&pageSize=1&historyLength=1 HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let tasks = response["tasks"]
        .as_array()
        .expect("tasks should be an array");

    assert_eq!(response["totalSize"], 3);
    assert_eq!(response["pageSize"], 1);
    let next_page_token = response["nextPageToken"]
        .as_str()
        .expect("next page token should be a string");
    assert!(!next_page_token.is_empty());
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-d");
    assert_eq!(tasks[0]["history"].as_array().unwrap().len(), 1);
    assert!(tasks[0].get("artifacts").is_none());

    let mut newer_task = a2a_task_value(
        "task-e",
        "ctx-1",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-1", "newer"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    newer_task["status"]["timestamp"] = Value::String("2026-05-15T00:05:00Z".to_string());
    state
        .a2a_tasks
        .lock()
        .await
        .insert("task-e".to_string(), newer_task);
    let request = format!(
        "GET /tasks?contextId=ctx-1&status=completed&pageSize=1&historyLength=1&pageToken={next_page_token} HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n"
    );
    let mut initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let tasks = response["tasks"]
        .as_array()
        .expect("tasks should be an array");

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-a");

    let request = "GET /tasks?statusTimestampAfter=2026-05-15T00:03:00Z HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let tasks = response["tasks"]
        .as_array()
        .expect("tasks should be an array");

    assert_eq!(response["totalSize"], 3);
    assert_eq!(tasks[0]["id"], "task-e");
    assert_eq!(tasks[1]["id"], "task-d");
    assert_eq!(tasks[2]["id"], "task-a");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_tasks_list_rejects_invalid_history_length() {
    // Authorization reads the profile environment mutated by other tests.
    let _guard = ENV_LOCK.lock().await;
    let state = test_app_state_with_sessions(HashMap::new());
    let request = "GET /tasks?historyLength=abc HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"]["code"], "INVALID_REQUEST");
    assert_eq!(
        response["error"]["message"],
        "A2A query parameter historyLength must be an integer"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_store_persists_control_plane_tasks_to_ledger_file() {
    let root = TestDir::new("a2a-task-ledger");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "remote-ledger-row",
                    "kind": "message",
                    "peer": "peer-b",
                    "taskId": "remote-task",
                    "text": "remote peer task",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:01:00Z"
                },
                {
                    "id": "maestro-runtime-gateway-other-task",
                    "kind": "message",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "other-control-plane-task",
                    "contextId": "ctx-other",
                    "text": "other process request",
                    "responseText": "other process response",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:03:00Z",
                    "metadata": { "workspaceId": "other-ws" }
                },
                {
                    "id": "raw-legacy-task",
                    "contextId": "ctx-legacy",
                    "status": {
                        "state": "TASK_STATE_COMPLETED",
                        "message": {
                            "messageId": "legacy-msg",
                            "contextId": "ctx-legacy",
                            "role": "ROLE_AGENT",
                            "parts": [{ "text": "legacy complete" }]
                        },
                        "timestamp": "2026-05-15T00:02:00Z"
                    },
                    "history": [
                        {
                            "messageId": "legacy-user",
                            "contextId": "ctx-legacy",
                            "role": "ROLE_USER",
                            "parts": [{ "text": "legacy request" }]
                        }
                    ],
                    "metadata": { "workspaceId": "legacy-ws" }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("existing ledger should be written");
    let task = a2a_task_value(
        "maestro-task-durable",
        "ctx-1",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-1", "complete"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({
            "workspaceId": "ws-1",
            "nonSecret": "keep-visible",
            "nonCredentials": "keep-visible-too",
            "apiToken": "secret-api-token",
            "oauthToken": "secret-oauth-token",
            "bearer": "secret-bearer",
            "tools": [
                {
                    "name": "inspect",
                    "input": {
                        "path": "src/main.rs",
                        "nonSecret": "nested-visible",
                        "nonCredentials": "nested-visible-too",
                        "apiToken": "nested-secret",
                        "webhookSecret": "nested-webhook-secret"
                    }
                }
            ],
            "usage": {
                "totalTokens": 42,
                "totalToken": 43,
                "tokenCount": 44,
                "bearer": "nested-bearer"
            },
            "workGraph": {
                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                "state": "completed",
                "childRunIds": ["a2a-task:maestro-task-durable"],
                "toolExecutionIds": [],
                "waitIds": []
            }
        }),
    );
    let raw_legacy_task = load_a2a_tasks(&state.config.a2a_tasks_file_path)
        .await
        .remove("raw-legacy-task")
        .expect("raw legacy task should hydrate before migration");
    state
        .a2a_tasks
        .lock()
        .await
        .insert("raw-legacy-task".to_string(), raw_legacy_task);

    store_a2a_task_unless_canceled(&state, "maestro-task-durable", task).await;
    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;
    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entries = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array");
    let remote_entry = entries
        .iter()
        .find(|entry| entry["peer"] == "peer-b")
        .expect("remote peer ledger row should be retained");
    let other_control_plane_entry = entries
        .iter()
        .find(|entry| {
            entry["peer"] == A2A_RUNTIME_GATEWAY_LEDGER_PEER
                && entry["taskId"] == "other-control-plane-task"
        })
        .expect("other process control-plane row should be retained");
    let control_plane_entry = entries
        .iter()
        .find(|entry| {
            entry["peer"] == A2A_RUNTIME_GATEWAY_LEDGER_PEER
                && entry["taskId"] == "maestro-task-durable"
        })
        .expect("control-plane ledger row should be written");
    let raw_legacy_entries = entries
        .iter()
        .filter(|entry| entry["id"] == "raw-legacy-task")
        .count();

    assert_eq!(remote_entry["taskId"], "remote-task");
    assert_eq!(
        other_control_plane_entry["metadata"]["workspaceId"],
        "other-ws"
    );
    assert_eq!(control_plane_entry["taskId"], "maestro-task-durable");
    assert_eq!(control_plane_entry["kind"], "delegation");
    assert_eq!(control_plane_entry["state"], "TASK_STATE_COMPLETED");
    assert_eq!(control_plane_entry["metadata"]["workspaceId"], "ws-1");
    assert_eq!(control_plane_entry["metadata"]["nonSecret"], "keep-visible");
    assert_eq!(
        control_plane_entry["metadata"]["nonCredentials"],
        "keep-visible-too"
    );
    assert!(control_plane_entry["metadata"].get("apiToken").is_none());
    assert!(control_plane_entry["metadata"].get("oauthToken").is_none());
    assert!(control_plane_entry["metadata"].get("bearer").is_none());
    assert!(control_plane_entry["metadata"].get("tools").is_none());
    assert_eq!(
        control_plane_entry["workGraph"]["childRunIds"][0],
        "a2a-task:maestro-task-durable"
    );
    assert_eq!(
        control_plane_entry["peerDisplayName"],
        A2A_RUNTIME_GATEWAY_LEDGER_DISPLAY_NAME
    );
    assert_eq!(control_plane_entry["a2aTask"]["id"], "maestro-task-durable");
    assert!(
        control_plane_entry["a2aTask"]["metadata"]
            .get("apiToken")
            .is_none()
    );
    assert!(
        control_plane_entry["a2aTask"]["metadata"]
            .get("oauthToken")
            .is_none()
    );
    assert!(
        control_plane_entry["a2aTask"]["metadata"]
            .get("bearer")
            .is_none()
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["tools"][0]["name"],
        "inspect"
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["tools"][0]["input"]["path"],
        "src/main.rs"
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["tools"][0]["input"]["nonSecret"],
        "nested-visible"
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["tools"][0]["input"]["nonCredentials"],
        "nested-visible-too"
    );
    assert!(
        control_plane_entry["a2aTask"]["metadata"]["tools"][0]["input"]
            .get("apiToken")
            .is_none()
    );
    assert!(
        control_plane_entry["a2aTask"]["metadata"]["tools"][0]["input"]
            .get("webhookSecret")
            .is_none()
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["usage"]["totalTokens"],
        42
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["usage"]["totalToken"],
        43
    );
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["usage"]["tokenCount"],
        44
    );
    assert!(
        control_plane_entry["a2aTask"]["metadata"]["usage"]
            .get("bearer")
            .is_none()
    );
    assert_eq!(
        raw_legacy_entries, 0,
        "raw legacy A2A rows should be migrated away from the shared TS ledger shape"
    );
    assert!(entries.iter().any(|entry| {
        entry["peer"] == A2A_RUNTIME_GATEWAY_LEDGER_PEER
            && entry["taskId"] == "raw-legacy-task"
            && entry["a2aTask"]["id"] == "raw-legacy-task"
    }));
    assert!(!loaded.contains_key("remote-task"));
    assert_eq!(
        loaded["maestro-task-durable"]["metadata"]["workspaceId"],
        "ws-1"
    );
    assert_eq!(
        loaded["maestro-task-durable"]["metadata"]["nonSecret"],
        "keep-visible"
    );
    assert_eq!(
        loaded["maestro-task-durable"]["metadata"]["nonCredentials"],
        "keep-visible-too"
    );
    assert!(
        loaded["maestro-task-durable"]["metadata"]
            .get("oauthToken")
            .is_none()
    );
    assert!(
        loaded["maestro-task-durable"]["metadata"]["tools"][0]["input"]
            .get("webhookSecret")
            .is_none()
    );
    assert_eq!(
        loaded["maestro-task-durable"]["metadata"]["workGraph"]["childRunIds"][0],
        "a2a-task:maestro-task-durable"
    );

    let reloaded_task = loaded["maestro-task-durable"].clone();
    {
        let mut tasks = state.a2a_tasks.lock().await;
        tasks.clear();
        tasks.insert("maestro-task-durable".to_string(), reloaded_task);
    }
    persist_a2a_tasks(&state).await;
    let reloaded_ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("reloaded database should be readable");
    let reloaded_control_plane_entry = reloaded_ledger["tasks"]
        .as_array()
        .expect("reloaded ledger tasks should be an array")
        .iter()
        .find(|entry| {
            entry["peer"] == A2A_RUNTIME_GATEWAY_LEDGER_PEER
                && entry["taskId"] == "maestro-task-durable"
        })
        .expect("reloaded control-plane ledger row should be written");
    assert_eq!(
        reloaded_control_plane_entry["workGraph"]["childRunIds"][0],
        "a2a-task:maestro-task-durable"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_persist_preserves_tui_orb_delegations_and_unknown_fields() {
    let root = TestDir::new("a2a-task-ledger-preserve-orb-projection");
    let ledger_path = root.path().join("tasks.json");
    let seeded_ledger = serde_json::json!({
        "tasks": [{
            "id": "maestro-runtime-gateway-gateway-transition",
            "kind": "delegation",
            "peer": "maestro-runtime-gateway",
            "taskId": "gateway-transition",
            "text": "old gateway text",
            "state": "TASK_STATE_QUEUED",
            "createdAt": "2026-05-15T00:00:00.000Z",
            "updatedAt": "2026-05-15T00:00:00.000Z",
            "futureTaskField": {"revision": 3},
            "a2aTask": {
                "id": "gateway-transition",
                "futureStatusProjection": "must-survive-gateway-write"
            }
        }],
        "orbDelegations": [{
            "maestroDelegationId": "delegation-from-tui",
            "maestroSessionId": "session-from-tui",
            "tenantId": "tenant-from-tui",
            "operationId": "operation-from-tui",
            "idempotencyKey": "idempotency-from-tui",
            "requestDigest": "digest-from-tui",
            "state": "starting",
            "observedRevision": 0,
            "createdAt": "2026-05-15T00:00:00.000Z",
            "updatedAt": "2026-05-15T00:00:00.000Z"
        }],
        "futureProjection": {
            "version": 1,
            "opaque": ["must-survive-gateway-write"]
        }
    });
    tokio::fs::write(
        &ledger_path,
        serde_json::to_vec_pretty(&seeded_ledger).expect("seed ledger should serialize"),
    )
    .await
    .expect("seed ledger should be writable");

    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = ledger_path.clone();
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    state.a2a_tasks.lock().await.insert(
        "gateway-transition".to_string(),
        a2a_task_value(
            "gateway-transition",
            "ctx-preserve-orb",
            "TASK_STATE_WORKING",
            a2a_agent_message("ctx-preserve-orb", "gateway update"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({}),
        ),
    );

    persist_a2a_tasks(&state).await;

    let persisted =
        maestro_a2a_ledger::load(&ledger_path).expect("persisted database should be readable");
    assert_eq!(persisted["orbDelegations"], seeded_ledger["orbDelegations"]);
    assert_eq!(
        persisted["futureProjection"],
        seeded_ledger["futureProjection"]
    );
    assert_eq!(persisted["tasks"][0]["taskId"], "gateway-transition");
    assert_eq!(
        persisted["tasks"][0]["futureTaskField"],
        seeded_ledger["tasks"][0]["futureTaskField"]
    );
    assert_eq!(
        persisted["tasks"][0]["a2aTask"]["futureStatusProjection"],
        "must-survive-gateway-write"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(maestro_a2a_ledger::database_path(&ledger_path))
                .await
                .expect("persisted database metadata should be readable")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_prefers_top_level_work_graph_over_embedded_metadata() {
    let root = TestDir::new("a2a-task-ledger-stale-work-graph");
    let ledger_path = root.path().join("tasks.json");
    tokio::fs::write(
        &ledger_path,
        serde_json::to_vec_pretty(&serde_json::json!({
        "tasks": [
            {
                "id": "maestro-runtime-gateway-stale-work-graph",
                "kind": "delegation",
                "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                "taskId": "stale-work-graph-task",
                "contextId": "ctx-stale",
                "text": "stale request",
                "state": "TASK_STATE_COMPLETED",
                "createdAt": "2026-05-15T00:00:00Z",
                "updatedAt": "2026-05-15T00:02:00Z",
                "metadata": { "workspaceId": "ws-stale" },
                "workGraph": {
                    "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                    "state": "completed",
                    "childRunIds": ["fresh-run"],
                    "toolExecutionIds": [],
                    "waitIds": []
                },
                "a2aTask": {
                    "id": "stale-work-graph-task",
                    "contextId": "ctx-stale",
                    "status": {
                        "state": "TASK_STATE_COMPLETED",
                        "message": {
                            "messageId": "stale-agent-message",
                            "contextId": "ctx-stale",
                            "role": "ROLE_AGENT",
                            "parts": [{ "text": "done" }]
                        },
                        "timestamp": "2026-05-15T00:02:00Z"
                    },
                    "history": [],
                    "metadata": {
                        "workspaceId": "ws-stale",
                        "workGraph": {
                            "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                            "state": "completed",
                            "childRunIds": ["stale-run"],
                            "toolExecutionIds": [],
                            "waitIds": []
                        }
                        }
                    }
                },
                {
                    "id": "maestro-runtime-gateway-empty-work-graph",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-empty-work-graph",
                    "contextId": "ctx-empty-work-graph",
                    "text": "delegate work",
                    "responseText": "done",
                    "state": "SUCCESS",
                    "workGraph": {},
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-empty-work-graph",
                        "contextId": "ctx-empty-work-graph",
                        "status": {
                            "state": "TASK_STATE_COMPLETED",
                            "message": a2a_agent_message("ctx-empty-work-graph", "done")
                        },
                        "metadata": {
                            "workspaceId": "ws-empty",
                            "workGraph": {
                                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                                "state": "completed",
                                "childRunIds": ["embedded-empty-run"],
                                "toolExecutionIds": [],
                                "waitIds": []
                            }
                        }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("existing ledger should be written");

    let loaded = load_a2a_tasks(&ledger_path).await;

    assert_eq!(
        loaded["stale-work-graph-task"]["metadata"]["workGraph"]["childRunIds"][0],
        "fresh-run"
    );
    assert_eq!(
        loaded["task-empty-work-graph"]["metadata"]["workGraph"]["childRunIds"][0],
        "embedded-empty-run"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_keeps_embedded_work_graph_when_top_level_value_is_null() {
    let root = TestDir::new("a2a-task-ledger-null-work-graph");
    let ledger_path = root.path().join("tasks.json");
    tokio::fs::write(
        &ledger_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-null-work-graph",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "null-work-graph-task",
                    "contextId": "ctx-null",
                    "text": "null work graph request",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "metadata": { "workspaceId": "ws-null" },
                    "workGraph": null,
                    "a2aTask": {
                        "id": "null-work-graph-task",
                        "contextId": "ctx-null",
                        "status": {
                            "state": "TASK_STATE_COMPLETED",
                            "message": {
                                "messageId": "null-agent-message",
                                "contextId": "ctx-null",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "done" }]
                            },
                            "timestamp": "2026-05-15T00:02:00Z"
                        },
                        "history": [],
                        "metadata": {
                            "workspaceId": "ws-null",
                            "workGraph": {
                                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                                "state": "completed",
                                "childRunIds": ["embedded-run"],
                                "toolExecutionIds": [],
                                "waitIds": []
                            }
                        }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("existing ledger should be written");

    let loaded = load_a2a_tasks(&ledger_path).await;

    assert_eq!(
        loaded["null-work-graph-task"]["metadata"]["workGraph"]["childRunIds"][0],
        "embedded-run"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_refreshes_history_messages_with_matching_ids() {
    let root = TestDir::new("a2a-task-ledger-stale-history");
    let ledger_path = root.path().join("tasks.json");
    tokio::fs::write(
        &ledger_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-stale-history",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "stale-history-task",
                    "contextId": "ctx-stale-history",
                    "text": "run checks",
                    "state": "TASK_STATE_COMPLETED",
                    "responseText": "done",
                    "transcript": [
                        {
                            "at": "2026-05-15T00:00:00Z",
                            "role": "user",
                            "text": "run checks",
                            "messageId": "user-message"
                        },
                        {
                            "at": "2026-05-15T00:01:00Z",
                            "role": "agent",
                            "text": "done",
                            "state": "TASK_STATE_COMPLETED",
                            "messageId": "agent-message"
                        }
                    ],
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "completedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "stale-history-task",
                        "contextId": "ctx-stale-history",
                        "status": {
                            "state": "TASK_STATE_COMPLETED",
                            "message": {
                                "messageId": "agent-message",
                                "contextId": "ctx-stale-history",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "still working" }]
                            },
                            "timestamp": "2026-05-15T00:02:00Z"
                        },
                        "history": [
                            {
                                "messageId": "user-message",
                                "contextId": "ctx-stale-history",
                                "role": "ROLE_USER",
                                "parts": [{ "text": "run checks" }]
                            },
                            {
                                "messageId": "agent-message",
                                "contextId": "ctx-stale-history",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "still working" }]
                            }
                        ],
                        "metadata": {}
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("existing ledger should be written");

    let loaded = load_a2a_tasks(&ledger_path).await;
    let reloaded_task = loaded["stale-history-task"].clone();
    assert_eq!(
        reloaded_task["status"]["message"]["parts"][0]["text"],
        "done"
    );
    assert_eq!(reloaded_task["history"][1]["parts"][0]["text"], "done");

    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = ledger_path.clone();
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    {
        let mut tasks = state.a2a_tasks.lock().await;
        tasks.insert("stale-history-task".to_string(), reloaded_task);
    }

    persist_a2a_tasks(&state).await;
    let reloaded_ledger =
        maestro_a2a_ledger::load(&ledger_path).expect("reloaded database should be readable");
    let reloaded_entry = reloaded_ledger["tasks"]
        .as_array()
        .expect("reloaded ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "stale-history-task")
        .expect("reloaded control-plane ledger row should be written");
    assert_eq!(reloaded_entry["responseText"], "done");
    assert_eq!(reloaded_entry["transcript"][1]["text"], "done");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_ledger_lock_heartbeat_refreshes_while_owned() {
    let root = TestDir::new("a2a-task-ledger-lock-heartbeat");
    let tasks_path = root.path().join("tasks.json");
    let file_lock = acquire_a2a_task_ledger_file_lock(&tasks_path)
        .await
        .expect("lock should be acquired");
    let heartbeat_path = file_lock.path.join(A2A_LEDGER_LOCK_HEARTBEAT_FILE);
    let first_heartbeat = tokio::fs::read_to_string(&heartbeat_path)
        .await
        .expect("heartbeat should be readable");

    let heartbeat_task =
        spawn_a2a_task_ledger_lock_heartbeat(&file_lock, Duration::from_millis(10));
    let refreshed_heartbeat = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let heartbeat = tokio::fs::read_to_string(&heartbeat_path)
                .await
                .expect("heartbeat should still be readable");
            if heartbeat != first_heartbeat && !heartbeat.is_empty() {
                break heartbeat;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    heartbeat_task.abort();
    let _ = heartbeat_task.await;

    assert_ne!(
        refreshed_heartbeat.expect("owned lock heartbeat should refresh"),
        first_heartbeat
    );

    release_a2a_task_ledger_file_lock(file_lock).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_ledger_lock_owner_fence_blocks_reclaim_probe() {
    use std::os::fd::AsRawFd;

    let root = TestDir::new("a2a-task-ledger-lock-owner-fence");
    let tasks_path = root.path().join("tasks.json");
    let file_lock = acquire_a2a_task_ledger_file_lock(&tasks_path)
        .await
        .expect("lock should be acquired");
    let owner_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file_lock.path.join("owner"))
        .expect("owner metadata should be openable");
    let result = unsafe { libc::flock(owner_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_ne!(result, 0, "a live owner fence must reject reclaim probes");
    release_a2a_task_ledger_file_lock(file_lock).await;
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_persist_aborts_after_lock_ownership_loss() {
    let root = TestDir::new("a2a-task-ledger-lock-ownership-loss");
    let tasks_path = root.path().join("tasks.json");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = tasks_path.clone();
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    state.a2a_tasks.lock().await.insert(
        "ownership-loss-task".to_string(),
        a2a_task_value(
            "ownership-loss-task",
            "ctx-ownership-loss",
            "TASK_STATE_WORKING",
            a2a_agent_message("ctx-ownership-loss", "must not overwrite"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({}),
        ),
    );

    let file_lock = acquire_a2a_task_ledger_file_lock(&tasks_path)
        .await
        .expect("lock should be acquired");
    tokio::fs::write(
        file_lock.path.join("owner"),
        b"reclaimed-by-another-process\n",
    )
    .await
    .expect("test should be able to simulate ownership loss");

    let error = persist_a2a_tasks_locked(&state, &tasks_path, &file_lock)
        .await
        .expect_err("a reclaimed lock must abort before replacing the ledger");
    assert!(error.contains("lost A2A task ledger lock ownership"));
    assert!(
        !tokio::fs::try_exists(maestro_a2a_ledger::database_path(&tasks_path))
            .await
            .expect("database existence should be checkable")
    );
    tokio::fs::remove_dir_all(&file_lock.path)
        .await
        .expect("test lock should be cleaned up");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_ledger_lock_resolves_symlink_aliases() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new("a2a-task-ledger-lock-alias");
    let target = root.path().join("tasks.json");
    let alias = root.path().join("alias.json");
    tokio::fs::write(&target, br#"{"tasks":[]}"#)
        .await
        .expect("target ledger should be writable");
    symlink(&target, &alias).expect("ledger alias should be creatable");

    let file_lock = acquire_a2a_task_ledger_file_lock(&alias)
        .await
        .expect("alias lock should be acquired");
    let canonical_target = dunce::canonicalize(&target).expect("target ledger should canonicalize");
    assert_eq!(
        file_lock.path,
        a2a_task_ledger_lock_path(&canonical_target),
        "symlink aliases must share the canonical Gateway/TUI lock path"
    );
    assert!(file_lock.path.is_dir());
    assert!(file_lock.path.join("owner").is_file());
    assert!(
        file_lock
            .path
            .join(A2A_LEDGER_LOCK_HEARTBEAT_FILE)
            .is_file()
    );
    release_a2a_task_ledger_file_lock(file_lock).await;
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_skips_remote_cli_ledger_entries() {
    let root = TestDir::new("a2a-task-ledger-remote-skip");
    let tasks_path = root.path().join("tasks.json");
    tokio::fs::write(
            &tasks_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "tasks": [
                    {
                        "id": "remote-ledger-row",
                        "kind": "message",
                        "peer": "peer-b",
                        "taskId": "remote-task",
                        "text": "remote peer task",
                        "state": "TASK_STATE_COMPLETED",
                        "createdAt": "2026-05-15T00:00:00Z",
                        "updatedAt": "2026-05-15T00:01:00Z"
                    },
                    {
                        "id": "raw-legacy-task",
                        "contextId": "ctx-legacy",
                        "status": {
                            "state": "TASK_STATE_COMPLETED",
                            "message": {
                                "messageId": "legacy-msg",
                                "contextId": "ctx-legacy",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "legacy complete" }]
                            }
                        },
                        "metadata": { "workspaceId": "legacy-ws" }
                    },
                    {
                        "id": "maestro-runtime-gateway-local-task",
                        "kind": "message",
                        "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                        "taskId": "local-task",
                        "contextId": "ctx-local",
                        "text": "local request",
                        "responseText": "local response",
                        "state": "TASK_STATE_COMPLETED",
                        "transcript": [
                            { "role": "user", "text": "local request", "messageId": "msg-local" },
                            { "role": "agent", "text": "local response", "state": "TASK_STATE_COMPLETED" }
                        ],
                        "createdAt": "2026-05-15T00:00:00Z",
                        "updatedAt": "2026-05-15T00:02:00Z",
                        "metadata": { "workspaceId": "local-ws" }
                    }
                ]
            }))
            .expect("ledger should serialize"),
        )
        .await
        .expect("ledger should be written");

    let loaded = load_a2a_tasks(&tasks_path).await;

    assert!(!loaded.contains_key("remote-task"));
    assert_eq!(
        loaded["raw-legacy-task"]["metadata"]["workspaceId"],
        "legacy-ws"
    );
    assert_eq!(loaded["local-task"]["metadata"]["workspaceId"], "local-ws");
    assert_eq!(
        loaded["local-task"]["status"]["message"]["parts"][0]["text"],
        "local response"
    );
    assert_eq!(loaded["local-task"]["history"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_store_prefers_top_level_ledger_work_graph() {
    let root = TestDir::new("a2a-task-ledger-top-level-work-graph");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-task-work-graph",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-work-graph",
                    "contextId": "ctx-work-graph",
                    "text": "delegate work",
                    "responseText": "done",
                    "state": "TASK_STATE_COMPLETED",
                    "workGraph": {
                        "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                        "state": "completed",
                        "childRunIds": ["new-run"],
                        "toolExecutionIds": [],
                        "waitIds": []
                    },
                    "transcript": [
                        { "role": "user", "text": "delegate work", "messageId": "msg-1" },
                        { "role": "agent", "text": "done", "state": "TASK_STATE_COMPLETED" }
                    ],
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-work-graph",
                        "contextId": "ctx-work-graph",
                        "status": {
                            "state": "TASK_STATE_COMPLETED",
                            "message": {
                                "messageId": "agent-msg-1",
                                "contextId": "ctx-work-graph",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "done" }]
                            }
                        },
                        "metadata": {
                            "workspaceId": "legacy-ws",
                            "workGraph": {
                                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                                "state": "waiting",
                                "childRunIds": ["old-run"],
                                "toolExecutionIds": [],
                                "waitIds": []
                            }
                        }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;
    assert_eq!(
        loaded["task-work-graph"]["metadata"]["workGraph"]["childRunIds"][0],
        "new-run"
    );
    assert_eq!(
        loaded["task-work-graph"]["history"]
            .as_array()
            .expect("embedded task transcript should hydrate history")
            .len(),
        2
    );
    assert_eq!(loaded["task-work-graph"]["history"][0]["role"], "ROLE_USER");
    assert_eq!(
        loaded["task-work-graph"]["history"][1]["role"],
        "ROLE_AGENT"
    );
    assert_eq!(
        loaded["task-work-graph"]["history"][1]["parts"][0]["text"],
        "done"
    );

    state.a2a_tasks.lock().await.extend(loaded);
    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-work-graph")
        .expect("control-plane row should exist");
    assert_eq!(entry["workGraph"]["childRunIds"][0], "new-run");
    assert_eq!(
        entry["transcript"]
            .as_array()
            .expect("transcript should persist")
            .len(),
        2
    );
    assert_eq!(entry["transcript"][0]["text"], "delegate work");
    assert_eq!(entry["transcript"][1]["text"], "done");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_ignores_null_top_level_work_graph() {
    let root = TestDir::new("a2a-task-ledger-null-work-graph");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-null-work-graph",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-null-work-graph",
                    "contextId": "ctx-null-work-graph",
                    "text": "delegate work",
                    "responseText": "done",
                    "state": "TASK_STATE_COMPLETED",
                    "workGraph": null,
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-null-work-graph",
                        "contextId": "ctx-null-work-graph",
                        "status": {
                            "state": "TASK_STATE_COMPLETED",
                            "message": a2a_agent_message("ctx-null-work-graph", "done")
                        },
                        "metadata": {
                            "workspaceId": "ws-null",
                            "workGraph": {
                                "schemaVersion": CODEX_SUBAGENT_WORK_GRAPH_SCHEMA,
                                "state": "completed",
                                "childRunIds": ["embedded-run"],
                                "toolExecutionIds": [],
                                "waitIds": []
                            }
                        }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-null-work-graph"]["metadata"]["workGraph"]["childRunIds"][0],
        "embedded-run"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_preserves_embedded_history_when_transcript_exists() {
    let root = TestDir::new("a2a-task-ledger-embedded-history");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-rich-history",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-rich-history",
                    "contextId": "ctx-rich-history",
                    "text": "inspect attachment",
                    "responseText": "saw attachment",
                    "state": "TASK_STATE_WORKING",
                    "transcript": [
                        { "role": "user", "text": "inspect attachment" },
                        { "role": "user", "text": "inspect attachment", "messageId": "msg-rich" },
                        { "role": "agent", "text": "saw attachment", "messageId": "agent-rich" }
                    ],
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-rich-history",
                        "contextId": "ctx-rich-history",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-rich",
                                "contextId": "ctx-rich-history",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "saw attachment" }]
                            }
                        },
                        "history": [
                            {
                                "messageId": "msg-rich",
                                "contextId": "ctx-rich-history",
                                "role": "ROLE_USER",
                                "parts": [
                                    { "text": "inspect attachment" },
                                    { "data": { "attachmentId": "artifact-1" } }
                                ]
                            },
                            {
                                "messageId": "agent-rich",
                                "contextId": "ctx-rich-history",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "stale attachment summary" }]
                            }
                        ],
                        "metadata": { "workspaceId": "ws-rich" }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-rich-history"]["history"][0]["parts"][1]["data"]["attachmentId"],
        "artifact-1"
    );
    assert_eq!(
        loaded["task-rich-history"]["history"]
            .as_array()
            .expect("history should be an array")
            .len(),
        2
    );
    assert_eq!(
        loaded["task-rich-history"]["history"][1]["parts"][0]["text"],
        "saw attachment"
    );

    state.a2a_tasks.lock().await.extend(loaded);
    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-rich-history")
        .expect("control-plane row should exist");
    assert_eq!(
        entry["a2aTask"]["history"][0]["parts"][1]["data"]["attachmentId"],
        "artifact-1"
    );
    assert_eq!(entry["transcript"][0]["text"], "inspect attachment");
    assert_eq!(entry["transcript"][1]["text"], "saw attachment");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_prefers_top_level_state_over_embedded_status() {
    let root = TestDir::new("a2a-task-ledger-top-level-state");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-stale-status",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-stale-status",
                    "contextId": "ctx-stale-status",
                    "text": "finish work",
                    "responseText": "done",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-stale-status",
                        "contextId": "ctx-stale-status",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-stale-status",
                                "contextId": "ctx-stale-status",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "still working" }]
                            },
                            "timestamp": "2026-05-15T00:01:00Z"
                        },
                        "history": [],
                        "metadata": {
                            "workspaceId": "ws-state",
                            "pushNotificationConfigs": [
                                {
                                    "id": "notify-state",
                                    "taskId": "task-stale-status",
                                    "url": "https://hooks.example/a2a",
                                    "token": "notify-token"
                                }
                            ]
                        }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-stale-status"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        loaded["task-stale-status"]["status"]["timestamp"],
        "2026-05-15T00:02:00Z"
    );
    assert_eq!(
        loaded["task-stale-status"]["status"]["message"]["parts"][0]["text"],
        "done"
    );

    state.a2a_tasks.lock().await.extend(loaded);
    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-stale-status")
        .expect("control-plane row should exist");
    assert_eq!(entry["state"], "TASK_STATE_COMPLETED");
    assert_eq!(entry["responseText"], "done");
    assert!(
        entry["a2aTask"]["metadata"]
            .get("pushNotificationConfigs")
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_preserves_embedded_response_when_top_level_response_missing() {
    let root = TestDir::new("a2a-task-ledger-missing-response");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-missing-response",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-missing-response",
                    "contextId": "ctx-missing-response",
                    "text": "please do the thing",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-missing-response",
                        "contextId": "ctx-missing-response",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-missing-response",
                                "contextId": "ctx-missing-response",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "agent actually finished it" }]
                            },
                            "timestamp": "2026-05-15T00:01:00Z"
                        },
                        "history": [],
                        "metadata": { "workspaceId": "ws-missing-response" }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-missing-response"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        loaded["task-missing-response"]["status"]["timestamp"],
        "2026-05-15T00:02:00Z"
    );
    assert_eq!(
        loaded["task-missing-response"]["status"]["message"]["parts"][0]["text"],
        "agent actually finished it"
    );

    state.a2a_tasks.lock().await.extend(loaded);
    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-missing-response")
        .expect("control-plane row should exist");
    assert_eq!(entry["state"], "TASK_STATE_COMPLETED");
    assert_eq!(entry["responseText"], "agent actually finished it");
    assert_ne!(entry["responseText"], "please do the thing");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_prefers_top_level_transcript_over_stale_embedded_response() {
    let root = TestDir::new("a2a-task-ledger-transcript-response");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-transcript-response",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-transcript-response",
                    "contextId": "ctx-transcript-response",
                    "text": "please do the thing",
                    "state": "TASK_STATE_COMPLETED",
                    "transcript": [
                        { "role": "user", "text": "please do the thing", "messageId": "user-transcript-response" },
                        { "role": "agent", "text": "agent transcript finished it", "messageId": "agent-transcript-response" }
                    ],
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-transcript-response",
                        "contextId": "ctx-transcript-response",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-transcript-response",
                                "contextId": "ctx-transcript-response",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "stale embedded response" }]
                            },
                            "timestamp": "2026-05-15T00:01:00Z"
                        },
                        "history": [],
                        "metadata": { "workspaceId": "ws-transcript-response" }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-transcript-response"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        loaded["task-transcript-response"]["status"]["message"]["parts"][0]["text"],
        "agent transcript finished it"
    );

    state.a2a_tasks.lock().await.extend(loaded);
    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-transcript-response")
        .expect("control-plane row should exist");
    assert_eq!(entry["responseText"], "agent transcript finished it");
    assert_ne!(entry["responseText"], "stale embedded response");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_ignores_blank_top_level_response_when_embedded_response_present()
{
    let root = TestDir::new("a2a-task-ledger-blank-response");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-blank-response",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-blank-response",
                    "contextId": "ctx-blank-response",
                    "text": "please do the thing",
                    "responseText": "   ",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-blank-response",
                        "contextId": "ctx-blank-response",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-blank-response",
                                "contextId": "ctx-blank-response",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "agent actually finished it" }]
                            },
                            "timestamp": "2026-05-15T00:01:00Z"
                        },
                        "history": [],
                        "metadata": { "workspaceId": "ws-blank-response" }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-blank-response"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        loaded["task-blank-response"]["status"]["timestamp"],
        "2026-05-15T00:02:00Z"
    );
    assert_eq!(
        loaded["task-blank-response"]["status"]["message"]["parts"][0]["text"],
        "agent actually finished it"
    );

    state.a2a_tasks.lock().await.extend(loaded);
    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-blank-response")
        .expect("control-plane row should exist");
    assert_eq!(entry["state"], "TASK_STATE_COMPLETED");
    assert_eq!(entry["responseText"], "agent actually finished it");
    assert_ne!(entry["responseText"], "please do the thing");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_preserves_rich_parts_when_history_message_is_refreshed() {
    let root = TestDir::new("a2a-task-ledger-rich-parts-refresh");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-rich-parts-refresh",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-rich-parts-refresh",
                    "contextId": "ctx-rich-parts-refresh",
                    "text": "inspect attachment",
                    "responseText": "saw attachment",
                    "state": "TASK_STATE_WORKING",
                    "transcript": [
                        { "role": "agent", "text": "saw attachment", "messageId": "agent-rich-parts" }
                    ],
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-rich-parts-refresh",
                        "contextId": "ctx-rich-parts-refresh",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-rich-parts",
                                "contextId": "ctx-rich-parts-refresh",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "stale summary" }]
                            }
                        },
                        "history": [
                            {
                                "messageId": "agent-rich-parts",
                                "contextId": "ctx-rich-parts-refresh",
                                "role": "ROLE_AGENT",
                                "parts": [
                                    { "text": "stale summary", "partId": "part-stale" },
                                    { "data": { "attachmentId": "artifact-rich-parts" } }
                                ]
                            }
                        ],
                        "metadata": { "workspaceId": "ws-rich-parts" }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    let refreshed_parts = &loaded["task-rich-parts-refresh"]["history"][0]["parts"];
    let refreshed_parts_array = refreshed_parts
        .as_array()
        .expect("parts should be an array");
    let attachment_part = refreshed_parts_array
        .iter()
        .find(|part| part.get("data").is_some())
        .expect("rich attachment part must survive transcript refresh");
    assert_eq!(
        attachment_part["data"]["attachmentId"],
        "artifact-rich-parts"
    );
    let text_part = refreshed_parts_array
        .iter()
        .find(|part| part.get("text").is_some())
        .expect("plain-text part must be present after refresh");
    assert_eq!(
        text_part["text"], "saw attachment",
        "plain-text part must be refreshed from the transcript candidate"
    );
    assert_ne!(
        text_part["text"], "stale summary",
        "stale plain-text part must not outlive the transcript refresh"
    );
    assert!(
        !refreshed_parts_array
            .iter()
            .any(|part| part.get("partId").is_some()),
        "decorated text part with partId must be replaced, not preserved with stale text"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_refreshes_stale_status_from_embedded_history() {
    let root = TestDir::new("a2a-task-ledger-stale-status-history");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-stale-status-history",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "task-stale-status-history",
                    "contextId": "ctx-stale-status-history",
                    "text": "finish work",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:02:00Z",
                    "a2aTask": {
                        "id": "task-stale-status-history",
                        "contextId": "ctx-stale-status-history",
                        "status": {
                            "state": "TASK_STATE_WORKING",
                            "message": {
                                "messageId": "agent-stale-status-history",
                                "contextId": "ctx-stale-status-history",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "still working" }]
                            },
                            "timestamp": "2026-05-15T00:01:00Z"
                        },
                        "history": [
                            {
                                "messageId": "agent-stale-status-history-final",
                                "contextId": "ctx-stale-status-history",
                                "role": "ROLE_AGENT",
                                "parts": [{ "text": "done with the work" }]
                            }
                        ],
                        "metadata": { "workspaceId": "ws-stale-status-history" }
                    }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;

    assert_eq!(
        loaded["task-stale-status-history"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        loaded["task-stale-status-history"]["status"]["message"]["parts"][0]["text"],
        "done with the work"
    );
    assert_ne!(
        loaded["task-stale-status-history"]["status"]["message"]["parts"][0]["text"],
        "still working"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_store_preserves_canceled_task_across_canonical_state_aliases() {
    let root = TestDir::new("a2a-task-canceled-canonical-aliases");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };

    for alias in [
        "CANCELLED",
        "STATE_CANCELED",
        "TASK_STATE_CANCELLED",
        "canceled",
    ] {
        let task_id = format!("task-canceled-{alias}");
        let canceled_task = a2a_task_value(
            &task_id,
            "ctx-canceled",
            alias,
            a2a_agent_message("ctx-canceled", "canceled by operator"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({ "workspaceId": "ws-canceled" }),
        );
        store_a2a_task_unless_canceled(&state, &task_id, canceled_task).await;

        let completion_task = a2a_task_value(
            &task_id,
            "ctx-canceled",
            "TASK_STATE_COMPLETED",
            a2a_agent_message("ctx-canceled", "finished anyway"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({ "workspaceId": "ws-canceled" }),
        );
        let stored = store_a2a_task_unless_canceled(&state, &task_id, completion_task).await;

        assert_eq!(
            stored["status"]["state"], alias,
            "canceled task with alias {alias} must not be overwritten by a later store"
        );
        assert_ne!(
            stored["status"]["state"], "TASK_STATE_COMPLETED",
            "canceled task with alias {alias} must not be replaced by completion"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_ledger_redacts_compound_secret_metadata_keys() {
    let root = TestDir::new("a2a-task-ledger-compound-secret-keys");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    let task = a2a_task_value(
        "task-compound-secrets",
        "ctx-compound-secrets",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-compound-secrets", "done"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({
            "workspaceId": "ws-compound",
            "nonSecret": "keep-visible",
            "nonCredentials": "keep-visible-too",
            "notASecret": "keep-visible-three",
            "webhookSecret": "wh-secret-value",
            "oauthToken": "oauth-token-value",
            "apiPassword": "api-password-value",
            "signingKey": "keep-visible-key"
        }),
    );
    store_a2a_task_unless_canceled(&state, "task-compound-secrets", task).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-compound-secrets")
        .expect("control-plane row should exist");
    let metadata = &entry["metadata"];
    assert_eq!(metadata["workspaceId"], "ws-compound");
    assert_eq!(metadata["nonSecret"], "keep-visible");
    assert_eq!(metadata["nonCredentials"], "keep-visible-too");
    assert_eq!(metadata["notASecret"], "keep-visible-three");
    assert_eq!(metadata["signingKey"], "keep-visible-key");
    assert!(
        metadata.get("webhookSecret").is_none(),
        "webhookSecret must be redacted"
    );
    assert!(
        metadata.get("oauthToken").is_none(),
        "oauthToken must be redacted"
    );
    assert!(
        metadata.get("apiPassword").is_none(),
        "apiPassword must be redacted"
    );

    let embedded_metadata = &entry["a2aTask"]["metadata"];
    assert!(embedded_metadata.get("webhookSecret").is_none());
    assert!(embedded_metadata.get("oauthToken").is_none());
    assert!(embedded_metadata.get("apiPassword").is_none());
    assert_eq!(embedded_metadata["nonSecret"], "keep-visible");
    assert_eq!(embedded_metadata["signingKey"], "keep-visible-key");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_persist_rewrites_legacy_control_plane_ledger_entries() {
    let root = TestDir::new("a2a-task-ledger-legacy-rewrite");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "remote-ledger-row",
                    "kind": "message",
                    "peer": "peer-b",
                    "taskId": "remote-task",
                    "text": "remote peer task",
                    "state": "TASK_STATE_COMPLETED",
                    "createdAt": "2026-05-15T00:00:00Z",
                    "updatedAt": "2026-05-15T00:01:00Z"
                },
                {
                    "id": "raw-legacy-task",
                    "contextId": "ctx-legacy",
                    "status": {
                        "state": "TASK_STATE_COMPLETED",
                        "message": {
                            "messageId": "legacy-msg",
                            "contextId": "ctx-legacy",
                            "role": "ROLE_AGENT",
                            "parts": [{ "text": "legacy complete" }]
                        }
                    },
                    "metadata": { "workspaceId": "legacy-ws" }
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;
    state.a2a_tasks.lock().await.extend(loaded);

    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entries = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array");
    let control_plane_entry = entries
        .iter()
        .find(|entry| entry["taskId"] == "raw-legacy-task")
        .expect("legacy task should be rewritten as a control-plane row");

    assert_eq!(entries.len(), 2);
    assert_eq!(control_plane_entry["peer"], A2A_RUNTIME_GATEWAY_LEDGER_PEER);
    assert_eq!(control_plane_entry["kind"], "delegation");
    assert_eq!(control_plane_entry["a2aTask"]["id"], "raw-legacy-task");
    assert_eq!(
        control_plane_entry["a2aTask"]["metadata"]["workspaceId"],
        "legacy-ws"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.get("peer").is_none() && entry["id"] == "raw-legacy-task")
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_load_canonicalizes_legacy_cancelled_state() {
    let root = TestDir::new("a2a-task-ledger-legacy-cancelled");
    let ledger_path = root.path().join("tasks.json");
    tokio::fs::write(
        &ledger_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-runtime-gateway-cancelled-task",
                    "kind": "delegation",
                    "peer": A2A_RUNTIME_GATEWAY_LEDGER_PEER,
                    "taskId": "cancelled-task",
                    "contextId": "ctx-cancelled",
                    "text": "task canceled elsewhere",
                    "state": "CANCELLED",
                    "updatedAt": "2026-05-15T00:01:00Z"
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&ledger_path).await;

    assert_eq!(
        loaded["cancelled-task"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_ledger_migrates_legacy_control_plane_peer() {
    let root = TestDir::new("a2a-task-ledger-legacy-control-plane-peer");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    tokio::fs::write(
        &state.config.a2a_tasks_file_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "tasks": [
                {
                    "id": "maestro-control-plane-legacy-task",
                    "kind": "delegation",
                    "peer": "maestro-control-plane",
                    "peerDisplayName": "Maestro Control Plane",
                    "taskId": "legacy-task",
                    "contextId": "ctx-legacy",
                    "text": "legacy request",
                    "state": "TASK_STATE_COMPLETED",
                    "updatedAt": "2026-05-15T00:01:00Z"
                }
            ]
        }))
        .expect("ledger should serialize"),
    )
    .await
    .expect("ledger should be written");

    let loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;
    assert!(loaded.contains_key("legacy-task"));
    state.a2a_tasks.lock().await.extend(loaded);

    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entries = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["peer"], A2A_RUNTIME_GATEWAY_LEDGER_PEER);
    assert_eq!(
        entries[0]["peerDisplayName"],
        A2A_RUNTIME_GATEWAY_LEDGER_DISPLAY_NAME
    );
    assert_eq!(entries[0]["taskId"], "legacy-task");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_stream_emits_task_status_and_artifact_events() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "streamed fake response");
    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-stream","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}],"metadata":{"evalops.delegationProjection":{"delegationId":"delegation-stream","kind":"progress","lifecycleState":"active","summary":"Delegated work is running.","availableControls":["steer","pause","resume","cancel"],"openUrl":"https://orb.example/tasks/stream"}}}}"#;
    let request = format!(
        "POST /message:stream HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let state = test_app_state_with_sessions(HashMap::new());
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        handle_a2a_streaming_endpoint(server, initial, head, state_for_run).await
    });
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut bytes))
        .await
        .expect("stream response should close")
        .expect("stream response should be readable");
    run.await
        .expect("stream task should join")
        .expect("stream endpoint should succeed");
    let response = String::from_utf8(bytes).expect("response should be utf-8");

    assert!(response.contains("Content-Type: text/event-stream"));
    assert!(response.contains("id: a2a:ctx-stream:maestro-task-"));
    assert!(response.contains(":task:TASK_STATE_WORKING:"));
    assert!(response.contains(":status:TASK_STATE_COMPLETED:"));
    assert!(response.contains(":artifact:maestro-task-"));
    assert!(response.contains("-assistant-response"));
    assert!(response.contains("event: task"));
    assert!(response.contains("event: statusUpdate"));
    assert!(response.contains("event: artifactUpdate"));
    assert!(response.contains("\"statusUpdate\""));
    assert!(response.contains("\"artifactUpdate\""));
    assert!(response.contains("\"delegation\""));
    assert!(response.contains("delegation-stream"));
    assert!(response.contains("steer"));
    assert!(response.contains("https://orb.example/tasks/stream"));
    assert!(response.contains("streamed fake response"));
    assert_eq!(state.a2a_tasks.lock().await.len(), 1);

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_task_follow_up_from_other_subject() {
    let _guard = ENV_LOCK.lock().await;
    let previous_secret = env::var_os("MAESTRO_JWT_SECRET");
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "should not run");
    let state = test_app_state_with_sessions(HashMap::new());
    let existing_message = a2a_agent_message("ctx-1", "Need more input");
    let task = a2a_task_value(
        "owned-task",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        existing_message.clone(),
        vec![existing_message],
        Vec::new(),
        serde_json::json!({ "ownerSubject": "user-123" }),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("owned-task".to_string(), task);
    let token = hs256_signed_claims(
        b"shared-secret",
        serde_json::json!({
            "sub": "user-456",
            "organization_id": "org_test",
            "workspace_id": "ws_test",
            "scope": "maestro:write",
            "exp": now_secs() + 60,
        }),
    );
    let body = r#"{"message":{"messageId":"msg-2","contextId":"ctx-1","taskId":"owned-task","role":"ROLE_USER","parts":[{"text":"follow up","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"]["code"], "TASK_NOT_FOUND");
    assert_eq!(
        state.a2a_tasks.lock().await["owned-task"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );

    if let Some(previous_secret) = previous_secret {
        env::set_var("MAESTRO_JWT_SECRET", previous_secret);
    } else {
        env::remove_var("MAESTRO_JWT_SECRET");
    }
    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_unknown_task_id() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "should not run");

    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","taskId":"missing-task","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"]["code"], "TASK_NOT_FOUND");
    assert!(state.a2a_tasks.lock().await.is_empty());

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_reuses_existing_task_id_and_history() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "follow-up response");

    let body = r#"{"message":{"messageId":"msg-2","taskId":"maestro-task-1","role":"ROLE_USER","parts":[{"text":"follow up","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nx-evalops-session-id: header-ctx\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());
    let existing_message = a2a_agent_message("ctx-1", "Need more input");
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        existing_message.clone(),
        vec![existing_message],
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let task = &response["task"];

    assert_eq!(task["id"], "maestro-task-1");
    assert_eq!(task["contextId"], "ctx-1");
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(task["history"].as_array().unwrap().len(), 3);
    assert_eq!(task["history"][1]["contextId"], "ctx-1");
    assert_eq!(task["history"][2]["parts"][0]["text"], "follow-up response");

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_terminal_task_id() {
    let body = r#"{"message":{"messageId":"msg-2","contextId":"ctx-1","taskId":"maestro-task-1","role":"ROLE_USER","parts":[{"text":"follow up","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-1", "complete"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"]["code"], "UNSUPPORTED_OPERATION");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_active_task_id() {
    let body = r#"{"message":{"messageId":"msg-2","contextId":"ctx-1","taskId":"maestro-task-1","role":"ROLE_USER","parts":[{"text":"follow up","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    state
        .a2a_cancel_senders
        .lock()
        .await
        .insert("maestro-task-1".to_string(), cancel_tx);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(response["error"]["code"], "UNSUPPORTED_OPERATION");
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    assert_eq!(state.a2a_cancel_senders.lock().await.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_claims_input_task_before_launch() {
    let state = test_app_state_with_sessions(HashMap::new());
    let existing_message = a2a_agent_message("ctx-1", "Need more input");
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        existing_message.clone(),
        vec![existing_message],
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);
    let head = RequestHead {
        method: "POST".to_string(),
        path: "/message:send".to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
    };
    let request = A2ASendMessageRequest {
        message: A2AMessageBody {
            message_id: Some("msg-2".to_string()),
            context_id: None,
            task_id: Some("maestro-task-1".to_string()),
            role: Some("ROLE_USER".to_string()),
            parts: vec![A2APartBody {
                text: Some("follow up".to_string()),
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
        configuration: None,
        metadata: None,
    };
    let auth = AuthContext {
        subject: None,
        unrestricted: true,
        ..AuthContext::default()
    };

    let claimed = claim_a2a_send_task(
        &state,
        &request,
        &head,
        &auth,
        serde_json::json!({ "test": "metadata" }),
    )
    .await
    .expect("input-required task should be claimed");
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(claimed.task_id, "maestro-task-1");
    assert_eq!(claimed.context_id, "ctx-1");
    assert_eq!(claimed.history.len(), 2);
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    drop(stored);

    let response = response_json(
        claim_a2a_send_task(&state, &request, &head, &auth, serde_json::json!({}))
            .await
            .expect_err("already-claimed task should reject overlap"),
    );
    assert_eq!(response["error"]["code"], "UNSUPPORTED_OPERATION");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_restores_claimed_task_when_cancel_sender_registration_fails() {
    let body = r#"{"message":{"messageId":"msg-2","contextId":"ctx-1","taskId":"maestro-task-1","role":"ROLE_USER","parts":[{"text":"follow up","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());
    let existing_message = a2a_agent_message("ctx-1", "Need more input");
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        existing_message.clone(),
        vec![existing_message],
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    state
        .a2a_cancel_senders
        .lock()
        .await
        .insert("maestro-task-1".to_string(), cancel_tx);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(response["error"]["code"], "UNSUPPORTED_OPERATION");
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
    assert_eq!(
        stored["maestro-task-1"]["history"]
            .as_array()
            .expect("history should be an array")
            .len(),
        1
    );
    drop(stored);
    assert_eq!(state.a2a_cancel_senders.lock().await.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_cancel_rejects_terminal_tasks() {
    let (_client, mut server) = tcp_stream_pair().await;
    let mut initial =
            b"POST /tasks/maestro-task-1:cancel HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n"
                .to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-1", "complete"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(response["error"]["code"], "TASK_NOT_CANCELABLE");
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_cancel_clears_stale_artifacts() {
    let (_client, mut server) = tcp_stream_pair().await;
    let mut initial =
            b"POST /tasks/maestro-task-1:cancel HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n"
                .to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        a2a_agent_message("ctx-1", "waiting"),
        Vec::new(),
        vec![serde_json::json!({
            "artifactId": "stale-artifact",
            "parts": [{ "text": "stale", "mediaType": "text/plain" }]
        })],
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(response["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(response["artifacts"].as_array().unwrap().len(), 0);
    assert_eq!(
        stored["maestro-task-1"]["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_cancel_persists_server_owned_subagent_disposition() {
    let (_client, mut server) = tcp_stream_pair().await;
    let mut initial =
        b"POST /tasks/maestro-task-capsule:cancel HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n"
            .to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-capsule",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({
            "evalops.subagentRequest": valid_code_writer_capsule(),
            "evalops.subagentCompletion": {
                "verified": true,
                "verificationDisposition": "verified"
            },
            "workGraph": "caller-owned"
        }),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-capsule".to_string(), task);

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(response["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(
        response["metadata"]["evalops.subagentCompletion"]["observedOutcome"],
        "canceled"
    );
    assert_eq!(
        response["metadata"]["evalops.subagentCompletion"]["serverGenerated"],
        true
    );
    assert!(
        response["metadata"]["evalops.subagentCompletion"]["verified"].is_null(),
        "caller verified state must be removed atomically"
    );
    assert_eq!(response["metadata"]["workGraph"]["state"], "canceled");
    assert_eq!(
        stored["maestro-task-capsule"]["metadata"]["evalops.subagentCompletion"]["observedOutcome"],
        "canceled"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_async_completion_preserves_canceled_task_state() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "late response");

    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_CANCELED",
        a2a_agent_message("ctx-1", "Task canceled"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);

    let user_message = a2a_user_message_value(
        &A2AMessageBody {
            message_id: Some("msg-1".to_string()),
            context_id: Some("ctx-1".to_string()),
            task_id: Some("maestro-task-1".to_string()),
            role: Some("ROLE_USER".to_string()),
            parts: vec![A2APartBody {
                text: Some("hello".to_string()),
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
    let task = complete_a2a_task(
        &state,
        "hello".to_string(),
        "maestro-task-1".to_string(),
        "ctx-1".to_string(),
        vec![user_message],
        serde_json::json!({}),
        cancel_rx,
    )
    .await;
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_async_completion_preserves_cancelled_alias_state() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "late response");

    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-1",
        "ctx-1",
        "TASK_STATE_CANCELLED",
        a2a_agent_message("ctx-1", "Task cancelled"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-1".to_string(), task);

    let user_message = a2a_user_message_value(
        &A2AMessageBody {
            message_id: Some("msg-1".to_string()),
            context_id: Some("ctx-1".to_string()),
            task_id: Some("maestro-task-1".to_string()),
            role: Some("ROLE_USER".to_string()),
            parts: vec![A2APartBody {
                text: Some("hello".to_string()),
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
    let task = complete_a2a_task(
        &state,
        "hello".to_string(),
        "maestro-task-1".to_string(),
        "ctx-1".to_string(),
        vec![user_message],
        serde_json::json!({}),
        cancel_rx,
    )
    .await;
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(task["status"]["state"], "TASK_STATE_CANCELLED");
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_CANCELLED"
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_cancel_signals_return_immediately_worker() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    let previous_delay = env::var("MAESTRO_A2A_FAKE_RESPONSE_DELAY_MS").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "late response");
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE_DELAY_MS", "500");

    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}]},"configuration":{"returnImmediately":true}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let task_id = response["task"]["id"]
        .as_str()
        .expect("task id should be present")
        .to_string();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        state.a2a_tasks.lock().await[&task_id]["status"]["state"],
        "TASK_STATE_WORKING"
    );

    let cancel_request = format!(
        "POST /tasks/{task_id}:cancel HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n"
    );
    let mut cancel_initial = cancel_request.into_bytes();
    let cancel_head = parse_request_head(&cancel_initial).expect("request should parse");
    let (_client, mut cancel_server) = tcp_stream_pair().await;
    let cancel_response = response_json(
        handle_a2a_endpoint(&mut cancel_server, &mut cancel_initial, cancel_head, &state).await,
    );
    assert_eq!(cancel_response["status"]["state"], "TASK_STATE_CANCELED");

    tokio::time::sleep(Duration::from_millis(600)).await;
    let stored = state.a2a_tasks.lock().await;
    assert_eq!(stored[&task_id]["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(stored[&task_id]["artifacts"].as_array().unwrap().len(), 0);

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
    if let Some(previous_delay) = previous_delay {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE_DELAY_MS", previous_delay);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE_DELAY_MS");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_cancel_requires_csrf_token_when_enabled() {
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.require_csrf = true;
    config.csrf_token = Some("csrf-token".to_string());
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    state.a2a_tasks.lock().await.insert(
        "maestro-task-1".to_string(),
        a2a_task_value(
            "maestro-task-1",
            "ctx-1",
            "TASK_STATE_WORKING",
            a2a_agent_message("ctx-1", "working"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({ "workspaceId": "ws-1" }),
        ),
    );

    let request = "POST /tasks/maestro-task-1:cancel HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"], "Forbidden: invalid CSRF token");
    assert_eq!(
        state.a2a_tasks.lock().await["maestro-task-1"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_store_evicts_old_terminal_tasks() {
    let state = test_app_state_with_sessions(HashMap::new());
    let working_task = a2a_task_value(
        "working-task",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "still running"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    store_a2a_task_unless_canceled(&state, "working-task", working_task).await;

    for index in 0..=A2A_TERMINAL_TASK_STORE_LIMIT {
        let task_id = format!("terminal-task-{index}");
        let mut task = a2a_task_value(
            &task_id,
            "ctx-1",
            "TASK_STATE_COMPLETED",
            a2a_agent_message("ctx-1", "done"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({}),
        );
        task["status"]["timestamp"] = Value::String(format!(
            "2026-05-15T00:{:02}:{:02}Z",
            index / 60,
            index % 60
        ));
        store_a2a_task_unless_canceled(&state, &task_id, task).await;
    }

    let newest_terminal_task_id = format!("terminal-task-{A2A_TERMINAL_TASK_STORE_LIMIT}");
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(stored.len(), A2A_TERMINAL_TASK_STORE_LIMIT + 1);
    assert!(stored.contains_key("working-task"));
    assert!(!stored.contains_key("terminal-task-0"));
    assert!(stored.contains_key(&newest_terminal_task_id));
    let histories = state.a2a_task_event_history.lock().await;
    assert_eq!(
        histories.len(),
        stored.len(),
        "evicted tasks must release replay payloads"
    );
    assert!(!histories.contains_key("terminal-task-0"));
    assert!(histories.contains_key("working-task"));
    drop(histories);
    drop(stored);
    let evicted =
        serde_json::json!({"id":"terminal-task-0", "status":{"state":"TASK_STATE_COMPLETED"}});
    publish_a2a_task_update(&state, &evicted).await;
    assert!(
        !state
            .a2a_task_event_history
            .lock()
            .await
            .contains_key("terminal-task-0")
    );
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(
        stored
            .values()
            .filter(|task| a2a_task_is_terminal(task))
            .count(),
        A2A_TERMINAL_TASK_STORE_LIMIT
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_cancel_prunes_terminal_task_store() {
    let state = test_app_state_with_sessions(HashMap::new());
    for index in 0..A2A_TERMINAL_TASK_STORE_LIMIT {
        let task_id = format!("terminal-task-{index}");
        let mut task = a2a_task_value(
            &task_id,
            "ctx-1",
            "TASK_STATE_COMPLETED",
            a2a_agent_message("ctx-1", "done"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({}),
        );
        task["status"]["timestamp"] = Value::String(format!(
            "2026-05-15T00:{:02}:{:02}Z",
            index / 60,
            index % 60
        ));
        state.a2a_tasks.lock().await.insert(task_id, task);
    }
    let task = a2a_task_value(
        "cancel-task",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        a2a_agent_message("ctx-1", "Need more input"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("cancel-task".to_string(), task);
    let request = b"POST /tasks/cancel-task:cancel HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let mut initial = request.to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(response["status"]["state"], "TASK_STATE_CANCELED");
    assert!(stored.contains_key("cancel-task"));
    assert!(!stored.contains_key("terminal-task-0"));
    assert_eq!(
        stored
            .values()
            .filter(|task| a2a_task_is_terminal(task))
            .count(),
        A2A_TERMINAL_TASK_STORE_LIMIT
    );
}

#[test]
pub(crate) fn same_subject_cannot_cross_a2a_task_tenants() {
    let task = a2a_task_value(
        "task-tenant-a",
        "ctx-tenant-a",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-tenant-a", "working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({
            "ownerSubject": "user-123",
            "organizationId": "org-a",
            "workspaceId": "workspace-a",
        }),
    );
    let same_tenant = AuthContext {
        subject: Some("user-123".to_string()),
        organization_id: Some("org-a".to_string()),
        workspace_id: Some("workspace-a".to_string()),
        ..AuthContext::default()
    };
    let other_tenant = AuthContext {
        subject: Some("user-123".to_string()),
        organization_id: Some("org-b".to_string()),
        workspace_id: Some("workspace-b".to_string()),
        ..AuthContext::default()
    };

    assert!(a2a_task_visible_to_auth(&task, &same_tenant));
    assert!(!a2a_task_visible_to_auth(&task, &other_tenant));
}
