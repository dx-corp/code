#[allow(unused_imports)]
use super::*;

#[tokio::test]
pub(crate) async fn codex_headless_bridge_streams_tool_events_over_websocket() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-headless-ws-tools");
    let cli_path = root.path().join("cli.js");
    fs::write(
            &cli_path,
            r#"const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, terminal: false });
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    send({
      type: "tool_call",
      call_id: "collab-call-ws",
      tool: "codex.subagent.spawnAgent",
      args: {
        codexTool: "spawnAgent",
        receiverThreadIds: ["child-thread-ws"],
        prompt: "Inspect the websocket bridge"
      }
    });
    send({
      type: "tool_end",
      call_id: "collab-call-ws",
      success: true
    });
    send({ type: "response_start", response_id: "response-1" });
    send({ type: "response_chunk", response_id: "response-1", content: "websocket output", is_thinking: false });
    send({ type: "response_end", response_id: "response-1", duration_ms: 1 });
    send({ type: "turn_completed", response_id: "response-1" });
  } else if (msg.type === "shutdown") {
    process.exit(0);
  }
});
"#,
        )
        .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "5000");

    let state = test_app_state_with_sessions(HashMap::new());
    let (mut client, server) = tcp_stream_pair().await;
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        let mut server = server;
        handle_codex_app_server_chat_transport(
            &mut server,
            &state_for_run,
            Some("session-ws"),
            "gpt-5.5",
            "hello",
            &[],
            CodexBridgeTransport::WebSocket,
        )
        .await
    });
    let mut bytes = Vec::new();
    // Closing depends on a cold Node fixture starting and shutting down. Keep
    // this as a bounded wait, but do not turn loaded CI scheduling into a
    // protocol failure.
    tokio::time::timeout(Duration::from_secs(10), client.read_to_end(&mut bytes))
        .await
        .expect("WebSocket stream should close")
        .expect("WebSocket frames should be readable");
    let result = run.await.expect("headless websocket run should join");

    if let Some(previous) = previous_cli {
        env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_CLI");
    }
    if let Some(previous) = previous_timeout {
        env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    }

    result.expect("headless WebSocket bridge should complete");
    let events = server_websocket_json_values(&bytes);
    assert!(events.iter().any(|event| event["type"] == "agent_start"));
    let start = events
        .iter()
        .find(|event| event["type"] == "tool_execution_start")
        .expect("tool start should stream over WebSocket");
    let end = events
        .iter()
        .find(|event| event["type"] == "tool_execution_end")
        .expect("tool end should stream over WebSocket");
    assert_eq!(start["toolCallId"], "collab-call-ws");
    assert_eq!(start["toolName"], "codex.subagent.spawnAgent");
    assert_eq!(start["args"]["receiverThreadIds"][0], "child-thread-ws");
    assert_eq!(
        start["args"]["childRunIds"][0],
        "codex-thread:child-thread-ws"
    );
    assert_eq!(
        start["args"]["codexWorkGraph"]["childRuns"][0]["childRunId"],
        "codex-thread:child-thread-ws"
    );
    assert_eq!(end["toolCallId"], "collab-call-ws");
    assert_eq!(end["isError"], false);
    assert!(events.iter().any(|event| {
        event["type"] == "message_update" && event["message"]["content"] == "websocket output"
    }));
    assert!(events.iter().any(|event| event["type"] == "done"));
}

pub(crate) fn server_websocket_json_values(bytes: &[u8]) -> Vec<Value> {
    let mut values = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        assert!(bytes.len() >= cursor + 2, "incomplete WebSocket frame");
        let opcode = bytes[cursor] & 0x0f;
        if opcode == 0x8 {
            break;
        }
        assert_eq!(opcode, 0x1, "expected text WebSocket frame");
        assert_eq!(
            bytes[cursor + 1] & 0x80,
            0,
            "server WebSocket frames should not be masked"
        );
        let mut offset = cursor + 2;
        let mut len = (bytes[cursor + 1] & 0x7f) as usize;
        if len == 126 {
            assert!(bytes.len() >= offset + 2, "incomplete extended length");
            len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
            offset += 2;
        } else if len == 127 {
            assert!(bytes.len() >= offset + 8, "incomplete extended length");
            len = u64::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]) as usize;
            offset += 8;
        }
        assert!(bytes.len() >= offset + len, "incomplete WebSocket payload");
        values.push(
            serde_json::from_slice(&bytes[offset..offset + len])
                .expect("WebSocket payload should be JSON"),
        );
        cursor = offset + len;
    }
    values
}

#[test]
pub(crate) fn read_only_tenant_jwt_can_subscribe_only_within_exact_task_tenant() {
    let _guard = ENV_LOCK.blocking_lock();
    let names: &[&str] = &[
        "MAESTRO_JWT_SECRET",
        "MAESTRO_JWT_ALG",
        "MAESTRO_JWT_AUD",
        "MAESTRO_JWT_ISS",
        "MAESTRO_PROFILE",
        "MAESTRO_WEB_PROFILE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_HOSTED_RUNNER",
        "MAESTRO_RUNNER_KIND",
    ];
    let snapshot = snapshot_env(names);
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    env::set_var("MAESTRO_JWT_ALG", "HS256");
    env::set_var("MAESTRO_JWT_AUD", "evalops.maestro");
    env::set_var("MAESTRO_JWT_ISS", "evalops.platform");
    for signal in [
        "MAESTRO_PROFILE",
        "MAESTRO_WEB_PROFILE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_HOSTED_RUNNER",
        "MAESTRO_RUNNER_KIND",
    ] {
        env::remove_var(signal);
    }

    let token = hs256_signed_claims(
        b"shared-secret",
        serde_json::json!({
            "sub": "subscribe-user",
            "organization_id": "org-a",
            "workspace_id": "workspace-a",
            "aud": "evalops.maestro",
            "iss": "evalops.platform",
            "exp": now_secs() + 60,
        }),
    );
    let subscribe_head = auth_head("POST", "/tasks/task-a:subscribe", &token);
    let mut remote_config = auth_test_config();
    remote_config.listen_host = "0.0.0.0".to_string();

    let auth = authorized_context(&subscribe_head, &remote_config)
        .expect("tenant-bound read-only JWT should authorize a task subscription");
    assert!(auth.scopes.is_empty());
    for path in ["/api/config", "/tasks/task-a:cancel"] {
        assert!(
            authorize(&auth_head("POST", path, &token), &remote_config).is_err(),
            "read-only tenant JWT must not mutate {path}"
        );
    }

    let task = a2a_task_value(
        "task-a",
        "context-a",
        "TASK_STATE_WORKING",
        a2a_agent_message("context-a", "working"),
        Vec::new(),
        Vec::new(),
        serde_json::json!({
            "ownerSubject": "subscribe-user",
            "organizationId": "org-a",
            "workspaceId": "workspace-a",
        }),
    );
    assert!(a2a_task_visible_to_auth(&task, &auth));

    let mut wrong_subject = auth.clone();
    wrong_subject.subject = Some("other-user".to_string());
    assert!(!a2a_task_visible_to_auth(&task, &wrong_subject));
    let mut wrong_organization = auth.clone();
    wrong_organization.organization_id = Some("org-b".to_string());
    assert!(!a2a_task_visible_to_auth(&task, &wrong_organization));
    let mut wrong_workspace = auth.clone();
    wrong_workspace.workspace_id = Some("workspace-b".to_string());
    assert!(!a2a_task_visible_to_auth(&task, &wrong_workspace));

    restore_env(snapshot);
}

#[test]
pub(crate) fn detects_chat_websocket_route_separately_from_sse() {
    let head = parse_request_head(b"GET /api/chat/ws HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("request should parse");

    assert!(is_chat_websocket_endpoint(&head));
    assert!(!is_chat_endpoint(&head));
}

#[test]
pub(crate) fn computes_websocket_accept_key() {
    assert_eq!(
        websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[test]
pub(crate) fn parses_masked_websocket_text_frame() {
    let payload = br#"{"messages":[]}"#;
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % mask.len()]);
    }

    let parsed = try_parse_websocket_text_message(&mut frame)
        .expect("frame should parse")
        .expect("frame should be complete");

    assert_eq!(parsed, payload);
    assert!(frame.is_empty());
}

#[test]
pub(crate) fn parses_fragmented_masked_websocket_text_frame() {
    let first = br#"{"messages":"#;
    let second = br#"[]}"#;
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut frame = vec![0x01, 0x80 | first.len() as u8];
    frame.extend_from_slice(&mask);
    for (index, byte) in first.iter().enumerate() {
        frame.push(byte ^ mask[index % mask.len()]);
    }
    frame.extend_from_slice(&[0x80, 0x80 | second.len() as u8]);
    frame.extend_from_slice(&mask);
    for (index, byte) in second.iter().enumerate() {
        frame.push(byte ^ mask[index % mask.len()]);
    }

    let parsed = try_parse_websocket_text_message(&mut frame)
        .expect("fragmented frame should parse")
        .expect("fragmented frame should be complete");

    assert_eq!(parsed, br#"{"messages":[]}"#);
    assert!(frame.is_empty());
}
