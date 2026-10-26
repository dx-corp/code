use super::*;

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_tasks_list_supports_spec_filters_pagination_and_payload_trimming() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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
pub(crate) async fn a2a_message_send_rejects_terminal_task_id() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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

    let raw_response = handle_a2a_endpoint(&mut server, &mut initial, head, &state).await;
    let status = response_status(&raw_response);
    let response = response_json(raw_response);

    assert_eq!(status, 400, "terminal task response: {response}");
    assert_eq!(
        response["error"]["code"], "UNSUPPORTED_OPERATION",
        "{response}"
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_active_task_id() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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

    let raw_response = handle_a2a_endpoint(&mut server, &mut initial, head, &state).await;
    let status = response_status(&raw_response);
    let response = response_json(raw_response);
    let stored = state.a2a_tasks.lock().await;

    assert_eq!(status, 409, "active task response: {response}");
    assert_eq!(
        response["error"]["code"], "UNSUPPORTED_OPERATION",
        "{response}"
    );
    assert_eq!(
        stored["maestro-task-1"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    assert_eq!(state.a2a_cancel_senders.lock().await.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_restores_claimed_task_when_cancel_sender_registration_fails() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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
pub(crate) async fn a2a_cancel_prunes_terminal_task_store() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
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
