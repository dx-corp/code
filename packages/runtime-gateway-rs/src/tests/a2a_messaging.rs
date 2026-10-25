#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn detects_a2a_streaming_routes_separately() {
    for request in [
        "POST /message:stream HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /tasks/maestro-task-1:subscribe HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "POST /tasks/maestro-task-1:subscribe HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /tasks/maestro-task-1/subscribe HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        let head = parse_request_head(request.as_bytes()).expect("request should parse");
        assert!(
            is_a2a_streaming_endpoint(&head),
            "{request} should be streaming A2A"
        );
        assert!(
            !is_a2a_endpoint(&head),
            "{request} should not be handled by non-streaming A2A routing"
        );
    }
}

#[test]
pub(crate) fn a2a_acceptance_checks_become_observed_test_report_artifacts() {
    let reports = vec![serde_json::json!({
        "kind": "acceptance.check",
        "success": true,
        "package": "maestro-runtime-gateway",
        "filter": "subagent_capsule"
    })];

    let artifacts = crate::a2a::a2a_acceptance_report_artifacts("task-1", &reports);

    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["artifactKind"], "test.report");
    assert_eq!(artifacts[0]["parts"][0]["data"]["success"], true);
    assert!(
        artifacts
            .iter()
            .all(|artifact| artifact["artifactKind"] != "patch.summary"),
        "assistant prose must not be promoted to a patch summary"
    );
}

#[test]
pub(crate) fn a2a_send_message_honors_return_immediately_configuration() {
    let request = A2ASendMessageRequest {
        message: A2AMessageBody {
            message_id: Some("msg-1".to_string()),
            context_id: Some("ctx-1".to_string()),
            task_id: None,
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
        configuration: Some(serde_json::json!({ "returnImmediately": true })),
        metadata: None,
    };

    assert!(a2a_return_immediately(&request).expect("configuration should be valid"));
}

#[test]
pub(crate) fn a2a_user_message_value_replaces_empty_context_id() {
    let message = A2AMessageBody {
        message_id: Some("msg-1".to_string()),
        context_id: Some("   ".to_string()),
        task_id: None,
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
    };

    let value = a2a_user_message_value(&message, "ctx-1");

    assert_eq!(value["contextId"], "ctx-1");
}

#[test]
pub(crate) fn a2a_context_id_ignores_whitespace_before_falling_back() {
    let request = A2ASendMessageRequest {
        message: A2AMessageBody {
            message_id: Some("msg-1".to_string()),
            context_id: Some("   ".to_string()),
            task_id: None,
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
        configuration: None,
        metadata: None,
    };
    let head = RequestHead {
        method: "POST".to_string(),
        path: "/message:send".to_string(),
        query: HashMap::new(),
        headers: HashMap::from([(
            "x-evalops-session-id".to_string(),
            " header-ctx ".to_string(),
        )]),
    };

    assert_eq!(a2a_context_id(&request, &head), "header-ctx");
}

#[test]
pub(crate) fn a2a_context_id_falls_back_when_message_context_is_blank() {
    let request = A2ASendMessageRequest {
        message: A2AMessageBody {
            message_id: Some("msg-1".to_string()),
            context_id: Some("   ".to_string()),
            task_id: None,
            role: Some("ROLE_USER".to_string()),
            parts: vec![A2APartBody {
                text: Some("hello".to_string()),
                url: None,
                data: None,
                metadata: Some(serde_json::json!({ "sessionId": " metadata-ctx " })),
                filename: None,
                media_type: Some("text/plain".to_string()),
            }],
            metadata: Some(serde_json::json!({ "sessionId": " metadata-ctx " })),
            extensions: None,
            reference_task_ids: None,
        },
        configuration: None,
        metadata: None,
    };
    let head = parse_request_head(
            b"POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-evalops-session-id: header-ctx\r\n\r\n",
        )
        .expect("request should parse");

    assert_eq!(a2a_context_id(&request, &head), "metadata-ctx");
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_unsupported_extensions() {
    let body = r#"{"message":{"messageId":"msg-extension","contextId":"ctx-extension","role":"ROLE_USER","extensions":["https://example.test/a2a/extensions/unsupported/v1"],"parts":[{"text":"hello","mediaType":"text/plain"}]}}"#;
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

    assert_eq!(response["error"]["code"], "EXTENSION_NOT_SUPPORTED");
    assert!(state.a2a_tasks.lock().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_requires_csrf_token_when_enabled() {
    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.require_csrf = true;
    config.csrf_token = Some("csrf-token".to_string());
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"], "Forbidden: invalid CSRF token");
    assert!(state.a2a_tasks.lock().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_non_boolean_return_immediately() {
    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}]},"configuration":{"returnImmediately":"true"}}"#;
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

    assert_eq!(response["error"]["code"], "INVALID_REQUEST");
    assert_eq!(
        response["error"]["message"],
        "A2A configuration returnImmediately must be a boolean"
    );
    assert!(state.a2a_tasks.lock().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_non_object_configuration() {
    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}]},"configuration":"oops"}"#;
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

    assert_eq!(response["error"]["code"], "INVALID_REQUEST");
    assert_eq!(
        response["error"]["message"],
        "A2A configuration must be an object"
    );
    assert!(state.a2a_tasks.lock().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_rejects_incompatible_protocol_version() {
    let body = r#"{"message":{"messageId":"msg-1","contextId":"ctx-1","role":"ROLE_USER","parts":[{"text":"hello","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send?a2a-version=2.0 HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);

    assert_eq!(response["error"]["code"], "UNSUPPORTED_VERSION");
    assert_eq!(
        response["error"]["message"],
        "Unsupported A2A protocol version 2.0; expected 1.0"
    );
    assert!(state.a2a_tasks.lock().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_preserves_owner_metadata_on_unrestricted_follow_up() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "follow-up response");
    let state = test_app_state_with_sessions(HashMap::new());
    let existing_message = a2a_agent_message("ctx-1", "Need more input");
    let task = a2a_task_value(
        "owned-task",
        "ctx-1",
        "TASK_STATE_INPUT_REQUIRED",
        existing_message.clone(),
        vec![existing_message],
        Vec::new(),
        serde_json::json!({
            "ownerSubject": "user-123",
            "workspaceId": "ws-old"
        }),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("owned-task".to_string(), task);
    let body = r#"{"message":{"messageId":"msg-2","contextId":"ctx-1","taskId":"owned-task","role":"ROLE_USER","parts":[{"text":"follow up","mediaType":"text/plain"}]}}"#;
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nx-evalops-workspace-id: ws-new\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let task = &response["task"];

    assert_eq!(task["id"], "owned-task");
    assert_eq!(task["metadata"]["ownerSubject"], "user-123");
    assert_eq!(task["metadata"]["workspaceId"], "ws-new");
    assert_eq!(
        state.a2a_tasks.lock().await["owned-task"]["metadata"]["ownerSubject"],
        "user-123"
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
}

#[test]
pub(crate) fn a2a_vfs_hydration_gap_matches_context_files_with_zero_hydrated() {
    assert!(crate::a2a::a2a_vfs_hydration_gap(4, 0));
    assert!(!crate::a2a::a2a_vfs_hydration_gap(4, 1));
    assert!(!crate::a2a::a2a_vfs_hydration_gap(0, 0));
}
