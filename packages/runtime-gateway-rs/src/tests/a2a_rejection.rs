use super::*;

#[tokio::test(flavor = "current_thread")]
async fn unsupported_a2a_subagent_lane_rejects_before_task_persistence() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
    let subagent_request = valid_browser_qa_capsule();
    let body = serde_json::json!({
        "message": {
            "messageId": "msg-unsupported-lane",
            "contextId": "ctx-unsupported-lane",
            "role": "ROLE_USER",
            "parts": [{
                "text": "run browser QA",
                "mediaType": "text/plain"
            }]
        },
        "configuration": {
            "returnImmediately": true
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
    assert_eq!(state.a2a_tasks.lock().await.len(), 0);
    assert_eq!(state.a2a_cancel_senders.lock().await.len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn unexecutable_a2a_capsule_rejects_before_task_persistence() {
    // Authentication reads the hosted-profile environment changed by sibling tests.
    let _guard = ENV_LOCK.lock().await;
    let mut subagent_request = valid_code_writer_capsule();
    subagent_request["capsule"]["acceptanceChecks"] = serde_json::json!(["cat /etc/passwd"]);
    let body = serde_json::json!({
        "message": {
            "messageId": "msg-unexecutable",
            "contextId": "ctx-unexecutable",
            "role": "ROLE_USER",
            "parts": [{
                "text": "run an unsafe check",
                "mediaType": "text/plain"
            }]
        },
        "configuration": {
            "returnImmediately": true
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
    assert_eq!(state.a2a_tasks.lock().await.len(), 0);
    assert_eq!(state.a2a_cancel_senders.lock().await.len(), 0);
}
