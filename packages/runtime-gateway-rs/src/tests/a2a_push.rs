#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn detects_platform_a2a_push_callback_route() {
    let head =
        parse_request_head(b"POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("request should parse");

    assert!(is_platform_a2a_push_endpoint(&head));
    assert!(!is_a2a_endpoint(&head));
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_records_extensions_and_push_config() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    let previous_disable_delivery = env::var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "hello with push config");
    env::set_var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY", "1");

    let body = format!(
        r#"{{
                "message": {{
                    "messageId": "msg-push",
                    "contextId": "ctx-push",
                    "role": "ROLE_USER",
                    "extensions": ["{EVALOPS_A2A_EXTENSION_URI}"],
                    "parts": [{{"text": "hello", "mediaType": "text/plain"}}]
                }},
                "configuration": {{
                    "taskPushNotificationConfig": {{
                        "id": "notify-1",
                        "url": "https://8.8.8.8/a2a",
                        "token": "notify-token",
                        "authentication": {{
                            "schemes": ["Bearer"],
                            "credentials": "auth-token"
                        }}
                    }}
                }}
            }}"#
    );
    let request = format!(
        "POST /message:send HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nA2A-Extensions: {EVALOPS_A2A_EXTENSION_URI}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    let task = &response["task"];

    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(
        task["metadata"]["a2aExtensions"][0],
        EVALOPS_A2A_EXTENSION_URI
    );
    assert_eq!(
        task["metadata"]["pushNotificationConfigs"][0]["taskId"],
        task["id"]
    );
    assert_eq!(
        task["metadata"]["pushNotificationConfigs"][0]["token"],
        "<redacted>"
    );
    let stored_tasks = state.a2a_tasks.lock().await;
    let stored_task = stored_tasks
        .get(task["id"].as_str().expect("task id should be a string"))
        .expect("task should be stored");
    assert_eq!(
        stored_task["metadata"]["pushNotificationConfigs"][0]["token"],
        "notify-token"
    );
    assert!(task["metadata"].get("configuration").is_none());

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
    if let Some(previous_disable_delivery) = previous_disable_delivery {
        env::set_var(
            "MAESTRO_A2A_PUSH_DISABLE_DELIVERY",
            previous_disable_delivery,
        );
    } else {
        env::remove_var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_accepts_status_updates() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", "callback-token");
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::set_var("MAESTRO_WORKSPACE_ID", "evalops");

    let state = test_app_state_with_sessions(HashMap::new());
    let body = r#"{
            "statusUpdate": {
                "taskId": "platform-run-1",
                "contextId": "ctx-platform-1",
                "status": {
                    "state": "SUCCESS",
                    "message": {
                        "messageId": "status-platform-run-1",
                        "contextId": "ctx-platform-1",
                        "role": "ROLE_AGENT",
                        "parts": [{"text": "Platform run completed", "mediaType": "text/plain"}]
                    },
                    "timestamp": "2026-05-17T00:00:00Z"
                },
                "metadata": {
                    "runtimeEventId": "event-1",
                    "runtimeEventType": "RUNTIME_EVENT_TYPE_RUN_SUCCEEDED"
                }
            }
        }"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: callback-token\r\nX-Evalops-Organization-Id: org-evalops\r\nX-Evalops-Workspace-Id: evalops\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_server = state.clone();
    let server_task =
        tokio::spawn(async move { handle_connection(server, state_for_server).await });

    client
        .write_all(request.as_bytes())
        .await
        .expect("request should write");
    client.shutdown().await.expect("client should shutdown");
    let mut response = Vec::new();
    client
        .read_to_end(&mut response)
        .await
        .expect("response should read");
    server_task
        .await
        .expect("server task should join")
        .expect("server should handle request");

    assert_eq!(response_status(&response), 202);
    let parsed = response_json(response);
    assert_eq!(parsed["accepted"], true);
    assert_eq!(parsed["taskId"], "platform-run-1");
    let tasks = state.a2a_tasks.lock().await;
    let task = tasks
        .get("platform-run-1")
        .expect("platform task should be recorded");
    assert_eq!(task["contextId"], "ctx-platform-1");
    assert_eq!(task["metadata"]["organizationId"], "org-evalops");
    assert_eq!(task["metadata"]["workspaceId"], "evalops");
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(
        task["metadata"]["lastPlatformStatusUpdate"]["runtimeEventType"],
        "RUNTIME_EVENT_TYPE_RUN_SUCCEEDED"
    );

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_accepts_workspace_derived_token() {
    // Mirrors PushNotificationTokenForWorkspace in the Platform service
    // internal/agentruntime/a2a/push.go. The smoke and any other client whose
    // callback host/path matches the agent-runtime allowlist will send this
    // HMAC variant instead of the raw shared secret.
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    let shared_secret = "callback-token";
    let workspace_id = "evalops";
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", shared_secret);
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::remove_var("MAESTRO_WORKSPACE_ID");
    env::remove_var("MAESTRO_REMOTE_RUNNER_WORKSPACE_ID");
    env::remove_var("MAESTRO_EVALOPS_WORKSPACE_ID");

    let mut mac = Hmac::<Sha256>::new_from_slice(shared_secret.as_bytes()).unwrap();
    mac.update(workspace_id.as_bytes());
    let derived = format!(
        "workspace-v1.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );

    let body =
        r#"{"statusUpdate":{"taskId":"platform-run-2","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: {derived}\r\nX-Evalops-Organization-Id: org-evalops\r\nX-Evalops-Workspace-Id: {workspace_id}\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 202);

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_accepts_legacy_agent_runtime_without_org_header() {
    // The deployed agent-runtime callback sender binds the workspace in the
    // HMAC token but predates the redundant organization header. The receiver
    // must use its configured organization as the service tenant in that case.
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    let shared_secret = "callback-token";
    let workspace_id = "evalops";
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", shared_secret);
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::remove_var("MAESTRO_WORKSPACE_ID");
    env::remove_var("MAESTRO_REMOTE_RUNNER_WORKSPACE_ID");
    env::remove_var("MAESTRO_EVALOPS_WORKSPACE_ID");

    let mut mac = Hmac::<Sha256>::new_from_slice(shared_secret.as_bytes()).unwrap();
    mac.update(workspace_id.as_bytes());
    let derived = format!(
        "workspace-v1.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );

    let body = r#"{"statusUpdate":{"taskId":"platform-legacy-org","contextId":"ctx-legacy-org","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: {derived}\r\nX-Evalops-Workspace-Id: {workspace_id}\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 202);
    let task = state
        .a2a_tasks
        .lock()
        .await
        .get("platform-legacy-org")
        .cloned()
        .expect("legacy callback task should be recorded");
    assert_eq!(task["metadata"]["organizationId"], "org-evalops");
    assert_eq!(task["metadata"]["workspaceId"], workspace_id);

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_rejects_unconfigured_organization_binding() {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    clear_env(A2A_PUSH_AUTH_ENV_NAMES);
    let shared_secret = "callback-token";
    let workspace_id = "evalops";
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", shared_secret);

    let mut mac = Hmac::<Sha256>::new_from_slice(shared_secret.as_bytes()).unwrap();
    mac.update(workspace_id.as_bytes());
    let derived = format!(
        "workspace-v1.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );
    let body =
        r#"{"statusUpdate":{"taskId":"platform-no-org","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: {derived}\r\nX-Evalops-Organization-Id: attacker-chosen-org\r\nX-Evalops-Workspace-Id: {workspace_id}\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 403);
    assert_eq!(
        response_json(response)["error"]["code"],
        "TENANT_BINDING_REQUIRED"
    );
    assert!(state.a2a_tasks.lock().await.is_empty());

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_rejects_mismatched_organization_header() {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    let shared_secret = "callback-token";
    let workspace_id = "evalops";
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", shared_secret);
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::remove_var("MAESTRO_WORKSPACE_ID");
    env::remove_var("MAESTRO_REMOTE_RUNNER_WORKSPACE_ID");
    env::remove_var("MAESTRO_EVALOPS_WORKSPACE_ID");

    let mut mac = Hmac::<Sha256>::new_from_slice(shared_secret.as_bytes()).unwrap();
    mac.update(workspace_id.as_bytes());
    let derived = format!(
        "workspace-v1.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );
    let body = r#"{"statusUpdate":{"taskId":"platform-wrong-org","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: {derived}\r\nX-Evalops-Organization-Id: attacker-chosen-org\r\nX-Evalops-Workspace-Id: {workspace_id}\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 403);
    assert_eq!(
        response_json(response)["error"]["code"],
        "TENANT_BINDING_MISMATCH"
    );
    assert!(state.a2a_tasks.lock().await.is_empty());

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_rejects_workspace_derived_token_with_wrong_workspace()
 {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", "callback-token");
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::remove_var("MAESTRO_WORKSPACE_ID");
    env::remove_var("MAESTRO_REMOTE_RUNNER_WORKSPACE_ID");
    env::remove_var("MAESTRO_EVALOPS_WORKSPACE_ID");

    // Token derived for a different workspace than the request header claims.
    let mut mac = Hmac::<Sha256>::new_from_slice(b"callback-token").unwrap();
    mac.update(b"some-other-workspace");
    let mismatched = format!(
        "workspace-v1.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );

    let body =
        r#"{"statusUpdate":{"taskId":"platform-run-3","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: {mismatched}\r\nX-Evalops-Organization-Id: org-evalops\r\nX-Evalops-Workspace-Id: evalops\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 401);
    assert!(state.a2a_tasks.lock().await.is_empty());

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_rejects_invalid_token() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", "callback-token");
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::remove_var("MAESTRO_WORKSPACE_ID");
    env::remove_var("MAESTRO_REMOTE_RUNNER_WORKSPACE_ID");
    env::remove_var("MAESTRO_EVALOPS_WORKSPACE_ID");

    let body =
        r#"{"statusUpdate":{"taskId":"platform-run-1","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: wrong-token\r\nX-Evalops-Organization-Id: org-evalops\r\nX-Evalops-Workspace-Id: evalops\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 401);
    assert!(state.a2a_tasks.lock().await.is_empty());

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_rejects_unbound_raw_shared_token() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", "callback-token");
    env::remove_var("MAESTRO_WORKSPACE_ID");
    env::remove_var("MAESTRO_REMOTE_RUNNER_WORKSPACE_ID");
    env::remove_var("MAESTRO_EVALOPS_WORKSPACE_ID");

    let body =
        r#"{"statusUpdate":{"taskId":"platform-unbound","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: callback-token\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 403);
    assert_eq!(
        response_json(response)["error"]["code"],
        "TENANT_BINDING_REQUIRED"
    );
    assert!(state.a2a_tasks.lock().await.is_empty());

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_requires_configured_token() {
    let _guard = ENV_LOCK.lock().await;
    let previous_primary = env::var_os("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN");
    let previous_legacy = env::var_os("MAESTRO_A2A_CALLBACK_TOKEN");
    env::remove_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN");
    env::remove_var("MAESTRO_A2A_CALLBACK_TOKEN");

    let body =
        r#"{"statusUpdate":{"taskId":"platform-run-1","status":{"state":"TASK_STATE_WORKING"}}}"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nX-A2a-Notification-Token: callback-token\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let state = test_app_state_with_sessions(HashMap::new());

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 503);
    let parsed = response_json(response);
    assert_eq!(parsed["error"]["code"], "CALLBACK_TOKEN_NOT_CONFIGURED");
    assert!(state.a2a_tasks.lock().await.is_empty());

    if let Some(previous_primary) = previous_primary {
        env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", previous_primary);
    } else {
        env::remove_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN");
    }
    if let Some(previous_legacy) = previous_legacy {
        env::set_var("MAESTRO_A2A_CALLBACK_TOKEN", previous_legacy);
    } else {
        env::remove_var("MAESTRO_A2A_CALLBACK_TOKEN");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_records_artifact_updates() {
    let _guard = ENV_LOCK.lock().await;
    let snapshot = snapshot_env(A2A_PUSH_AUTH_ENV_NAMES);
    env::set_var("MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN", "callback-token");
    env::set_var("MAESTRO_ORGANIZATION_ID", "org-evalops");
    env::set_var("MAESTRO_WORKSPACE_ID", "evalops");

    let state = test_app_state_with_sessions(HashMap::new());
    let body = r#"{
            "artifactUpdate": {
                "taskId": "platform-run-2",
                "contextId": "ctx-platform-2",
                "artifact": {
                    "artifactId": "artifact-1",
                    "name": "result",
                    "parts": [{"text": "artifact text", "mediaType": "text/plain"}]
                },
                "lastChunk": true
            }
        }"#;
    let request = format!(
        "POST /api/platform/a2a/push HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer callback-token\r\nX-Evalops-Organization-Id: org-evalops\r\nX-Evalops-Workspace-Id: evalops\r\nContent-Type: application/a2a+json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;

    let response = handle_platform_a2a_push_endpoint(&mut server, &mut initial, head, &state).await;

    assert_eq!(response_status(&response), 202);
    let tasks = state.a2a_tasks.lock().await;
    let task = tasks
        .get("platform-run-2")
        .expect("platform artifact task should be recorded");
    assert_eq!(task["artifacts"][0]["artifactId"], "artifact-1");
    assert_eq!(task["artifacts"][0]["parts"][0]["text"], "artifact text");

    restore_env(snapshot);
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn platform_a2a_push_callback_preserves_existing_context_without_context_update() {
    let state = test_app_state_with_sessions(HashMap::new());
    state.a2a_tasks.lock().await.insert(
        "platform-run-3".to_string(),
        serde_json::json!({
            "id": "platform-run-3",
            "contextId": "ctx-existing",
            "status": {"state": "TASK_STATE_WORKING"},
            "history": [],
            "artifacts": []
        }),
    );

    let status_update = serde_json::json!({
        "taskId": "platform-run-3",
        "status": {"state": "TASK_STATE_COMPLETED"}
    });
    let task = apply_platform_a2a_status_update(&state, &status_update)
        .await
        .expect("status update should be accepted");
    assert_eq!(task["contextId"], "ctx-existing");

    let artifact_update = serde_json::json!({
        "taskId": "platform-run-3",
        "artifact": {
            "artifactId": "artifact-ctx",
            "parts": [{"text": "keeps context", "mediaType": "text/plain"}]
        }
    });
    let task = apply_platform_a2a_artifact_update(&state, &artifact_update)
        .await
        .expect("artifact update should be accepted");
    assert_eq!(task["contextId"], "ctx-existing");
    assert_eq!(task["artifacts"][0]["artifactId"], "artifact-ctx");
}

#[test]
pub(crate) fn a2a_push_notification_payloads_use_stream_response_shape() {
    let terminal_task = a2a_task_value(
        "task-push-payload",
        "ctx-push-payload",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-push-payload", "done"),
        Vec::new(),
        vec![serde_json::json!({
            "artifactId": "artifact-1",
            "parts": [{ "text": "artifact text", "mediaType": "text/plain" }]
        })],
        serde_json::json!({
            "workspaceId": "ws-1",
            "pushNotificationConfigs": [{
                "id": "notify-1",
                "taskId": "task-push-payload",
                "url": "https://hooks.example/a2a",
                "token": "notify-token",
                "authentication": {
                    "schemes": ["Bearer"],
                    "credentials": "secret"
                }
            }]
        }),
    );

    let payloads = a2a_push_notification_payloads(&terminal_task);

    assert_eq!(payloads.len(), 3);
    assert_eq!(
        payloads[0]["statusUpdate"]["taskId"],
        serde_json::json!("task-push-payload")
    );
    assert_eq!(
        payloads[1]["artifactUpdate"]["artifact"]["artifactId"],
        serde_json::json!("artifact-1")
    );
    assert_eq!(
        payloads[2]["task"]["id"],
        serde_json::json!("task-push-payload")
    );
    assert_eq!(
        payloads[0]["statusUpdate"]["metadata"]["workspaceId"],
        "ws-1"
    );
    assert!(
        payloads[0]["statusUpdate"]["metadata"]
            .get(A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY)
            .is_none()
    );
    assert!(
        payloads[1]["artifactUpdate"]["metadata"]
            .get(A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY)
            .is_none()
    );
    assert!(
        payloads[2]["task"]["metadata"]
            .get(A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY)
            .is_none()
    );

    for payload in payloads {
        let stream_response_fields = ["task", "message", "statusUpdate", "artifactUpdate"]
            .into_iter()
            .filter(|field| payload.get(field).is_some())
            .count();
        assert_eq!(stream_response_fields, 1);
    }
}

#[test]
pub(crate) fn a2a_push_notification_tenant_headers_require_independent_durable_ids() {
    let task = serde_json::json!({
        "metadata": {
            "organizationId": "org-1",
            "workspaceId": "ws-1"
        }
    });

    let headers = a2a_push_notification_tenant_headers(&task)
        .expect("tenant-bound tasks should emit callback tenant headers");
    assert_eq!(
        headers
            .get("X-Evalops-Organization-Id")
            .and_then(|value| value.to_str().ok()),
        Some("org-1")
    );
    assert_eq!(
        headers
            .get("X-Evalops-Workspace-Id")
            .and_then(|value| value.to_str().ok()),
        Some("ws-1")
    );

    let workspace_only = serde_json::json!({
        "metadata": {"workspaceId": "ws-1"}
    });
    assert!(a2a_push_notification_tenant_headers(&workspace_only).is_none());

    let no_metadata = serde_json::json!({"id": "task-without-tenant"});
    assert!(a2a_push_notification_tenant_headers(&no_metadata).is_none());
}

#[test]
pub(crate) fn a2a_push_notification_config_rejects_unaddressable_ids() {
    for id in ["bad/id", "bad:id"] {
        let result = normalize_a2a_push_notification_config(
            "task-push",
            serde_json::json!({
                "id": id,
                "taskId": "task-push",
                "url": "https://hooks.example/a2a"
            }),
            true,
        );

        assert!(result.is_err(), "expected {id} to be rejected");
    }
}

#[test]
pub(crate) fn a2a_push_notification_config_generates_distinct_ids_when_missing() {
    let first = normalize_a2a_push_notification_config(
        "task-push",
        serde_json::json!({
            "taskId": "task-push",
            "url": "https://8.8.8.8/a2a"
        }),
        true,
    )
    .expect("first config should normalize");
    let second = normalize_a2a_push_notification_config(
        "task-push",
        serde_json::json!({
            "taskId": "task-push",
            "url": "https://8.8.8.8/a2a"
        }),
        true,
    )
    .expect("second config should normalize");

    let first_id = first["id"].as_str().expect("first id should exist");
    let second_id = second["id"].as_str().expect("second id should exist");
    assert_ne!(first_id, "task-push");
    assert_ne!(second_id, "task-push");
    assert_ne!(first_id, second_id);
    assert!(first_id.starts_with("pushcfg-"));
    assert!(second_id.starts_with("pushcfg-"));
}

#[test]
pub(crate) fn a2a_push_private_ip_check_includes_ipv4_mapped_ipv6() {
    let mapped_loopback = "::ffff:127.0.0.1"
        .parse::<IpAddr>()
        .expect("mapped IPv6 parses");

    assert!(a2a_push_ip_is_private(mapped_loopback));
}

#[test]
pub(crate) fn a2a_push_private_ip_check_includes_cgnat_shared_address_space() {
    // 100.64.0.0/10 (RFC 6598 Shared Address Space / CGNAT). This fleet's
    // Tailscale network lives entirely inside this range, and it is not
    // covered by `Ipv4Addr::is_private()` -- a push notification callback
    // URL pointed at a Tailscale peer or at Alibaba Cloud's metadata
    // endpoint (100.100.100.200) would previously have been accepted.
    let cgnat = "100.64.0.5".parse::<IpAddr>().expect("CGNAT IPv4 parses");
    let alibaba_metadata = "100.100.100.200"
        .parse::<IpAddr>()
        .expect("Alibaba metadata IPv4 parses");

    assert!(a2a_push_ip_is_private(cgnat));
    assert!(a2a_push_ip_is_private(alibaba_metadata));
    assert!(!a2a_push_ip_is_private(
        "100.63.255.255"
            .parse::<IpAddr>()
            .expect("adjacent public IPv4 parses")
    ));
}

#[test]
pub(crate) fn a2a_push_private_ip_check_includes_cgnat_via_ipv4_mapped_ipv6() {
    // Classic bypass: encode the blocked IPv4 target as an IPv4-mapped IPv6
    // literal to route around a naive IPv4-only blocklist.
    let mapped_cgnat = "::ffff:100.64.0.1"
        .parse::<IpAddr>()
        .expect("mapped CGNAT IPv6 parses");

    assert!(a2a_push_ip_is_private(mapped_cgnat));
}

#[test]
pub(crate) fn a2a_push_private_ip_check_includes_remaining_special_use_ranges() {
    for literal in [
        "0.0.0.5",         // 0.0.0.0/8 "this network"
        "192.0.0.8",       // 192.0.0.0/24 IETF protocol assignment
        "198.18.0.1",      // 198.18.0.0/15 benchmarking
        "240.0.0.1",       // 240.0.0.0/4 reserved
        "224.0.0.1",       // multicast
        "255.255.255.255", // broadcast
    ] {
        let addr = literal
            .parse::<IpAddr>()
            .unwrap_or_else(|_| panic!("{literal} should parse as an IP"));
        assert!(
            a2a_push_ip_is_private(addr),
            "{literal} should be treated as private/reserved"
        );
    }

    assert!(!a2a_push_ip_is_private(
        "8.8.8.8".parse::<IpAddr>().expect("public IPv4 parses")
    ));
}

#[test]
pub(crate) fn a2a_push_private_ip_check_includes_ranges_that_only_net_guard_covered() {
    // These reached this call site while `a2a_push_ip_is_private` kept its own
    // copy of the range list. It now delegates to
    // `maestro_local_host::tools::net_guard::is_blocked_ip`.
    for literal in [
        "192.0.2.1",          // documentation TEST-NET-1
        "198.51.100.1",       // documentation TEST-NET-2
        "203.0.113.1",        // documentation TEST-NET-3
        "192.88.99.1",        // 6to4 relay anycast
        "::ffff:192.88.99.1", // ... and its IPv4-mapped form
        "fec0::1",            // deprecated site-local
        "64:ff9b::10.0.0.1",  // NAT64 translation of an RFC 1918 address
        "64:ff9b:1::1",       // RFC 8215 local-use NAT64 prefix
        "100::1",             // discard-only
        "2001::1",            // Teredo
        "2001:2::1",          // benchmarking
        "2001:10::1",         // deprecated ORCHID
        "2001:db8::1",        // documentation
        "2002::1",            // 6to4
        "3fff::1",            // RFC 9637 documentation
        "5f00::1",            // segment routing SIDs
    ] {
        let addr = literal
            .parse::<IpAddr>()
            .unwrap_or_else(|_| panic!("{literal} should parse as an IP"));
        assert!(a2a_push_ip_is_private(addr), "{literal} was accepted");
    }

    // Globally reachable neighbours of those ranges stay reachable.
    for literal in ["2001:1::1", "2001:20::1", "3fff:1000::1", "192.0.0.9"] {
        let addr = literal
            .parse::<IpAddr>()
            .unwrap_or_else(|_| panic!("{literal} should parse as an IP"));
        assert!(!a2a_push_ip_is_private(addr), "{literal} was rejected");
    }
}

#[test]
pub(crate) fn a2a_push_pinned_addr_rejects_any_private_resolved_address() {
    let addresses = vec![
        "93.184.216.34:443"
            .parse()
            .expect("public socket addr parses"),
        "169.254.169.254:443"
            .parse()
            .expect("metadata socket addr parses"),
    ];

    let error = a2a_push_select_pinned_addr("hooks.example", addresses, false)
        .expect_err("mixed private resolution should be rejected");

    assert!(error.contains("host is private"));
}

#[test]
pub(crate) fn a2a_push_pinned_addr_selects_public_resolved_address() {
    let expected = "93.184.216.34:443"
        .parse()
        .expect("public socket addr parses");

    let selected = a2a_push_select_pinned_addr("hooks.example", vec![expected], false)
        .expect("public resolution should be accepted");

    assert_eq!(selected, expected);
}

#[test]
pub(crate) fn a2a_push_pinned_addr_allows_private_when_explicitly_enabled() {
    let expected = "127.0.0.1:443"
        .parse()
        .expect("loopback socket addr parses");

    let selected = a2a_push_select_pinned_addr("local.test", vec![expected], true)
        .expect("private resolution should be accepted when explicitly enabled");

    assert_eq!(selected, expected);
}

#[test]
pub(crate) fn a2a_push_authorization_header_accepts_schemes_list() {
    let authentication = serde_json::json!({
        "schemes": ["Bearer"],
        "credentials": "secret"
    });
    let header = a2a_push_authorization_header(
        authentication
            .as_object()
            .expect("authentication should be an object"),
    );

    assert_eq!(header.as_deref(), Some("Bearer secret"));
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_message_send_ignores_push_config_metadata_smuggling() {
    let _guard = ENV_LOCK.lock().await;
    let previous_fake = env::var("MAESTRO_A2A_FAKE_RESPONSE").ok();
    let previous_disable_delivery = env::var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY").ok();
    env::set_var("MAESTRO_A2A_FAKE_RESPONSE", "hello without smuggled push");
    env::set_var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY", "1");

    let body = r#"{
            "message": {
                "messageId": "msg-smuggle",
                "contextId": "ctx-smuggle",
                "role": "ROLE_USER",
                "parts": [{"text": "hello", "mediaType": "text/plain"}],
                "metadata": {
                    "pushNotificationConfigs": [
                        {"id": "msg-smuggled", "url": "https://hooks.example/a2a"}
                    ]
                }
            },
            "metadata": {
                "pushNotificationConfigs": [
                    {"id": "request-smuggled", "url": "https://hooks.example/a2a"}
                ]
            }
        }"#;
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

    assert!(
        response["task"]["metadata"]
            .get(A2A_PUSH_NOTIFICATION_CONFIG_METADATA_KEY)
            .is_none()
    );

    if let Some(previous_fake) = previous_fake {
        env::set_var("MAESTRO_A2A_FAKE_RESPONSE", previous_fake);
    } else {
        env::remove_var("MAESTRO_A2A_FAKE_RESPONSE");
    }
    if let Some(previous_disable_delivery) = previous_disable_delivery {
        env::set_var(
            "MAESTRO_A2A_PUSH_DISABLE_DELIVERY",
            previous_disable_delivery,
        );
    } else {
        env::remove_var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_push_notification_config_crud_updates_task_metadata() {
    let _guard = ENV_LOCK.lock().await;
    let previous_disable_delivery = env::var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY").ok();
    env::set_var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY", "1");
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "task-push",
        "ctx-push",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-push", "working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("task-push".to_string(), task);

    let body = r#"{"id":"notify-1","taskId":"task-push","url":"https://8.8.8.8/a2a","token":"notify-token"}"#;
    let request = format!(
        "POST /tasks/task-push/pushNotificationConfigs HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut initial = request.into_bytes();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let created = response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    assert_eq!(created["id"], "notify-1");
    assert_eq!(created["taskId"], "task-push");
    assert_eq!(created["token"], "<redacted>");
    assert_eq!(
        state.a2a_tasks.lock().await["task-push"]["metadata"]["pushNotificationConfigs"][0]["token"],
        "notify-token"
    );

    let mut initial =
            b"GET /tasks/task-push/pushNotificationConfigs HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n".to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let listed = response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    assert_eq!(listed["configs"][0]["id"], "notify-1");
    assert_eq!(listed["configs"][0]["token"], "<redacted>");

    let mut initial =
            b"GET /tasks/task-push/pushNotificationConfigs/notify-1 HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n".to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let fetched = response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    assert_eq!(fetched["token"], "<redacted>");

    let mut initial =
            b"DELETE /tasks/task-push/pushNotificationConfigs/notify-1 HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n".to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let deleted = response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    assert_eq!(deleted, serde_json::json!({}));
    assert!(
        state.a2a_tasks.lock().await["task-push"]["metadata"]
            .get("pushNotificationConfigs")
            .is_none()
    );

    let mut initial =
            b"DELETE /tasks/task-push/pushNotificationConfigs/notify-1 HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n".to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (_client, mut server) = tcp_stream_pair().await;
    let deleted_again =
        response_json(handle_a2a_endpoint(&mut server, &mut initial, head, &state).await);
    assert_eq!(deleted_again, serde_json::json!({}));

    if let Some(previous_disable_delivery) = previous_disable_delivery {
        env::set_var(
            "MAESTRO_A2A_PUSH_DISABLE_DELIVERY",
            previous_disable_delivery,
        );
    } else {
        env::remove_var("MAESTRO_A2A_PUSH_DISABLE_DELIVERY");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_store_preserves_in_flight_push_configs_for_reload() {
    let root = TestDir::new("a2a-task-ledger-push-config-reload");
    let base_state = test_app_state_with_sessions(HashMap::new());
    let mut config = auth_test_config();
    config.a2a_tasks_file_path = root.path().join("tasks.json");
    let state = AppState {
        config: Arc::new(config),
        ..base_state
    };
    let task = a2a_task_value(
        "task-push-reload",
        "ctx-push-reload",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-push-reload", "still working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({
            "workspaceId": "ws-push",
            "pushNotificationConfigs": [
                {
                    "id": "notify-1",
                    "taskId": "task-push-reload",
                    "url": "https://hooks.example/a2a",
                    "token": "notify-token",
                    "authentication": {
                        "schemes": ["Bearer"],
                        "credentials": "auth-token"
                    }
                }
            ]
        }),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("task-push-reload".to_string(), task);

    persist_a2a_tasks(&state).await;

    let ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("database should be readable");
    let entry = ledger["tasks"]
        .as_array()
        .expect("ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-push-reload")
        .expect("control-plane row should exist");
    assert!(entry["metadata"].get("pushNotificationConfigs").is_none());
    assert_eq!(
        entry["a2aTask"]["metadata"]["pushNotificationConfigs"][0]["token"],
        "notify-token"
    );
    assert_eq!(
        entry["a2aTask"]["metadata"]["pushNotificationConfigs"][0]["authentication"]["credentials"],
        "auth-token"
    );

    let mut loaded = load_a2a_tasks(&state.config.a2a_tasks_file_path).await;
    assert_eq!(
        loaded["task-push-reload"]["metadata"]["pushNotificationConfigs"][0]["token"],
        "notify-token"
    );

    let mut completed_task = loaded
        .remove("task-push-reload")
        .expect("reloaded task should exist");
    completed_task["status"]["state"] = Value::String("TASK_STATE_COMPLETED".to_string());
    {
        let mut tasks = state.a2a_tasks.lock().await;
        tasks.clear();
        tasks.insert("task-push-reload".to_string(), completed_task);
    }
    persist_a2a_tasks(&state).await;

    let terminal_ledger = maestro_a2a_ledger::load(&state.config.a2a_tasks_file_path)
        .expect("terminal database should be readable");
    let terminal_entry = terminal_ledger["tasks"]
        .as_array()
        .expect("terminal ledger tasks should be an array")
        .iter()
        .find(|entry| entry["taskId"] == "task-push-reload")
        .expect("terminal control-plane row should exist");
    assert!(
        terminal_entry["a2aTask"]["metadata"]
            .get("pushNotificationConfigs")
            .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_subscribe_rejects_existing_terminal_task() {
    let _guard = ENV_LOCK.lock().await;
    let state = test_app_state_with_sessions(HashMap::new());
    let task = a2a_task_value(
        "maestro-task-subscribe",
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
        .insert("maestro-task-subscribe".to_string(), task);
    let request = "GET /tasks/maestro-task-subscribe:subscribe HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        handle_a2a_streaming_endpoint(server, initial, head, state_for_run).await
    });
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut bytes))
        .await
        .expect("subscribe response should close")
        .expect("subscribe response should be readable");
    run.await
        .expect("subscribe task should join")
        .expect("subscribe endpoint should succeed");
    let response = String::from_utf8(bytes).expect("response should be utf-8");
    let body: Value =
        serde_json::from_str(response_body_text(&response)).expect("body should be json");

    assert!(response.contains("400 Bad Request"));
    assert_eq!(body["error"]["code"], "UNSUPPORTED_OPERATION");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("terminal tasks cannot be subscribed")
    );
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_subscribe_streams_active_task_until_terminal_update() {
    let _guard = ENV_LOCK.lock().await;
    let previous_timeout = env::var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS").ok();
    let previous_heartbeat = env::var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS").ok();
    env::set_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS", "250");
    env::set_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS", "10");
    let state = test_app_state_with_sessions(HashMap::new());
    let initial_task = a2a_task_value(
        "maestro-task-active-subscribe",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        vec![a2a_agent_message("ctx-1", "working")],
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-active-subscribe".to_string(), initial_task);
    let request = "GET /tasks/maestro-task-active-subscribe:subscribe HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        handle_a2a_streaming_endpoint(server, initial, head, state_for_run).await
    });
    let mut response_bytes = vec![0_u8; 4096];
    let initial_len =
        tokio::time::timeout(Duration::from_secs(2), client.read(&mut response_bytes))
            .await
            .expect("subscribe response should start")
            .expect("subscribe response should be readable");
    response_bytes.truncate(initial_len);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let terminal_task = a2a_task_value(
        "maestro-task-active-subscribe",
        "ctx-1",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-1", "complete"),
        vec![
            a2a_agent_message("ctx-1", "working"),
            a2a_agent_message("ctx-1", "complete"),
        ],
        vec![serde_json::json!({
            "artifactId": "artifact-1",
            "parts": [{ "text": "artifact body", "mediaType": "text/plain" }]
        })],
        serde_json::json!({}),
    );
    state.a2a_tasks.lock().await.insert(
        "maestro-task-active-subscribe".to_string(),
        terminal_task.clone(),
    );
    publish_a2a_task_update(&state, &terminal_task).await;
    tokio::time::timeout(
        Duration::from_secs(2),
        client.read_to_end(&mut response_bytes),
    )
    .await
    .expect("subscribe response should close")
    .expect("subscribe response should be readable");
    run.await
        .expect("subscribe task should join")
        .expect("subscribe endpoint should succeed");
    let response = String::from_utf8(response_bytes).expect("response should be utf-8");

    assert!(response.contains("Content-Type: text/event-stream"));
    assert!(
        response.contains("id: a2a:ctx-1:maestro-task-active-subscribe:task:TASK_STATE_WORKING:")
    );
    assert!(
        response
            .contains("id: a2a:ctx-1:maestro-task-active-subscribe:status:TASK_STATE_COMPLETED:")
    );
    assert!(response.contains("id: a2a:ctx-1:maestro-task-active-subscribe:artifact:artifact-1"));
    assert!(response.contains("event: task"));
    assert!(response.contains("event: statusUpdate"));
    assert!(response.contains("event: artifactUpdate"));
    assert!(response.contains("TASK_STATE_COMPLETED"));
    assert!(response.contains("artifact body"));

    if let Some(previous_timeout) = previous_timeout {
        env::set_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS", previous_timeout);
    } else {
        env::remove_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS");
    }
    if let Some(previous_heartbeat) = previous_heartbeat {
        env::set_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS", previous_heartbeat);
    } else {
        env::remove_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_subscribe_times_out_active_stream() {
    let _guard = ENV_LOCK.lock().await;
    let previous_timeout = env::var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS").ok();
    let previous_heartbeat = env::var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS").ok();
    env::set_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS", "40");
    env::set_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS", "10");
    let state = test_app_state_with_sessions(HashMap::new());
    let initial_task = a2a_task_value(
        "maestro-task-timeout-subscribe",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        vec![a2a_agent_message("ctx-1", "working")],
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-timeout-subscribe".to_string(), initial_task);
    let request = "GET /tasks/maestro-task-timeout-subscribe:subscribe HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        handle_a2a_streaming_endpoint(server, initial, head, state_for_run).await
    });
    let mut response_bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(1),
        client.read_to_end(&mut response_bytes),
    )
    .await
    .expect("subscribe response should close after timeout")
    .expect("subscribe response should be readable");
    run.await
        .expect("subscribe task should join")
        .expect("subscribe endpoint should succeed");
    let response = String::from_utf8(response_bytes).expect("response should be utf-8");

    assert!(response.contains("Content-Type: text/event-stream"));
    assert!(response.contains("TASK_STATE_WORKING"));
    assert!(response.contains(": keep-alive"));

    if let Some(previous_timeout) = previous_timeout {
        env::set_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS", previous_timeout);
    } else {
        env::remove_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS");
    }
    if let Some(previous_heartbeat) = previous_heartbeat {
        env::set_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS", previous_heartbeat);
    } else {
        env::remove_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_subscribe_first_frame_redacts_push_notification_credentials() {
    let _guard = ENV_LOCK.lock().await;
    let previous_timeout = env::var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS").ok();
    let previous_heartbeat = env::var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS").ok();
    env::set_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS", "40");
    env::set_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS", "10");
    let state = test_app_state_with_sessions(HashMap::new());
    let initial_task = a2a_task_value(
        "maestro-task-redacted-subscribe",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        vec![a2a_agent_message("ctx-1", "working")],
        Vec::new(),
        serde_json::json!({
            "pushNotificationConfigs": [{
                "id": "notify-1",
                "taskId": "maestro-task-redacted-subscribe",
                "url": "https://hooks.example/a2a",
                "token": "subscribe-webhook-token",
                "authentication": {
                    "schemes": ["bearer"],
                    "credentials": "subscribe-webhook-bearer"
                }
            }]
        }),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-redacted-subscribe".to_string(), initial_task);
    let request = "GET /tasks/maestro-task-redacted-subscribe:subscribe HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        handle_a2a_streaming_endpoint(server, initial, head, state_for_run).await
    });
    let mut response_bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(1),
        client.read_to_end(&mut response_bytes),
    )
    .await
    .expect("subscribe response should close after timeout")
    .expect("subscribe response should be readable");
    run.await
        .expect("subscribe task should join")
        .expect("subscribe endpoint should succeed");
    let response = String::from_utf8(response_bytes).expect("response should be utf-8");

    assert!(
        !response.contains("subscribe-webhook-token"),
        "subscribe stream leaked the raw push-notification token: {response}"
    );
    assert!(
        !response.contains("subscribe-webhook-bearer"),
        "subscribe stream leaked the raw push-notification credentials: {response}"
    );

    let first_task_frame = response
        .split("\n\n")
        .find(|frame| frame.contains("event: task\n"))
        .expect("subscribe stream should open with a task frame");
    let payload: Value = serde_json::from_str(
        first_task_frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("task frame should carry a data line"),
    )
    .expect("task frame data should be json");
    let config = &payload["task"]["metadata"]["pushNotificationConfigs"][0];
    assert_eq!(config["token"], "<redacted>");
    assert_eq!(config["authentication"]["credentials"], "<redacted>");
    assert_eq!(config["url"], "https://hooks.example/a2a");
    assert_eq!(config["id"], "notify-1");

    let stored_tasks = state.a2a_tasks.lock().await;
    let stored_config =
        &stored_tasks["maestro-task-redacted-subscribe"]["metadata"]["pushNotificationConfigs"][0];
    assert_eq!(stored_config["token"], "subscribe-webhook-token");
    assert_eq!(
        stored_config["authentication"]["credentials"],
        "subscribe-webhook-bearer"
    );
    drop(stored_tasks);

    if let Some(previous_timeout) = previous_timeout {
        env::set_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS", previous_timeout);
    } else {
        env::remove_var("MAESTRO_A2A_SUBSCRIBE_TIMEOUT_MS");
    }
    if let Some(previous_heartbeat) = previous_heartbeat {
        env::set_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS", previous_heartbeat);
    } else {
        env::remove_var("MAESTRO_A2A_SUBSCRIBE_HEARTBEAT_MS");
    }
}

#[tokio::test(flavor = "current_thread")]
pub(crate) async fn a2a_task_subscribe_reconciles_current_task_after_broadcast_lag() {
    let _guard = ENV_LOCK.lock().await;
    let base_state = test_app_state_with_sessions(HashMap::new());
    let (a2a_task_events, _) = broadcast::channel(1);
    let state = AppState {
        a2a_task_events,
        ..base_state
    };
    let initial_task = a2a_task_value(
        "maestro-task-lagged-subscribe",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "working"),
        vec![a2a_agent_message("ctx-1", "working")],
        Vec::new(),
        serde_json::json!({}),
    );
    state
        .a2a_tasks
        .lock()
        .await
        .insert("maestro-task-lagged-subscribe".to_string(), initial_task);
    let request = "GET /tasks/maestro-task-lagged-subscribe:subscribe HTTP/1.1\r\nHost: localhost\r\nx-maestro-api-key: api-key\r\n\r\n";
    let initial = request.as_bytes().to_vec();
    let head = parse_request_head(&initial).expect("request should parse");
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        handle_a2a_streaming_endpoint(server, initial, head, state_for_run).await
    });
    let mut response_bytes = vec![0_u8; 4096];
    let initial_len =
        tokio::time::timeout(Duration::from_secs(2), client.read(&mut response_bytes))
            .await
            .expect("subscribe response should start")
            .expect("subscribe response should be readable");
    response_bytes.truncate(initial_len);

    let intermediate_task = a2a_task_value(
        "maestro-task-lagged-subscribe",
        "ctx-1",
        "TASK_STATE_WORKING",
        a2a_agent_message("ctx-1", "halfway after lag"),
        vec![
            a2a_agent_message("ctx-1", "working"),
            a2a_agent_message("ctx-1", "halfway after lag"),
        ],
        Vec::new(),
        serde_json::json!({}),
    );
    state.a2a_tasks.lock().await.insert(
        "maestro-task-lagged-subscribe".to_string(),
        intermediate_task.clone(),
    );
    publish_a2a_task_update(&state, &intermediate_task).await;
    let terminal_task = a2a_task_value(
        "maestro-task-lagged-subscribe",
        "ctx-1",
        "TASK_STATE_COMPLETED",
        a2a_agent_message("ctx-1", "complete after lag"),
        vec![
            a2a_agent_message("ctx-1", "working"),
            a2a_agent_message("ctx-1", "halfway after lag"),
            a2a_agent_message("ctx-1", "complete after lag"),
        ],
        Vec::new(),
        serde_json::json!({}),
    );
    state.a2a_tasks.lock().await.insert(
        "maestro-task-lagged-subscribe".to_string(),
        terminal_task.clone(),
    );
    publish_a2a_task_update(&state, &terminal_task).await;
    for index in 0..4 {
        let unrelated_task = a2a_task_value(
            &format!("unrelated-task-{index}"),
            "ctx-other",
            "TASK_STATE_WORKING",
            a2a_agent_message("ctx-other", "noise"),
            Vec::new(),
            Vec::new(),
            serde_json::json!({}),
        );
        publish_a2a_task_update(&state, &unrelated_task).await;
    }

    tokio::time::timeout(
        Duration::from_secs(2),
        client.read_to_end(&mut response_bytes),
    )
    .await
    .expect("subscribe response should close after reconciling lag")
    .expect("subscribe response should be readable");
    run.await
        .expect("subscribe task should join")
        .expect("subscribe endpoint should succeed");
    let response = String::from_utf8(response_bytes).expect("response should be utf-8");

    assert!(response.contains("TASK_STATE_COMPLETED"));
    assert!(response.contains("halfway after lag"));
    assert!(response.contains("complete after lag"));
}

// Item 5 (platform A2A push): the `task` branch inserted the caller's task
// wholesale, overwriting any existing record with the same id regardless of
// owner. It now rejects a replace when the stored record belongs to a different
// callback tenant, mirroring the statusUpdate path.
#[tokio::test]
pub(crate) async fn platform_a2a_push_task_branch_rejects_cross_tenant_overwrite() {
    let state = test_app_state_with_sessions(HashMap::new());
    let victim = serde_json::json!({
        "id": "task-1",
        "contextId": "ctx-b",
        "status": { "state": "working" },
        "metadata": { "workspaceId": "workspace-b", "organizationId": "org-b" }
    });
    state
        .a2a_tasks
        .lock()
        .await
        .insert("task-1".to_string(), victim.clone());

    let attacker = crate::a2a::PlatformA2APushServiceAuth {
        organization_id: "org-a".to_string(),
        workspace_id: "workspace-a".to_string(),
    };
    let payload = serde_json::json!({
        "task": {
            "id": "task-1",
            "contextId": "ctx-evil",
            "status": { "state": "completed" },
            "metadata": { "workspaceId": "workspace-a" }
        }
    });
    let result = crate::a2a::record_platform_a2a_push_payload(&state, payload, &attacker).await;
    assert!(
        result.is_err(),
        "cross-tenant task overwrite must be rejected"
    );
    assert_eq!(
        state.a2a_tasks.lock().await.get("task-1").cloned(),
        Some(victim),
        "the victim task must be left unchanged"
    );

    // The owning tenant may still replace its own task.
    let owner = crate::a2a::PlatformA2APushServiceAuth {
        organization_id: "org-b".to_string(),
        workspace_id: "workspace-b".to_string(),
    };
    let update = serde_json::json!({
        "task": {
            "id": "task-1",
            "contextId": "ctx-b2",
            "status": { "state": "completed" },
            "metadata": { "workspaceId": "workspace-b" }
        }
    });
    let ok = crate::a2a::record_platform_a2a_push_payload(&state, update, &owner).await;
    assert!(ok.is_ok(), "same-tenant task update must be accepted");
    assert_eq!(
        state
            .a2a_tasks
            .lock()
            .await
            .get("task-1")
            .and_then(|task| task.get("contextId").and_then(Value::as_str))
            .map(str::to_string),
        Some("ctx-b2".to_string()),
        "same-tenant update must be applied"
    );
}

#[tokio::test]
pub(crate) async fn platform_a2a_push_rejects_claiming_a_legacy_unbound_task() {
    let state = test_app_state_with_sessions(HashMap::new());
    let legacy = serde_json::json!({
        "id": "legacy-unbound",
        "contextId": "ctx-legacy",
        "status": { "state": "working" }
    });
    state
        .a2a_tasks
        .lock()
        .await
        .insert("legacy-unbound".to_string(), legacy.clone());
    let auth = crate::a2a::PlatformA2APushServiceAuth {
        organization_id: "org-a".to_string(),
        workspace_id: "workspace-a".to_string(),
    };
    let result = crate::a2a::record_platform_a2a_push_payload(
        &state,
        serde_json::json!({
            "task": {
                "id": "legacy-unbound",
                "contextId": "ctx-claimed",
                "status": { "state": "completed" }
            }
        }),
        &auth,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        state.a2a_tasks.lock().await.get("legacy-unbound").cloned(),
        Some(legacy)
    );
}

#[tokio::test]
pub(crate) async fn platform_a2a_push_applies_concurrent_status_and_artifacts_without_lost_updates()
{
    let state = test_app_state_with_sessions(HashMap::new());
    let service_auth = crate::a2a::PlatformA2APushServiceAuth {
        organization_id: "org-a".to_string(),
        workspace_id: "workspace-a".to_string(),
    };
    state.a2a_tasks.lock().await.insert(
        "task-concurrent".to_string(),
        serde_json::json!({
            "id": "task-concurrent",
            "contextId": "ctx-concurrent",
            "status": { "state": "TASK_STATE_WORKING" },
            "artifacts": [],
            "metadata": { "workspaceId": "workspace-a", "organizationId": "org-a" }
        }),
    );

    // Queue all callbacks behind the task lock. Tokio's FIFO mutex then forces
    // them to contend at the exact boundary that previously split mutation,
    // tenant validation, and reinsertion across separate critical sections.
    let tasks_guard = state.a2a_tasks.lock().await;
    let mut callbacks = Vec::new();
    for index in 0..8 {
        let state = state.clone();
        let service_auth = service_auth.clone();
        callbacks.push(tokio::spawn(async move {
            crate::a2a::record_platform_a2a_push_payload(
                &state,
                serde_json::json!({
                    "artifactUpdate": {
                        "taskId": "task-concurrent",
                        "artifact": {
                            "artifactId": format!("artifact-{index}"),
                            "parts": [{ "text": format!("result-{index}"), "mediaType": "text/plain" }]
                        }
                    }
                }),
                &service_auth,
            )
            .await
        }));
    }
    let status_state = state.clone();
    let status_auth = service_auth.clone();
    callbacks.push(tokio::spawn(async move {
        crate::a2a::record_platform_a2a_push_payload(
            &status_state,
            serde_json::json!({
                "statusUpdate": {
                    "taskId": "task-concurrent",
                    "status": { "state": "TASK_STATE_COMPLETED" }
                }
            }),
            &status_auth,
        )
        .await
    }));
    tokio::task::yield_now().await;
    drop(tasks_guard);

    for callback in callbacks {
        tokio::time::timeout(Duration::from_secs(5), callback)
            .await
            .expect("publication must not reacquire the task lock")
            .expect("callback task should join")
            .expect("callback should be accepted");
    }

    let tasks = state.a2a_tasks.lock().await;
    let task = tasks
        .get("task-concurrent")
        .expect("concurrent task should remain stored")
        .clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    let artifacts = task["artifacts"]
        .as_array()
        .expect("artifacts should remain an array");
    assert_eq!(artifacts.len(), 8, "no concurrent artifact may be lost");
    for index in 0..8 {
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact["artifactId"] == format!("artifact-{index}")),
            "artifact-{index} should be preserved"
        );
    }
    drop(tasks);

    let histories = state.a2a_task_event_history.lock().await;
    let events = &histories
        .get("task-concurrent")
        .expect("concurrent task should have event history")
        .events;
    assert_eq!(
        events.last().map(|event| &event.task),
        Some(&task),
        "the final published snapshot must match durable task state"
    );
    let mut previous_artifact_count = 0;
    let mut saw_completed = false;
    for event in events {
        let artifact_count = event.task["artifacts"].as_array().map_or(0, Vec::len);
        assert!(
            artifact_count >= previous_artifact_count,
            "published snapshots must not regress accumulated artifacts"
        );
        previous_artifact_count = artifact_count;
        let completed = event.task["status"]["state"] == "TASK_STATE_COMPLETED";
        assert!(
            !saw_completed || completed,
            "published snapshots must not regress a completed status"
        );
        saw_completed |= completed;
    }
}

#[tokio::test]
pub(crate) async fn platform_a2a_push_evicts_terminal_payloads_and_replay_history() {
    let state = test_app_state_with_sessions(HashMap::new());
    let auth = crate::a2a::PlatformA2APushServiceAuth {
        organization_id: "org-retention".into(),
        workspace_id: "workspace-retention".into(),
    };
    for index in 0..A2A_TERMINAL_TASK_STORE_LIMIT + 16 {
        crate::a2a::record_platform_a2a_push_payload(&state, serde_json::json!({
            "task": {"id":format!("callback-{index:04}"), "status":{"state":"TASK_STATE_COMPLETED", "timestamp":format!("{index:04}")}, "artifacts":[{"parts":[{"text":"x".repeat(4096)}]}]}
        }), &auth).await.unwrap();
    }
    let tasks = state.a2a_tasks.lock().await;
    let histories = state.a2a_task_event_history.lock().await;
    assert_eq!(tasks.len(), A2A_TERMINAL_TASK_STORE_LIMIT);
    assert_eq!(histories.len(), tasks.len());
    assert!(!tasks.contains_key("callback-0000"));
    assert!(!histories.contains_key("callback-0000"));
}
