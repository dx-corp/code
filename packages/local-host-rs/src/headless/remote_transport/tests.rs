use super::*;

#[test]
fn accepts_coalesced_response_completion_once_at_shared_cursor() {
    let mut pending = None;
    let chunk = FromAgentMessage::ResponseChunk {
        response_id: "response".into(),
        content: "complete answer".into(),
        is_thinking: false,
    };
    let end = FromAgentMessage::ResponseEnd {
        response_id: "response".into(),
        usage: None,
        tools_summary: None,
        duration_ms: None,
        ttft_ms: None,
    };

    assert!(accepts_remote_message_cursor(4, 5, &chunk, &mut pending));
    assert!(accepts_remote_message_cursor(5, 5, &end, &mut pending));
    assert!(!accepts_remote_message_cursor(5, 5, &end, &mut pending));
}
use crate::headless::HEADLESS_PROTOCOL_VERSION;
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};

#[test]
fn response_retries_have_stable_idempotency_keys() {
    let response = ToAgentMessage::ServerRequestResponse {
        request_id: "request-1".into(),
        request_type: ServerRequestType::Approval,
        approved: Some(true),
        result: None,
        content: None,
        is_error: None,
        decision_action: None,
        reason: Some("approved".into()),
    };
    let key = super::response_idempotency_key(&response).expect("response key");
    assert_eq!(key, super::response_idempotency_key(&response).unwrap());
    assert!(key.starts_with("maestro-response-"));
    let changed_payload = ToAgentMessage::ServerRequestResponse {
        request_id: "request-1".into(),
        request_type: ServerRequestType::Approval,
        approved: Some(false),
        result: None,
        content: None,
        is_error: None,
        decision_action: None,
        reason: Some("changed decision payload".into()),
    };
    assert_eq!(
        key,
        super::response_idempotency_key(&changed_payload).expect("same request identity key")
    );
    let different_request = ToAgentMessage::ServerRequestResponse {
        request_id: "request-2".into(),
        request_type: ServerRequestType::Approval,
        approved: Some(true),
        result: None,
        content: None,
        is_error: None,
        decision_action: None,
        reason: Some("approved".into()),
    };
    assert_ne!(
        key,
        super::response_idempotency_key(&different_request).expect("different request key")
    );
    assert!(
        super::response_idempotency_key(&ToAgentMessage::Prompt {
            content: "same payload must remain repeatable".into(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .is_none()
    );
}

async fn read_http_request(
    socket: &mut TcpStream,
) -> Option<(String, Vec<(String, String)>, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];

    loop {
        let bytes_read = socket.read(&mut chunk).await.ok()?;
        if bytes_read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..bytes_read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")?;
    let header_bytes = &buffer[..header_end];
    let header_text = String::from_utf8_lossy(header_bytes);
    let request_line = header_text.lines().next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let headers = header_text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find_map(|(name, value)| {
            if name == "content-length" {
                value.parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let mut body = buffer[(header_end + 4)..].to_vec();
    while body.len() < content_length {
        let bytes_read = socket.read(&mut chunk).await.ok()?;
        if bytes_read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..bytes_read]);
    }

    Some((
        path,
        headers,
        String::from_utf8_lossy(&body[..content_length]).to_string(),
    ))
}

async fn write_http_response(
    socket: &mut TcpStream,
    status_line: &str,
    content_type: &str,
    body: &str,
) {
    let response = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn wait_for_posted_bodies_len(
    posted_bodies: &Arc<Mutex<Vec<String>>>,
    expected_len: usize,
) -> Vec<String> {
    for _ in 0..50 {
        let posted = posted_bodies.lock().await.clone();
        if posted.len() >= expected_len {
            return posted;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    posted_bodies.lock().await.clone()
}

async fn spawn_remote_headless_server(
    snapshot_json: String,
    sse_events: Vec<String>,
) -> (
    std::net::SocketAddr,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<Vec<(String, String)>>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posted_bodies = Arc::new(Mutex::new(Vec::new()));
    let request_paths = Arc::new(Mutex::new(Vec::new()));
    let request_headers = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(VecDeque::from(sse_events)));

    tokio::spawn({
        let posted_bodies = Arc::clone(&posted_bodies);
        let request_paths = Arc::clone(&request_paths);
        let request_headers = Arc::clone(&request_headers);
        let events = Arc::clone(&events);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let posted_bodies = Arc::clone(&posted_bodies);
                let request_paths = Arc::clone(&request_paths);
                let request_headers = Arc::clone(&request_headers);
                let events = Arc::clone(&events);
                let snapshot_json = snapshot_json.clone();

                tokio::spawn(async move {
                    let Some((path, headers, body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    request_paths.lock().await.push(path.clone());
                    request_headers.lock().await.push(headers);

                    if path == "/api/headless/connections" {
                        let body = serde_json::json!({
                            "session_id": "sess_remote",
                            "connection_id": "conn_remote",
                            "connection_capability": "cap_remote",
                            "controller_connection_id": "conn_remote",
                            "lease_expires_at": "2026-04-02T00:00:15Z",
                            "heartbeat_interval_ms": 15000,
                            "snapshot": serde_json::from_str::<serde_json::Value>(&snapshot_json)
                                .expect("valid snapshot json"),
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &body,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        let snapshot = serde_json::from_str::<serde_json::Value>(&snapshot_json)
                            .expect("valid snapshot json");
                        let controller_pending_events = snapshot
                            .get("controller_pending_events")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!([]));
                        let body = serde_json::json!({
                            "connection_id": "conn_remote",
                            "connection_capability": "cap_remote",
                            "subscription_id": "sub_remote",
                            "controller_connection_id": "conn_remote",
                            "lease_expires_at": "2026-04-02T00:00:15Z",
                            "heartbeat_interval_ms": 15000,
                            "controller_pending_events": controller_pending_events,
                            "snapshot": snapshot,
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &body,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/unsubscribe")
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect")
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"connection_id":"conn_remote","controller_lease_granted":true,"controller_connection_id":"conn_remote","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") {
                        posted_bodies.lock().await.push(body);
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                        if socket.write_all(headers.as_bytes()).await.is_err() {
                            return;
                        }
                        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                        while let Some(event) = events.lock().await.pop_front() {
                            let _ = tx.send(event);
                        }
                        while let Some(event) = rx.recv().await {
                            let payload = format!("data: {event}\n\n");
                            if socket.write_all(payload.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        return;
                    }

                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "text/plain",
                        "not found",
                    )
                    .await;
                });
            }
        }
    });

    (addr, posted_bodies, request_paths, request_headers)
}

#[test]
fn remote_runtime_state_snapshot_maps_into_agent_state() {
    let snapshot = RemoteRuntimeStateSnapshot {
        protocol_version: Some("2026-03-30".to_string()),
        client_protocol_version: Some("2026-03-30".to_string()),
        controller_binding_version: None,
        controller_binding_sha256: None,
        client_info: Some(ClientInfo {
            name: "maestro-tui-rs".to_string(),
            version: Some("0.1.0".to_string()),
        }),
        capabilities: Some(ClientCapabilities {
            server_requests: Some(vec![
                crate::headless::ServerRequestType::Approval,
                crate::headless::ServerRequestType::ClientTool,
                crate::headless::ServerRequestType::UserInput,
                crate::headless::ServerRequestType::ToolRetry,
            ]),
            utility_operations: Some(vec![crate::headless::UtilityOperation::CommandExec]),
            raw_agent_events: Some(true),
            transcript_grade: None,
            governed_code_mode: None,
        }),
        server_capabilities: Some(crate::headless::ServerCapabilities {
            native_tools: vec![crate::headless::NativeToolCapability {
                name: "bash".to_string(),
                requires_approval: true,
                version: Some("current".to_string()),
            }],
            ..crate::headless::ServerCapabilities::default()
        }),
        opt_out_notifications: Some(vec!["status".to_string()]),
        connection_role: Some(ConnectionRole::Controller),
        connection_count: 1,
        subscriber_count: 2,
        controller_subscription_id: Some("sub_remote".to_string()),
        controller_connection_id: Some("conn_remote".to_string()),
        connections: vec![ConnectionState {
            connection_id: "conn_remote".to_string(),
            role: ConnectionRole::Controller,
            client_protocol_version: Some("2026-03-30".to_string()),
            client_info: Some(ClientInfo {
                name: "maestro-tui-rs".to_string(),
                version: Some("0.1.0".to_string()),
            }),
            capabilities: Some(ClientCapabilities {
                server_requests: Some(vec![
                    crate::headless::ServerRequestType::Approval,
                    crate::headless::ServerRequestType::ClientTool,
                    crate::headless::ServerRequestType::UserInput,
                    crate::headless::ServerRequestType::ToolRetry,
                ]),
                utility_operations: Some(vec![crate::headless::UtilityOperation::CommandExec]),
                raw_agent_events: Some(true),
                transcript_grade: None,
                governed_code_mode: None,
            }),
            opt_out_notifications: Some(vec!["status".to_string()]),
            subscription_count: 1,
            attached_subscription_count: 1,
            controller_lease_granted: true,
            lease_expires_at: Some("2026-04-02T00:00:15Z".to_string()),
        }],
        model: Some("gpt-5.6".to_string()),
        provider: Some("openai".to_string()),
        session_id: Some("session-1".to_string()),
        cwd: Some("/tmp/project".to_string()),
        git_branch: Some("main".to_string()),
        current_response: Some(StreamingResponse {
            response_id: "resp-1".to_string(),
            text: "Hello".to_string(),
            thinking: "Thinking".to_string(),
            usage: None,
        }),
        pending_approvals: vec![PendingApproval {
            call_id: "call-1".to_string(),
            tool_execution_id: None,
            request_id: None,
            tool: "bash".to_string(),
            args: serde_json::json!({"cmd": "ls"}),
            started_at_ms: Some(1_771_000_000_000),
        }],
        pending_client_tools: vec![PendingApproval {
            call_id: "call-client".to_string(),
            tool_execution_id: None,
            request_id: None,
            tool: "artifacts".to_string(),
            args: serde_json::json!({"command": "create", "filename": "report.txt"}),
            started_at_ms: None,
        }],
        governed_client_tool_bindings: HashMap::new(),
        pending_user_inputs: vec![PendingApproval {
            call_id: "call-user-input".to_string(),
            tool_execution_id: None,
            request_id: None,
            tool: "ask_user".to_string(),
            args: serde_json::json!({
                "questions": [{
                    "header": "Stack",
                    "question": "Which schema library should we use?",
                    "options": [{
                        "label": "Zod",
                        "description": "Use Zod schemas"
                    }]
                }]
            }),
            started_at_ms: None,
        }],
        pending_tool_retries: vec![PendingApproval {
            call_id: "call-retry".to_string(),
            tool_execution_id: None,
            request_id: Some("req-retry".to_string()),
            tool: "bash".to_string(),
            args: serde_json::json!({
                "tool_call_id": "call-retry",
                "args": {"cmd": "ls"},
                "error_message": "command failed",
                "attempt": 1
            }),
            started_at_ms: None,
        }],
        tracked_tools: vec![PendingApproval {
            call_id: "call-2".to_string(),
            tool_execution_id: None,
            request_id: None,
            tool: "read".to_string(),
            args: serde_json::json!({"path": "package.json"}),
            started_at_ms: None,
        }],
        active_tools: vec![RemoteActiveToolState {
            call_id: "call-2".to_string(),
            tool: "read".to_string(),
            output: "partial".to_string(),
        }],
        codex_subagent_edges: vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-remote".to_string()),
            spawn_tool_execution_id: None,
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-remote".to_string()),
            thread_id: Some("child-thread-remote".to_string()),
            operation: "spawn_agent".to_string(),
            status: "waiting_for_restore".to_string(),
        }],
        active_utility_commands: vec![RemoteActiveUtilityCommandState {
            command_id: "cmd-1".to_string(),
            command: "echo hi".to_string(),
            cwd: Some("/tmp/project".to_string()),
            shell_mode: UtilityCommandShellMode::Direct,
            terminal_mode: UtilityCommandTerminalMode::Pipe,
            pid: Some(1234),
            columns: None,
            rows: None,
            owner_connection_id: Some("conn-1".to_string()),
            output: "hi\n".to_string(),
        }],
        active_file_watches: vec![RemoteActiveFileWatchState {
            watch_id: "watch-1".to_string(),
            root_dir: "/tmp/project".to_string(),
            include_patterns: Some(vec!["src/**".to_string()]),
            exclude_patterns: Some(vec!["dist/**".to_string()]),
            debounce_ms: 100,
            owner_connection_id: Some("conn-1".to_string()),
        }],
        last_error: Some("boom".to_string()),
        last_error_type: Some(HeadlessErrorType::Tool),
        provider_error_kind: Some(maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure),
        last_status: Some("Working".to_string()),
        last_response_duration_ms: Some(42),
        last_ttft_ms: Some(7),
        is_ready: true,
        is_responding: true,
    };

    let state = snapshot.into_agent_state();
    assert_eq!(state.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(state.provider.as_deref(), Some("openai"));
    assert_eq!(state.client_protocol_version.as_deref(), Some("2026-03-30"));
    assert_eq!(
        state.client_info.as_ref().map(|info| info.name.as_str()),
        Some("maestro-tui-rs")
    );
    assert_eq!(
        state
            .server_capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.native_tools.first())
            .map(|tool| tool.name.as_str()),
        Some("bash")
    );
    assert_eq!(
        state
            .opt_out_notifications
            .as_ref()
            .map(|items| items.len()),
        Some(1)
    );
    assert_eq!(state.pending_approvals.len(), 1);
    assert_eq!(state.pending_client_tools.len(), 1);
    assert_eq!(state.pending_user_inputs.len(), 1);
    assert_eq!(state.pending_tool_retries.len(), 1);
    assert_eq!(state.subscriber_count, 2);
    assert_eq!(
        state.controller_subscription_id.as_deref(),
        Some("sub_remote")
    );
    assert_eq!(state.tracked_tools.len(), 1);
    assert_eq!(state.active_tools.len(), 1);
    assert_eq!(state.codex_subagent_edges.len(), 1);
    assert_eq!(
        state.codex_subagent_edges[0].child_run_id.as_deref(),
        Some("agent-run-child-remote")
    );
    assert_eq!(state.active_utility_commands.len(), 1);
    assert_eq!(state.active_file_watches.len(), 1);
    assert_eq!(
        state
            .active_utility_commands
            .get("cmd-1")
            .and_then(|command| command.owner_connection_id.as_deref()),
        Some("conn-1")
    );
    assert_eq!(
        state
            .active_file_watches
            .get("watch-1")
            .and_then(|watch| watch.owner_connection_id.as_deref()),
        Some("conn-1")
    );
    assert_eq!(state.last_error.as_deref(), Some("boom"));
    assert_eq!(
        state.provider_error_kind,
        Some(maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure)
    );
    assert_eq!(state.last_status.as_deref(), Some("Working"));
    assert!(state.is_ready);
    assert!(state.is_responding);
}

#[test]
fn remote_reset_snapshot_preserves_each_provider_error_kind() {
    for (encoded, expected) in [
        (
            "transient_protocol",
            maestro_ai::ProviderStreamErrorKind::TransientProtocol,
        ),
        (
            "output_token_exhaustion",
            maestro_ai::ProviderStreamErrorKind::OutputTokenExhaustion,
        ),
        (
            "provider_declared_failure",
            maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
        ),
    ] {
        let snapshot: RemoteRuntimeStateSnapshot = serde_json::from_value(serde_json::json!({
            "provider_error_kind": encoded,
            "last_error": "terminal provider failure",
        }))
        .expect("remote reset snapshot");
        let state = snapshot.into_agent_state();
        assert_eq!(state.provider_error_kind, Some(expected));
    }
}

#[test]
fn remote_connection_create_request_serializes_client_tool_flags() {
    let request = RemoteConnectionCreateRequest {
        protocol_version: Some("2026-03-30".to_string()),
        client_info: Some(ClientInfo {
            name: "maestro-tui-rs".to_string(),
            version: Some("0.1.0".to_string()),
        }),
        session_id: Some("sess_remote".to_string()),
        connection_id: Some("conn_remote".to_string()),
        connection_capability: Some("cap_remote".to_string()),
        connection_capability_required: true,
        model: Some("gpt-5.6".to_string()),
        thinking_level: Some(ThinkingLevel::Low),
        approval_mode: Some(ApprovalMode::Prompt),
        enable_client_tools: true,
        capabilities: Some(RemoteClientCapabilities {
            server_requests: vec!["approval", "client_tool", "user_input", "tool_retry"],
            utility_operations: vec!["command_exec"],
            raw_agent_events: true,
            transcript_grade: crate::transcript::TranscriptGrade::Delta,
        }),
        opt_out_notifications: vec!["status".to_string()],
        client: Some("vscode".to_string()),
        role: Some("controller".to_string()),
        take_control: true,
    };

    let json = serde_json::to_value(request).expect("serialize request");
    assert_eq!(json["protocolVersion"], "2026-03-30");
    assert_eq!(json["clientInfo"]["name"], "maestro-tui-rs");
    assert_eq!(json["clientInfo"]["version"], "0.1.0");
    assert_eq!(json["sessionId"], "sess_remote");
    assert_eq!(json["connectionId"], "conn_remote");
    assert_eq!(json["connectionCapability"], "cap_remote");
    assert_eq!(json["connectionCapabilityRequired"], true);
    assert_eq!(json["model"], "gpt-5.6");
    assert_eq!(json["thinkingLevel"], "low");
    assert_eq!(json["approvalMode"], "prompt");
    assert_eq!(json["enableClientTools"], true);
    assert_eq!(json["capabilities"]["serverRequests"][0], "approval");
    assert_eq!(json["capabilities"]["serverRequests"][1], "client_tool");
    assert_eq!(json["capabilities"]["serverRequests"][2], "user_input");
    assert_eq!(json["capabilities"]["serverRequests"][3], "tool_retry");
    assert_eq!(json["capabilities"]["rawAgentEvents"], true);
    assert_eq!(json["capabilities"]["transcriptGrade"], "delta");
    assert_eq!(json["optOutNotifications"][0], "status");
    assert_eq!(json["client"], "vscode");
    assert_eq!(json["role"], "controller");
    assert_eq!(json["takeControl"], true);
}

#[test]
fn remote_http_bootstrap_carries_the_negotiated_transcript_grade() {
    let request = build_remote_connection_create_request(
        &RemoteTransportConfig {
            role: Some("viewer".to_string()),
            enable_raw_agent_events: false,
            ..RemoteTransportConfig::default()
        },
        None,
    );
    let json = serde_json::to_value(request).expect("serialize request");

    assert_eq!(json["capabilities"]["transcriptGrade"], "block");
}

#[test]
fn remote_http_bootstrap_carries_configured_reconnect_capability() {
    let request = build_remote_connection_create_request(
        &RemoteTransportConfig {
            connection_id: Some("conn_remote".to_string()),
            headers: HashMap::from([(
                "X-Maestro-Headless-Connection-Capability".to_string(),
                "cap_remote".to_string(),
            )]),
            ..RemoteTransportConfig::default()
        },
        Some("conn_remote".to_string()),
    );
    let json = serde_json::to_value(request).expect("serialize request");

    assert_eq!(json["connectionId"], "conn_remote");
    assert_eq!(json["connectionCapability"], "cap_remote");
}

#[test]
fn remote_session_subscribe_request_serializes_opt_out_notifications() {
    let request = RemoteSessionSubscribeRequest {
        connection_id: Some("conn_remote".to_string()),
        connection_capability: Some("cap_remote".to_string()),
        connection_capability_required: true,
        protocol_version: Some("2026-08-01".to_string()),
        client_info: Some(ClientInfo {
            name: "maestro-tui-rs".to_string(),
            version: Some("0.1.0".to_string()),
        }),
        capabilities: Some(RemoteClientCapabilities {
            server_requests: vec!["approval", "client_tool", "user_input"],
            utility_operations: vec!["command_exec", "file_read", "file_watch"],
            raw_agent_events: true,
            transcript_grade: crate::transcript::TranscriptGrade::Block,
        }),
        role: Some("viewer".to_string()),
        opt_out_notifications: vec!["status".to_string(), "heartbeat".to_string()],
        take_control: false,
    };

    let json = serde_json::to_value(request).expect("serialize request");
    assert_eq!(json["connectionId"], "conn_remote");
    assert_eq!(json["connectionCapability"], "cap_remote");
    assert_eq!(json["connectionCapabilityRequired"], true);
    assert_eq!(json["role"], "viewer");
    assert_eq!(json["capabilities"]["rawAgentEvents"], true);
    assert_eq!(json["capabilities"]["transcriptGrade"], "block");
    assert_eq!(json["optOutNotifications"][0], "status");
    assert_eq!(json["optOutNotifications"][1], "heartbeat");
}

#[test]
fn remote_lifecycle_requests_carry_bound_connection_authority() {
    let request =
        connection_lifecycle_request("conn_remote", Some("cap_remote"), Some("sub_remote"));

    assert_eq!(request["connectionId"], "conn_remote");
    assert_eq!(request["subscriptionId"], "sub_remote");
    assert_eq!(request["connectionCapability"], "cap_remote");
}

#[test]
fn remote_controller_omits_unsupported_user_input_request() {
    let message = build_remote_hello_message(&RemoteTransportConfig {
        enable_client_tools: true,
        ..RemoteTransportConfig::default()
    });

    let ToAgentMessage::Hello {
        capabilities: Some(capabilities),
        ..
    } = message
    else {
        panic!("expected hello message");
    };

    assert_eq!(
        capabilities.server_requests,
        Some(vec![
            ServerRequestType::Approval,
            ServerRequestType::ClientTool,
            ServerRequestType::ToolRetry,
        ])
    );
    assert_eq!(
        build_remote_server_requests(&RemoteTransportConfig {
            enable_client_tools: true,
            ..RemoteTransportConfig::default()
        }),
        vec!["approval", "client_tool", "tool_retry"]
    );
}

#[test]
fn remote_hello_message_omits_interactive_server_requests_for_viewer() {
    let message = build_remote_hello_message(&RemoteTransportConfig {
        enable_client_tools: true,
        role: Some("viewer".to_string()),
        ..RemoteTransportConfig::default()
    });

    let ToAgentMessage::Hello {
        capabilities: Some(capabilities),
        ..
    } = message
    else {
        panic!("expected hello message");
    };

    assert_eq!(
        capabilities.server_requests,
        Some(vec![
            ServerRequestType::Approval,
            ServerRequestType::ClientTool
        ])
    );
}

#[tokio::test]
async fn remote_transport_reuses_fallback_authority_when_response_is_lost() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let bootstrap_attempt = Arc::new(AtomicUsize::new(0));
    let connection_requests = Arc::new(Mutex::new(Vec::new()));
    let fallback_authority = Arc::new(Mutex::new(None::<(String, String)>));

    tokio::spawn({
        let bootstrap_attempt = Arc::clone(&bootstrap_attempt);
        let connection_requests = Arc::clone(&connection_requests);
        let fallback_authority = Arc::clone(&fallback_authority);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };

                tokio::spawn({
                    let bootstrap_attempt = Arc::clone(&bootstrap_attempt);
                    let connection_requests = Arc::clone(&connection_requests);
                    let fallback_authority = Arc::clone(&fallback_authority);
                    async move {
                        let Some((path, _headers, body)) = read_http_request(&mut socket).await
                        else {
                            return;
                        };

                        if path == "/api/headless/connections" {
                            let attempt = bootstrap_attempt.fetch_add(1, Ordering::SeqCst) + 1;
                            connection_requests.lock().await.push(body.clone());
                            let request = serde_json::from_str::<serde_json::Value>(&body)
                                .expect("valid connection request");
                            assert!(request.get("takeControl").is_none());
                            if attempt == 1 {
                                assert_eq!(
                                    request
                                        .get("connectionId")
                                        .and_then(serde_json::Value::as_str),
                                    Some("conn_stale")
                                );
                                assert_eq!(
                                    request
                                        .get("connectionCapability")
                                        .and_then(serde_json::Value::as_str),
                                    Some("cap_stale")
                                );
                                write_http_response(
                                    &mut socket,
                                    "HTTP/1.1 404 Not Found",
                                    "application/json",
                                    r#"{"error":"Headless connection not found"}"#,
                                )
                                .await;
                                return;
                            }
                            let connection_id = request
                                .get("connectionId")
                                .and_then(serde_json::Value::as_str)
                                .expect("fresh fallback connection id")
                                .to_string();
                            let connection_capability = request
                                .get("connectionCapability")
                                .and_then(serde_json::Value::as_str)
                                .expect("fresh fallback connection capability")
                                .to_string();
                            assert_ne!(connection_id, "conn_stale");
                            assert_ne!(connection_capability, "cap_stale");
                            if attempt == 2 {
                                *fallback_authority.lock().await =
                                    Some((connection_id, connection_capability));
                                let _ = socket.shutdown().await;
                                return;
                            }
                            assert_eq!(
                                fallback_authority.lock().await.as_ref(),
                                Some(&(connection_id.clone(), connection_capability))
                            );
                            let body = serde_json::json!({
                                "session_id": "sess_remote",
                                "connection_id": connection_id,
                            })
                            .to_string();
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                &body,
                            )
                            .await;
                            return;
                        }

                        if path.starts_with("/api/headless/sessions/")
                            && path.ends_with("/subscribe")
                        {
                            let request = serde_json::from_str::<serde_json::Value>(&body)
                                .expect("valid subscribe request");
                            let authority = fallback_authority
                                .lock()
                                .await
                                .clone()
                                .expect("accepted fallback authority");
                            assert_eq!(request["connectionId"], authority.0);
                            assert_eq!(request["connectionCapability"], authority.1);
                            let body = serde_json::json!({
                                "connection_id": authority.0,
                                "subscription_id": "sub_remote",
                                "controller_connection_id": authority.0,
                                "lease_expires_at": "2026-04-02T00:00:15Z",
                                "heartbeat_interval_ms": 15000,
                                "snapshot": {
                                    "protocolVersion": "2026-03-30",
                                    "session_id": "sess_remote",
                                    "cursor": 0,
                                    "state": {
                                        "protocol_version": "2026-03-30",
                                        "session_id": "sess_remote",
                                        "pending_approvals": [],
                                        "active_tools": [],
                                        "active_utility_commands": [],
                                        "active_file_watches": [],
                                        "is_ready": true,
                                        "is_responding": false
                                    }
                                }
                            })
                            .to_string();
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                &body,
                            )
                            .await;
                            return;
                        }

                        if path.starts_with("/api/headless/sessions/")
                            && path.ends_with("/disconnect")
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                r#"{"success":true,"connection_id":"conn_fresh","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                            )
                            .await;
                            return;
                        }

                        if path.starts_with("/api/headless/sessions/")
                            && path.ends_with("/heartbeat")
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                r#"{"connection_id":"conn_fresh","controller_lease_granted":true,"controller_connection_id":"conn_fresh","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                            )
                            .await;
                            return;
                        }

                        if path.starts_with("/api/headless/sessions/") && path.contains("/events?")
                        {
                            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                            let _ = socket.write_all(headers.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 404 Not Found",
                            "text/plain",
                            "not found",
                        )
                        .await;
                    }
                });
            }
        }
    });

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        session_id: Some("sess_remote".to_string()),
        ..RemoteTransportConfig::default()
    };
    let stale_authority = RemoteConnectionResumeAuthority {
        connection_id: "conn_stale".to_string(),
        connection_capability: Some("cap_stale".to_string()),
    };

    let failure = match RemoteAgentTransport::connect_with_resume_authority(
        config.clone(),
        Some(stale_authority),
    )
    .await
    {
        Ok(_) => panic!("lost fallback response should fail the first connect"),
        Err(failure) => failure,
    };
    assert!(matches!(&failure.error, AsyncTransportError::Remote(_)));
    let retained_authority = failure
        .resume_authority
        .expect("ambiguous fallback must retain fresh authority");
    let fallback_authority = fallback_authority
        .lock()
        .await
        .clone()
        .expect("accepted fallback authority");
    assert_eq!(retained_authority.connection_id, fallback_authority.0);
    assert_eq!(
        retained_authority.connection_capability.as_deref(),
        Some(fallback_authority.1.as_str())
    );

    let transport =
        RemoteAgentTransport::connect_with_resume_authority(config, Some(retained_authority))
            .await
            .unwrap_or_else(|failure| panic!("retry should connect: {}", failure.error));

    assert_eq!(transport.connection_id(), fallback_authority.0);
    assert_eq!(connection_requests.lock().await.len(), 3);

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_recovers_when_bootstrap_response_is_lost() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let bootstrap_attempt = Arc::new(AtomicUsize::new(0));
    let connection_requests = Arc::new(Mutex::new(Vec::new()));
    let accepted_connection_id = Arc::new(Mutex::new(None::<String>));
    let accepted_capability = Arc::new(Mutex::new(None::<String>));

    tokio::spawn({
        let bootstrap_attempt = Arc::clone(&bootstrap_attempt);
        let connection_requests = Arc::clone(&connection_requests);
        let accepted_connection_id = Arc::clone(&accepted_connection_id);
        let accepted_capability = Arc::clone(&accepted_capability);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let bootstrap_attempt = Arc::clone(&bootstrap_attempt);
                let connection_requests = Arc::clone(&connection_requests);
                let accepted_connection_id = Arc::clone(&accepted_connection_id);
                let accepted_capability = Arc::clone(&accepted_capability);
                tokio::spawn(async move {
                    let Some((path, _headers, body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    if path == "/api/headless/connections" {
                        let attempt = bootstrap_attempt.fetch_add(1, Ordering::SeqCst) + 1;
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid connection request");
                        connection_requests.lock().await.push(request.clone());
                        assert!(request.get("takeControl").is_none());
                        let connection_id = request
                            .get("connectionId")
                            .and_then(serde_json::Value::as_str)
                            .expect("client-generated bootstrap connection id")
                            .to_string();
                        assert!(connection_id.starts_with("conn_"));
                        let capability = request
                            .get("connectionCapability")
                            .and_then(serde_json::Value::as_str)
                            .expect("client-generated bootstrap capability")
                            .to_string();
                        assert!(capability.starts_with("cap_"));

                        if attempt == 1 {
                            *accepted_connection_id.lock().await = Some(connection_id);
                            *accepted_capability.lock().await = Some(capability);
                            let _ = socket.shutdown().await;
                            return;
                        }

                        assert_eq!(
                            accepted_connection_id.lock().await.as_deref(),
                            Some(connection_id.as_str()),
                            "retry must reuse the accepted connection id"
                        );
                        assert_eq!(
                            accepted_capability.lock().await.as_deref(),
                            Some(capability.as_str()),
                            "retry must reuse the accepted private authority"
                        );
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &serde_json::json!({
                                "session_id": "sess_remote",
                                "connection_id": connection_id,
                            })
                            .to_string(),
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid subscribe request");
                        assert_eq!(
                            request
                                .get("connectionId")
                                .and_then(serde_json::Value::as_str),
                            accepted_connection_id.lock().await.as_deref()
                        );
                        assert_eq!(
                            request
                                .get("connectionCapability")
                                .and_then(serde_json::Value::as_str),
                            accepted_capability.lock().await.as_deref()
                        );
                        let response = serde_json::json!({
                            "connection_id": accepted_connection_id
                                .lock()
                                .await
                                .as_deref(),
                            "subscription_id": "sub_remote",
                            "heartbeat_interval_ms": 15000,
                            "snapshot": {
                                "protocolVersion": "2026-03-30",
                                "session_id": "sess_remote",
                                "cursor": 0,
                                "state": {
                                    "protocol_version": "2026-03-30",
                                    "session_id": "sess_remote",
                                    "pending_approvals": [],
                                    "active_tools": [],
                                    "active_utility_commands": [],
                                    "active_file_watches": [],
                                    "is_ready": true,
                                    "is_responding": false
                                }
                            }
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &response,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect")
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true,"connection_id":"conn_indeterminate","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"connection_id":"conn_indeterminate","controller_lease_granted":true,"controller_connection_id":"conn_indeterminate","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 404 Not Found",
                            "text/plain",
                            "not found",
                        )
                        .await;
                    }
                });
            }
        }
    });

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        session_id: Some("sess_remote".to_string()),
        ..RemoteTransportConfig::default()
    };
    let failure =
        match RemoteAgentTransport::connect_with_resume_authority(config.clone(), None).await {
            Ok(_) => panic!("lost bootstrap response should fail the first connect"),
            Err(failure) => failure,
        };
    assert!(matches!(failure.error, AsyncTransportError::Remote(_)));
    let retained_authority = failure
        .resume_authority
        .expect("ambiguous bootstrap must retain client-generated authority");
    assert_eq!(
        Some(retained_authority.connection_id.as_str()),
        accepted_connection_id.lock().await.as_deref()
    );
    assert_eq!(
        retained_authority.connection_capability.as_deref(),
        accepted_capability.lock().await.as_deref()
    );

    let transport =
        RemoteAgentTransport::connect_with_resume_authority(config, Some(retained_authority))
            .await
            .unwrap_or_else(|failure| panic!("retry should connect: {}", failure.error));
    assert_eq!(
        Some(transport.connection_id()),
        accepted_connection_id.lock().await.as_deref()
    );
    assert_eq!(
        transport
            .resume_authority()
            .connection_capability
            .as_deref(),
        accepted_capability.lock().await.as_deref()
    );
    assert_eq!(connection_requests.lock().await.len(), 2);

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_preserves_known_capability_when_bootstrap_omits_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connection_requests = Arc::new(Mutex::new(Vec::new()));
    let subscribe_requests = Arc::new(Mutex::new(Vec::new()));
    let subscribe_attempts = Arc::new(AtomicUsize::new(0));

    tokio::spawn({
        let connection_requests = Arc::clone(&connection_requests);
        let subscribe_requests = Arc::clone(&subscribe_requests);
        let subscribe_attempts = Arc::clone(&subscribe_attempts);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let connection_requests = Arc::clone(&connection_requests);
                let subscribe_requests = Arc::clone(&subscribe_requests);
                let subscribe_attempts = Arc::clone(&subscribe_attempts);
                tokio::spawn(async move {
                    let Some((path, _headers, body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    if path == "/api/headless/connections" {
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid connection request");
                        connection_requests.lock().await.push(request.clone());
                        if request
                            .get("connectionId")
                            .and_then(serde_json::Value::as_str)
                            != Some("conn_known")
                            || request
                                .get("connectionCapability")
                                .and_then(serde_json::Value::as_str)
                                != Some("cap_known")
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 403 Forbidden",
                                "application/json",
                                r#"{"error":"known connection authority required"}"#,
                            )
                            .await;
                            return;
                        }
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"session_id":"sess_remote","connection_id":"conn_known"}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid subscribe request");
                        subscribe_requests.lock().await.push(request.clone());
                        if request
                            .get("connectionCapability")
                            .and_then(serde_json::Value::as_str)
                            != Some("cap_known")
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 403 Forbidden",
                                "application/json",
                                r#"{"error":"known connection authority required"}"#,
                            )
                            .await;
                            return;
                        }
                        let attempt = subscribe_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        if attempt == 1 {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 503 Service Unavailable",
                                "application/json",
                                r#"{"error":"temporary subscription failure"}"#,
                            )
                            .await;
                            return;
                        }
                        let response = serde_json::json!({
                            "connection_id": "conn_known",
                            "subscription_id": "sub_remote",
                            "heartbeat_interval_ms": 15000,
                            "snapshot": {
                                "protocolVersion": "2026-03-30",
                                "session_id": "sess_remote",
                                "cursor": 0,
                                "state": {
                                    "protocol_version": "2026-03-30",
                                    "session_id": "sess_remote",
                                    "pending_approvals": [],
                                    "active_tools": [],
                                    "active_utility_commands": [],
                                    "active_file_watches": [],
                                    "is_ready": true,
                                    "is_responding": false
                                }
                            }
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &response,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect")
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true,"connection_id":"conn_known","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"connection_id":"conn_known","controller_lease_granted":true,"controller_connection_id":"conn_known","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "text/plain",
                        "not found",
                    )
                    .await;
                });
            }
        }
    });

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        session_id: Some("sess_remote".to_string()),
        ..RemoteTransportConfig::default()
    };
    let initial_authority = RemoteConnectionResumeAuthority {
        connection_id: "conn_known".to_string(),
        connection_capability: Some("cap_known".to_string()),
    };

    let failure = match RemoteAgentTransport::connect_with_resume_authority(
        config.clone(),
        Some(initial_authority),
    )
    .await
    {
        Ok(_) => panic!("first subscribe should fail transiently"),
        Err(failure) => failure,
    };
    assert!(
        matches!(
            &failure.error,
            AsyncTransportError::RemoteStatus {
                status: 503,
                retryable: true,
                ..
            }
        ),
        "unexpected first failure: {:?}",
        failure.error
    );
    let retained_authority = failure
        .resume_authority
        .expect("known authority should survive subscribe failure");
    assert_eq!(retained_authority.connection_id, "conn_known");
    assert_eq!(
        retained_authority.connection_capability.as_deref(),
        Some("cap_known")
    );

    let transport =
        RemoteAgentTransport::connect_with_resume_authority(config, Some(retained_authority))
            .await
            .unwrap_or_else(|failure| panic!("retry should connect: {}", failure.error));

    assert_eq!(
        transport
            .resume_authority()
            .connection_capability
            .as_deref(),
        Some("cap_known")
    );
    assert_eq!(connection_requests.lock().await.len(), 2);
    assert_eq!(subscribe_requests.lock().await.len(), 2);

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_classifies_bootstrap_conflict_as_non_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
            return;
        };
        assert_eq!(path, "/api/headless/connections");
        write_http_response(
            &mut socket,
            "HTTP/1.1 409 Conflict",
            "application/json",
            r#"{"error":"Controller lease is already held by another connection"}"#,
        )
        .await;
    });

    let error = match RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    {
        Ok(_) => panic!("bootstrap conflict should fail"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 409,
            retryable: false,
            kind: RemoteErrorKind::ControllerLeaseConflict,
            ..
        }
    ));
}

#[tokio::test]
async fn remote_transport_classifies_generic_subscribe_404_as_non_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                    return;
                };

                if path == "/api/headless/connections" {
                    let body = serde_json::json!({
                        "session_id": "sess_remote",
                        "connection_id": "conn_remote",
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "application/json",
                        r#"{"error":"route not found"}"#,
                    )
                    .await;
                    return;
                }

                write_http_response(
                    &mut socket,
                    "HTTP/1.1 404 Not Found",
                    "text/plain",
                    "not found",
                )
                .await;
            });
        }
    });

    let error = match RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    {
        Ok(_) => panic!("generic subscribe 404 should fail"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 404,
            retryable: false,
            kind: RemoteErrorKind::Other,
            ..
        }
    ));
}

#[tokio::test]
async fn remote_transport_classifies_stream_404_as_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                    return;
                };

                if path == "/api/headless/connections" {
                    let body = serde_json::json!({
                        "session_id": "sess_remote",
                        "connection_id": "conn_remote",
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                    let body = serde_json::json!({
                        "connection_id": "conn_remote",
                        "subscription_id": "sub_remote",
                        "controller_connection_id": "conn_remote",
                        "lease_expires_at": "2026-04-02T00:00:15Z",
                        "heartbeat_interval_ms": 15000,
                        "snapshot": {
                            "protocolVersion": "2026-03-30",
                            "session_id": "sess_remote",
                            "cursor": 0,
                            "state": {
                                "protocol_version": "2026-03-30",
                                "session_id": "sess_remote",
                                "pending_approvals": [],
                                "active_tools": [],
                                "active_utility_commands": [],
                                "active_file_watches": [],
                                "is_ready": true,
                                "is_responding": false
                            }
                        }
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "application/json",
                        r#"{"error":"Headless subscriber not found"}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"connection_id":"conn_remote","controller_lease_granted":true,"controller_connection_id":"conn_remote","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                    )
                    .await;
                    return;
                }

                write_http_response(
                    &mut socket,
                    "HTTP/1.1 404 Not Found",
                    "text/plain",
                    "not found",
                )
                .await;
            });
        }
    });

    let mut transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    let error = transport
        .recv_incoming()
        .await
        .expect_err("stream 404 should fail");
    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 404,
            retryable: true,
            kind: RemoteErrorKind::StaleSubscriber,
            ..
        }
    ));

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_classifies_generic_stream_404_as_non_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                    return;
                };

                if path == "/api/headless/connections" {
                    let body = serde_json::json!({
                        "session_id": "sess_remote",
                        "connection_id": "conn_remote",
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                    let body = serde_json::json!({
                        "connection_id": "conn_remote",
                        "subscription_id": "sub_remote",
                        "controller_connection_id": "conn_remote",
                        "lease_expires_at": "2026-04-02T00:00:15Z",
                        "heartbeat_interval_ms": 15000,
                        "snapshot": {
                            "protocolVersion": "2026-03-30",
                            "session_id": "sess_remote",
                            "cursor": 0,
                            "state": {
                                "protocol_version": "2026-03-30",
                                "session_id": "sess_remote",
                                "pending_approvals": [],
                                "active_tools": [],
                                "active_utility_commands": [],
                                "active_file_watches": [],
                                "is_ready": true,
                                "is_responding": false
                            }
                        }
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "application/json",
                        r#"{"error":"route not found"}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "application/json",
                        r#"{"error":"route not found"}"#,
                    )
                    .await;
                    return;
                }

                write_http_response(
                    &mut socket,
                    "HTTP/1.1 404 Not Found",
                    "text/plain",
                    "not found",
                )
                .await;
            });
        }
    });

    let mut transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    let error = transport
        .recv_incoming()
        .await
        .expect_err("generic stream 404 should fail");
    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 404,
            retryable: false,
            kind: RemoteErrorKind::Other,
            ..
        }
    ));

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_surfaces_non_retryable_heartbeat_failures() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                    return;
                };

                if path == "/api/headless/connections" {
                    let body = serde_json::json!({
                        "session_id": "sess_remote",
                        "connection_id": "conn_remote",
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                    let body = serde_json::json!({
                        "connection_id": "conn_remote",
                        "subscription_id": "sub_remote",
                        "controller_connection_id": "conn_remote",
                        "lease_expires_at": "2026-04-02T00:00:15Z",
                        "heartbeat_interval_ms": 1,
                        "snapshot": {
                            "protocolVersion": "2026-03-30",
                            "session_id": "sess_remote",
                            "cursor": 0,
                            "state": {
                                "protocol_version": "2026-03-30",
                                "session_id": "sess_remote",
                                "pending_approvals": [],
                                "active_tools": [],
                                "active_utility_commands": [],
                                "active_file_watches": [],
                                "is_ready": true,
                                "is_responding": false
                            }
                        }
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                    if socket.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_mins(1)).await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "application/json",
                        r#"{"error":"route not found"}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/unsubscribe") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true}"#,
                    )
                    .await;
                    return;
                }

                write_http_response(
                    &mut socket,
                    "HTTP/1.1 404 Not Found",
                    "text/plain",
                    "not found",
                )
                .await;
            });
        }
    });

    let mut transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");
    let cancel_token = transport.cancel_token();

    let error = tokio::time::timeout(Duration::from_secs(1), transport.recv_incoming())
        .await
        .expect("heartbeat failure should arrive before timeout")
        .expect_err("heartbeat failure should surface as an incoming error");
    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 404,
            retryable: false,
            kind: RemoteErrorKind::Other,
            ..
        }
    ));

    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
    transport.shutdown().expect("shutdown");
}

#[test]
fn retryability_allows_recovery_from_stale_message_connection_errors() {
    assert_eq!(
        classify_remote_status(
            StatusCode::FORBIDDEN,
            RemoteRequestKind::Message,
            r#"{"error":"Headless connection not found"}"#,
        ),
        (true, RemoteErrorKind::StaleConnection)
    );
}

#[test]
fn retryability_stops_bootstrap_connection_not_found_retries() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Bootstrap,
            r#"{"error":"Headless connection not found"}"#,
        ),
        (false, RemoteErrorKind::StaleConnection)
    );
}

#[test]
fn retryability_keeps_session_not_found_retryable_after_bootstrap() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Stream,
            r#"{"error":"Headless session not found"}"#,
        ),
        (true, RemoteErrorKind::StaleSession)
    );
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Subscribe,
            r#"{"error":"Headless session not found"}"#,
        ),
        (true, RemoteErrorKind::StaleSession)
    );
}

#[test]
fn retryability_stops_bootstrap_session_not_found_retries() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Bootstrap,
            r#"{"error":"Headless session not found"}"#,
        ),
        (false, RemoteErrorKind::StaleSession)
    );
}

#[test]
fn retryability_keeps_controller_lease_conflicts_non_retryable() {
    assert_eq!(
        classify_remote_status(
            StatusCode::CONFLICT,
            RemoteRequestKind::Subscribe,
            r#"{"error":"Controller lease is already held by another connection"}"#,
        ),
        (false, RemoteErrorKind::ControllerLeaseConflict)
    );
}

#[test]
fn retryability_classifies_structured_runtime_owner_mismatches() {
    assert_eq!(
        classify_remote_status(
            StatusCode::CONFLICT,
            RemoteRequestKind::Subscribe,
            r#"{"error":"Hosted runner is bound to Maestro session sess_owner","code":"ALREADY_EXISTS","error_type":"runtime_owned_elsewhere"}"#,
        ),
        (false, RemoteErrorKind::OwnershipConflict)
    );
    assert_eq!(
        classify_remote_status(
            StatusCode::CONFLICT,
            RemoteRequestKind::Heartbeat,
            r#"{"error":"Hosted runner is bound to Maestro session sess_owner","code":"ALREADY_EXISTS","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"runtime_owned_elsewhere","domain":"maestro.hosted_runner","metadata":{"owner_instance_id":"pod-a","maestro_session_id":"sess_owner"}}]}"#,
        ),
        (false, RemoteErrorKind::OwnershipConflict)
    );
}

#[test]
fn retryability_classifies_structured_runtime_not_ready_as_retryable() {
    assert_eq!(
        classify_remote_status(
            StatusCode::SERVICE_UNAVAILABLE,
            RemoteRequestKind::Subscribe,
            r#"{"error":"Hosted runner is draining and not accepting headless session traffic","code":"UNAVAILABLE","error_type":"runtime_not_ready"}"#,
        ),
        (true, RemoteErrorKind::RuntimeNotReady)
    );
    assert_eq!(
        classify_remote_status(
            StatusCode::SERVICE_UNAVAILABLE,
            RemoteRequestKind::Message,
            r#"{"error":"Hosted runner is draining and not accepting headless session traffic","code":"UNAVAILABLE","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"runtime_not_ready","domain":"maestro.hosted_runner","metadata":{"draining":"true","maestro_session_id":"sess_owner"}}]}"#,
        ),
        (true, RemoteErrorKind::RuntimeNotReady)
    );
}

#[test]
fn retryability_keeps_subscriber_not_found_retryable_for_streams() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Stream,
            r#"{"error":"Headless subscriber not found"}"#,
        ),
        (true, RemoteErrorKind::StaleSubscriber)
    );
}

#[test]
fn retryability_keeps_generic_stream_404s_non_retryable() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Stream,
            r#"{"error":"route not found"}"#,
        ),
        (false, RemoteErrorKind::Other)
    );
}

#[test]
fn retryability_keeps_generic_heartbeat_404s_non_retryable() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Heartbeat,
            r#"{"error":"route not found"}"#,
        ),
        (false, RemoteErrorKind::Other)
    );
}

#[test]
fn retryability_keeps_generic_subscribe_404s_non_retryable() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Subscribe,
            r#"{"error":"route not found"}"#,
        ),
        (false, RemoteErrorKind::Other)
    );
}

#[test]
fn retryability_stops_subscriber_not_found_retries_for_subscribe() {
    assert_eq!(
        classify_remote_status(
            StatusCode::NOT_FOUND,
            RemoteRequestKind::Subscribe,
            r#"{"error":"Headless subscriber not found"}"#,
        ),
        (false, RemoteErrorKind::StaleSubscriber)
    );
}

#[test]
fn retryability_keeps_generic_message_client_errors_non_retryable() {
    assert_eq!(
        classify_remote_status(
            StatusCode::BAD_REQUEST,
            RemoteRequestKind::Message,
            r#"{"error":"bad request"}"#,
        ),
        (false, RemoteErrorKind::Other)
    );
}

#[test]
fn writer_retry_budget_ignores_stale_reference_errors() {
    assert!(!should_retry_message_error(
        &AsyncTransportError::RemoteStatus {
            status: 404,
            message: "remote request failed with status 404: Headless connection not found"
                .to_string(),
            retryable: true,
            kind: RemoteErrorKind::StaleConnection,
        }
    ));
    assert!(!should_retry_message_error(
        &AsyncTransportError::RemoteStatus {
            status: 404,
            message: "remote request failed with status 404: Headless session not found"
                .to_string(),
            retryable: true,
            kind: RemoteErrorKind::StaleSession,
        }
    ));
    assert!(!should_retry_message_error(
        &AsyncTransportError::RemoteStatus {
            status: 404,
            message: "remote request failed with status 404: Headless subscriber not found"
                .to_string(),
            retryable: true,
            kind: RemoteErrorKind::StaleSubscriber,
        }
    ));
}

#[tokio::test]
async fn remote_transport_connects_sends_and_receives_events() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "controller_pending_events": [
            {
                "type": "client_tool_request",
                "call_id": "pending_client_tool",
                "tool": "artifacts",
                "args": {"command": "create", "filename": "pending.txt"}
            },
            {
                "type": "server_request",
                "request_id": "pending_server_request",
                "request_type": "client_tool",
                "call_id": "pending_server_call",
                "tool": "artifacts",
                "args": {"command": "update", "filename": "pending.txt"},
                "reason": "Client tool execution is pending"
            }
        ],
        "last_init": {
            "system_prompt": "Be terse",
            "thinking_level": "high",
            "approval_mode": "prompt"
        },
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "cwd": "/tmp/project",
            "git_branch": "main",
            "pending_approvals": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let hello_ok_event = serde_json::json!({
        "type": "message",
        "cursor": 2,
        "message": {
            "type": "hello_ok",
            "protocol_version": "2026-04-03",
            "connection_id": "conn_remote",
            "client_protocol_version": "2026-04-03",
            "client_info": {
                "name": "maestro-tui-rs",
                "version": "1.0.0"
            },
            "capabilities": {
                "server_requests": ["approval"],
                "utility_operations": ["command_exec", "file_search", "file_read", "file_watch"],
                "raw_agent_events": false
            },
            "role": "controller",
            "controller_connection_id": "conn_remote"
        }
    })
    .to_string();
    let message_event = serde_json::json!({
        "type": "message",
        "cursor": 3,
        "message": {
            "type": "status",
            "message": "Remote update"
        }
    })
    .to_string();

    let (addr, posted_bodies, request_paths, request_headers) =
        spawn_remote_headless_server(snapshot.to_string(), vec![hello_ok_event, message_event])
            .await;

    let mut config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        api_key: Some("secret".to_string()),
        ..RemoteTransportConfig::default()
    };
    config
        .headers
        .insert("x-maestro-client".to_string(), "tui-rs".to_string());

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");
    let cancel_token = transport.cancel_token();
    assert_eq!(transport.session_id(), "sess_remote");
    assert_eq!(transport.subscription_id(), "sub_remote");
    assert_eq!(transport.state().model.as_deref(), Some("gpt-5.6"));
    assert_eq!(transport.state().provider.as_deref(), Some("openai"));
    assert_eq!(transport.state().last_status.as_deref(), Some("Attached"));
    assert!(
        transport.state().pending_client_tools.is_empty(),
        "raw controller backlog must not be exposed through the public snapshot state"
    );
    assert_eq!(
        transport
            .last_init()
            .and_then(|init| init.system_prompt.as_deref()),
        Some("Be terse")
    );

    let posted = wait_for_posted_bodies_len(&posted_bodies, 1).await;
    assert_eq!(posted.len(), 1);
    let sent = serde_json::from_str::<ToAgentMessage>(&posted[0]).expect("parse sent message");
    assert!(matches!(
        sent,
        ToAgentMessage::Hello {
            protocol_version,
            client_info,
            capabilities,
            role,
            ..
        } if protocol_version.as_deref() == Some(HEADLESS_PROTOCOL_VERSION)
            && client_info.as_ref().map(|info| info.name.as_str()) == Some("maestro-tui-rs")
            && capabilities.as_ref().and_then(|items| items.server_requests.as_ref()) == Some(&vec![
                ServerRequestType::Approval,
                ServerRequestType::ToolRetry,
            ])
            && role == Some(ConnectionRole::Controller)
    ));

    let incoming = transport
        .recv_incoming()
        .await
        .expect("pending client tool request");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::ClientToolRequest {
            call_id,
            tool,
            args,
            ..
        }) => {
            assert_eq!(call_id, "pending_client_tool");
            assert_eq!(tool, "artifacts");
            assert_eq!(args["filename"], "pending.txt");
        }
        other => panic!("expected pending client tool request, got {other:?}"),
    }

    let incoming = transport
        .recv_incoming()
        .await
        .expect("pending structured server request");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::ServerRequest {
            request_id,
            request_type,
            call_id,
            args,
            ..
        }) => {
            assert_eq!(request_id, "pending_server_request");
            assert_eq!(request_type, ServerRequestType::ClientTool);
            assert_eq!(call_id, "pending_server_call");
            assert_eq!(args["command"], "update");
        }
        other => panic!("expected pending structured server request, got {other:?}"),
    }

    let incoming = transport.recv_incoming().await.expect("incoming hello_ok");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::HelloOk {
            connection_id,
            controller_connection_id,
            ..
        }) => {
            assert_eq!(connection_id.as_deref(), Some("conn_remote"));
            assert_eq!(controller_connection_id.as_deref(), Some("conn_remote"));
        }
        other => panic!("expected remote hello_ok, got {other:?}"),
    }
    assert_eq!(
        transport.state().client_protocol_version.as_deref(),
        Some("2026-04-03")
    );
    assert_eq!(
        transport.state().controller_connection_id.as_deref(),
        Some("conn_remote")
    );

    let incoming = transport
        .recv_incoming()
        .await
        .expect("incoming status event");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::Status { message }) => {
            assert_eq!(message, "Remote update");
        }
        other => panic!("expected remote status message, got {other:?}"),
    }
    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Remote update")
    );

    transport
        .send(ToAgentMessage::Interrupt)
        .expect("send interrupt");

    let posted = wait_for_posted_bodies_len(&posted_bodies, 2).await;
    assert_eq!(posted.len(), 2);
    let sent = serde_json::from_str::<ToAgentMessage>(&posted[1]).expect("parse sent message");
    assert!(matches!(sent, ToAgentMessage::Interrupt));

    let headers = request_headers.lock().await.clone();
    let connection_headers = headers.first().expect("connection request headers");
    let subscribe_headers = headers.get(1).expect("subscribe request headers");
    let message_headers = headers.iter().find(|entry| {
        entry.iter().any(|(name, value)| {
            name == "x-maestro-headless-subscriber-id" && value == "sub_remote"
        })
    });
    assert!(
        connection_headers
            .iter()
            .any(|(name, value)| { name == "authorization" && value == "Bearer secret" })
    );
    assert!(
        connection_headers
            .iter()
            .any(|(name, value)| { name == "x-maestro-client" && value == "tui-rs" })
    );
    assert!(
        connection_headers
            .iter()
            .any(|(name, value)| { name == "x-maestro-headless-role" && value == "controller" })
    );
    assert!(
        connection_headers
            .iter()
            .any(|(name, value)| { name == "x-composer-headless-role" && value == "controller" })
    );
    assert!(
        subscribe_headers
            .iter()
            .any(|(name, value)| { name == "x-maestro-headless-role" && value == "controller" })
    );
    let message_headers = message_headers.expect("message request headers");
    assert!(message_headers.iter().any(|(name, value)| {
        name == "x-maestro-headless-connection-capability" && value == "cap_remote"
    }));

    transport.shutdown().expect("shutdown");

    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let posted = posted_bodies.lock().await.clone();
    assert_eq!(posted.len(), 2);

    let paths = request_paths.lock().await.clone();
    assert!(
        paths.iter().any(|path| path.ends_with("/disconnect")),
        "expected remote shutdown to disconnect the explicit connection without shutting down the runtime"
    );
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_viewer_transport_rejects_controller_messages() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });

    let (addr, posted_bodies, request_paths, _request_headers) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        role: Some("viewer".to_string()),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(posted_bodies.lock().await.is_empty());
    assert!(
        request_paths
            .lock()
            .await
            .iter()
            .all(|path| !path.ends_with("/messages")),
        "viewer connect should not post a bootstrap hello message"
    );

    let prompt_error = transport
        .send(ToAgentMessage::Prompt {
            content: "viewer should stay read-only".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .expect_err("viewer prompt should be rejected");
    assert!(matches!(
        prompt_error,
        AsyncTransportError::SendFailed(ref message)
            if message.contains("viewer connections cannot send remote session messages")
    ));

    let interrupt_error = transport
        .send(ToAgentMessage::Interrupt)
        .expect_err("viewer interrupt should be rejected");
    assert!(matches!(
        interrupt_error,
        AsyncTransportError::SendFailed(ref message)
            if message.contains("viewer connections cannot send remote session messages")
    ));

    let cancel_error = transport
        .send(ToAgentMessage::Cancel)
        .expect_err("viewer cancel should be rejected");
    assert!(matches!(
        cancel_error,
        AsyncTransportError::SendFailed(ref message)
            if message.contains("viewer connections cannot send remote session messages")
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(posted_bodies.lock().await.is_empty());

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_updates_cached_state_on_snapshot_events() {
    let initial_snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "last_init": {
            "system_prompt": "Initial prompt"
        },
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "tracked_tools": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let snapshot_event = serde_json::json!({
        "type": "snapshot",
        "snapshot": {
            "protocolVersion": "2026-03-30",
            "session_id": "sess_remote",
            "cursor": 2,
            "last_init": {
                "system_prompt": "Updated prompt"
            },
            "state": {
                "protocol_version": "2026-03-30",
                "model": "gpt-5.6",
                "provider": "openai",
                "session_id": "sess_remote",
                "pending_approvals": [],
                "tracked_tools": [],
                "active_tools": [],
                "last_status": "Replayed snapshot",
                "is_ready": true,
                "is_responding": false
            }
        }
    })
    .to_string();

    let (addr, _posted_bodies, _request_paths, _request_headers) =
        spawn_remote_headless_server(initial_snapshot.to_string(), vec![snapshot_event]).await;

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    };

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");
    let cancel_token = transport.cancel_token();
    assert_eq!(transport.state().last_status.as_deref(), Some("Attached"));
    assert_eq!(
        transport
            .last_init()
            .and_then(|init| init.system_prompt.as_deref()),
        Some("Initial prompt")
    );

    let incoming = transport.recv_incoming().await.expect("incoming snapshot");
    match incoming {
        RemoteIncoming::Snapshot { .. } => {}
        other => panic!("expected remote snapshot, got {other:?}"),
    }

    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Replayed snapshot")
    );
    assert_eq!(
        transport
            .last_init()
            .and_then(|init| init.system_prompt.as_deref()),
        Some("Updated prompt")
    );

    transport.shutdown().expect("shutdown");
    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_transport_ignores_replayed_events_that_do_not_advance_cursor() {
    let initial_snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "tracked_tools": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let first_status_event = serde_json::json!({
        "type": "message",
        "cursor": 2,
        "message": {
            "type": "status",
            "message": "Remote update"
        }
    })
    .to_string();
    let replayed_status_event = serde_json::json!({
        "type": "message",
        "cursor": 2,
        "message": {
            "type": "status",
            "message": "stale replay"
        }
    })
    .to_string();
    let heartbeat_event = serde_json::json!({
        "type": "heartbeat",
        "cursor": 3
    })
    .to_string();

    let (addr, _posted_bodies, _request_paths, _request_headers) = spawn_remote_headless_server(
        initial_snapshot.to_string(),
        vec![first_status_event, replayed_status_event, heartbeat_event],
    )
    .await;

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    };

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");
    let cancel_token = transport.cancel_token();

    let incoming = transport
        .recv_incoming()
        .await
        .expect("incoming status event");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::Status { message }) => {
            assert_eq!(message, "Remote update");
        }
        other => panic!("expected remote status message, got {other:?}"),
    }

    let incoming = transport.recv_incoming().await.expect("incoming heartbeat");
    assert!(matches!(incoming, RemoteIncoming::Heartbeat));
    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Remote update")
    );
    assert!(transport.try_recv_incoming().is_none());

    transport.shutdown().expect("shutdown");
    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_transport_accepts_heartbeat_without_cursor_advance() {
    let initial_snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "tracked_tools": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let first_status_event = serde_json::json!({
        "type": "message",
        "cursor": 2,
        "message": {
            "type": "status",
            "message": "Remote update"
        }
    })
    .to_string();
    let nonadvancing_heartbeat_event = serde_json::json!({
        "type": "heartbeat",
        "cursor": 2
    })
    .to_string();

    let (addr, _posted_bodies, _request_paths, _request_headers) = spawn_remote_headless_server(
        initial_snapshot.to_string(),
        vec![first_status_event, nonadvancing_heartbeat_event],
    )
    .await;

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    };

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");
    let cancel_token = transport.cancel_token();

    let incoming = transport
        .recv_incoming()
        .await
        .expect("incoming status event");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::Status { message }) => {
            assert_eq!(message, "Remote update");
        }
        other => panic!("expected remote status message, got {other:?}"),
    }

    let incoming = transport.recv_incoming().await.expect("incoming heartbeat");
    assert!(matches!(incoming, RemoteIncoming::Heartbeat));
    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Remote update")
    );

    transport.shutdown().expect("shutdown");
    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_transport_synthesizes_liveness_when_stream_heartbeats_are_opted_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                    return;
                };

                if path == "/api/headless/connections" {
                    let body = serde_json::json!({
                        "session_id": "sess_remote",
                        "connection_id": "conn_remote",
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                    let body = serde_json::json!({
                        "connection_id": "conn_remote",
                        "subscription_id": "sub_remote",
                        "controller_connection_id": "conn_remote",
                        "lease_expires_at": "2026-04-02T00:00:15Z",
                        "heartbeat_interval_ms": 1,
                        "snapshot": {
                            "protocolVersion": "2026-03-30",
                            "session_id": "sess_remote",
                            "cursor": 0,
                            "state": {
                                "protocol_version": "2026-03-30",
                                "session_id": "sess_remote",
                                "pending_approvals": [],
                                "active_tools": [],
                                "active_utility_commands": [],
                                "active_file_watches": [],
                                "is_ready": true,
                                "is_responding": false
                            }
                        }
                    })
                    .to_string();
                    write_http_response(&mut socket, "HTTP/1.1 200 OK", "application/json", &body)
                        .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                    if socket.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    let status_event = serde_json::json!({
                        "type": "message",
                        "cursor": 1,
                        "message": {
                            "type": "status",
                            "message": "Remote update"
                        }
                    });
                    let payload = format!("data: {status_event}\n\n");
                    if socket.write_all(payload.as_bytes()).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_mins(1)).await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"connection_id":"conn_remote","controller_lease_granted":true,"controller_connection_id":"conn_remote","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":25}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                    )
                    .await;
                    return;
                }

                if path.starts_with("/api/headless/sessions/") && path.ends_with("/unsubscribe") {
                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"success":true}"#,
                    )
                    .await;
                    return;
                }

                write_http_response(
                    &mut socket,
                    "HTTP/1.1 404 Not Found",
                    "text/plain",
                    "not found",
                )
                .await;
            });
        }
    });

    let mut transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        opt_out_notifications: vec!["heartbeat".to_string()],
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");
    let cancel_token = transport.cancel_token();

    let mut saw_status = false;
    let mut saw_heartbeat = false;
    for _ in 0..3 {
        let incoming = tokio::time::timeout(Duration::from_secs(1), transport.recv_incoming())
            .await
            .expect("remote status or synthetic heartbeat should arrive before timeout")
            .expect("remote status or synthetic heartbeat should be delivered");
        match incoming {
            RemoteIncoming::Message(FromAgentMessage::Status { message }) => {
                assert_eq!(message, "Remote update");
                saw_status = true;
            }
            RemoteIncoming::Heartbeat => {
                saw_heartbeat = true;
            }
            other => panic!("expected remote status or heartbeat, got {other:?}"),
        }

        if saw_status && saw_heartbeat {
            break;
        }
    }
    assert!(saw_status, "expected streamed status event");
    assert!(saw_heartbeat, "expected synthetic heartbeat event");
    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Remote update")
    );

    transport.shutdown().expect("shutdown");
    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_transport_ignores_malformed_events_and_keeps_streaming() {
    let initial_snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "tracked_tools": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let first_status_event = serde_json::json!({
        "type": "message",
        "cursor": 2,
        "message": {
            "type": "status",
            "message": "Remote update"
        }
    })
    .to_string();
    let malformed_event = "{\"type\":\"message\",\"cursor\":3,\"message\":".to_string();
    let heartbeat_event = serde_json::json!({
        "type": "heartbeat",
        "cursor": 4
    })
    .to_string();

    let (addr, _posted_bodies, _request_paths, _request_headers) = spawn_remote_headless_server(
        initial_snapshot.to_string(),
        vec![first_status_event, malformed_event, heartbeat_event],
    )
    .await;

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    };

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");
    let cancel_token = transport.cancel_token();

    let incoming = transport
        .recv_incoming()
        .await
        .expect("incoming status event");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::Status { message }) => {
            assert_eq!(message, "Remote update");
        }
        other => panic!("expected remote status message, got {other:?}"),
    }

    let incoming = transport.recv_incoming().await.expect("incoming heartbeat");
    assert!(matches!(incoming, RemoteIncoming::Heartbeat));
    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Remote update")
    );

    transport.shutdown().expect("shutdown");
    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_transport_reader_exits_after_malformed_event_when_receiver_is_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server_handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let Some((_path, _headers, _body)) = read_http_request(&mut socket).await else {
            return;
        };

        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }

        let malformed_event = "data: {\"type\":\"message\",\"cursor\":1,\"message\":\n\n";
        if socket.write_all(malformed_event.as_bytes()).await.is_err() {
            return;
        }

        tokio::time::sleep(Duration::from_mins(1)).await;
    });

    let (event_tx, event_rx) =
        mpsc::unbounded_channel::<Result<RemoteIncoming, AsyncTransportError>>();
    drop(event_rx);

    let cancel = CancellationToken::new();
    tokio::time::timeout(
        Duration::from_secs(1),
        reader_loop(
            Client::new(),
            RemoteTransportConfig {
                base_url: format!("http://{addr}"),
                ..RemoteTransportConfig::default()
            },
            "sess_remote".to_string(),
            "sub_remote".to_string(),
            0,
            ReaderEventContext {
                event_tx,
                event_notification: Arc::new(Notify::new()),
            },
            cancel.clone(),
        ),
    )
    .await
    .expect("reader loop should exit once the consumer channel is dropped");

    cancel.cancel();
    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn remote_transport_surfaces_stream_closure_without_internal_reader_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let request_paths = Arc::new(Mutex::new(Vec::new()));
    let initial_snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "tracked_tools": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let status_event = serde_json::json!({
        "type": "message",
        "cursor": 2,
        "message": {
            "type": "status",
            "message": "Remote update"
        }
    })
    .to_string();
    tokio::spawn({
        let request_paths = Arc::clone(&request_paths);
        let snapshot_json = initial_snapshot.to_string();
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request_paths = Arc::clone(&request_paths);
                let snapshot_json = snapshot_json.clone();
                let status_event = status_event.clone();
                tokio::spawn(async move {
                    let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    request_paths.lock().await.push(path.clone());

                    if path == "/api/headless/connections" {
                        let body = serde_json::json!({
                            "session_id": "sess_remote",
                            "connection_id": "conn_remote",
                            "controller_connection_id": "conn_remote",
                            "lease_expires_at": "2026-04-02T00:00:15Z",
                            "heartbeat_interval_ms": 15000,
                            "snapshot": serde_json::from_str::<serde_json::Value>(&snapshot_json)
                                .expect("valid snapshot json"),
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &body,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        let body = serde_json::json!({
                            "connection_id": "conn_remote",
                            "subscription_id": "sub_remote",
                            "controller_connection_id": "conn_remote",
                            "lease_expires_at": "2026-04-02T00:00:15Z",
                            "heartbeat_interval_ms": 15000,
                            "snapshot": serde_json::from_str::<serde_json::Value>(&snapshot_json)
                                .expect("valid snapshot json"),
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &body,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect")
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"connection_id":"conn_remote","controller_lease_granted":true,"controller_connection_id":"conn_remote","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                        if socket.write_all(headers.as_bytes()).await.is_err() {
                            return;
                        }
                        let payload = format!("data: {status_event}\n\n");
                        let _ = socket.write_all(payload.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }

                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "text/plain",
                        "not found",
                    )
                    .await;
                });
            }
        }
    });

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    };

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");
    let cancel_token = transport.cancel_token();

    let incoming = transport
        .recv_incoming()
        .await
        .expect("incoming status event");
    match incoming {
        RemoteIncoming::Message(FromAgentMessage::Status { message }) => {
            assert_eq!(message, "Remote update");
        }
        other => panic!("expected remote status message, got {other:?}"),
    }

    let error = transport
        .recv_incoming()
        .await
        .expect_err("stream closure should surface as an incoming error");
    assert!(
        matches!(error, AsyncTransportError::Remote(message) if message.contains("closed after emitting data"))
    );

    tokio::time::sleep(Duration::from_millis(25)).await;
    let paths = request_paths.lock().await.clone();
    let event_requests = paths
        .iter()
        .filter(|path| path.contains("/events?"))
        .count();
    assert_eq!(
        event_requests, 1,
        "reader loop should not retry /events internally"
    );

    transport.shutdown().expect("shutdown");
    for _ in 0..50 {
        if cancel_token.is_cancelled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn remote_transport_sends_utility_command_resize_messages() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "active_tools": [],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    });

    let (addr, posted_bodies, _request_paths, _request_headers) =
        spawn_remote_headless_server(snapshot.to_string(), Vec::new()).await;

    let transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    transport
        .resize_utility_command("cmd_pty".to_string(), 120, 40)
        .expect("send utility command resize");

    let posted = wait_for_posted_bodies_len(&posted_bodies, 2).await;
    assert_eq!(posted.len(), 2);
    let sent = serde_json::from_str::<ToAgentMessage>(&posted[1]).expect("parse sent message");
    assert!(matches!(
        sent,
        ToAgentMessage::UtilityCommandResize {
            command_id,
            columns,
            rows,
        } if command_id == "cmd_pty" && columns == 120 && rows == 40
    ));

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_retries_retryable_message_post_failures() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "active_tools": [],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let posted_bodies = Arc::new(Mutex::new(Vec::new()));
    let interrupt_attempts = Arc::new(AtomicUsize::new(0));

    tokio::spawn({
        let posted_bodies = Arc::clone(&posted_bodies);
        let interrupt_attempts = Arc::clone(&interrupt_attempts);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let posted_bodies = Arc::clone(&posted_bodies);
                let interrupt_attempts = Arc::clone(&interrupt_attempts);
                let snapshot = snapshot.clone();

                tokio::spawn(async move {
                    let Some((path, _headers, body)) = read_http_request(&mut socket).await else {
                        return;
                    };

                    if path == "/api/headless/connections" {
                        let body = serde_json::json!({
                            "session_id": "sess_remote",
                            "connection_id": "conn_remote",
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &body,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        let body = serde_json::json!({
                            "connection_id": "conn_remote",
                            "subscription_id": "sub_remote",
                            "controller_connection_id": "conn_remote",
                            "lease_expires_at": "2026-04-02T00:00:15Z",
                            "heartbeat_interval_ms": 15000,
                            "snapshot": snapshot,
                        })
                        .to_string();
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            &body,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/disconnect")
                    {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/heartbeat") {
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"connection_id":"conn_remote","controller_lease_granted":true,"controller_connection_id":"conn_remote","lease_expires_at":"2026-04-02T00:00:15Z","heartbeat_interval_ms":15000}"#,
                        )
                        .await;
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.contains("/events?") {
                        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let (_tx, mut rx) = mpsc::unbounded_channel::<String>();
                        while let Some(event) = rx.recv().await {
                            let payload = format!("data: {event}\n\n");
                            if socket.write_all(payload.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") {
                        posted_bodies.lock().await.push(body.clone());
                        let message = serde_json::from_str::<ToAgentMessage>(&body)
                            .expect("valid outbound message");
                        if matches!(message, ToAgentMessage::Interrupt) {
                            let attempt = interrupt_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                            if attempt == 1 {
                                write_http_response(
                                    &mut socket,
                                    "HTTP/1.1 500 Internal Server Error",
                                    "application/json",
                                    r#"{"error":"temporary upstream failure"}"#,
                                )
                                .await;
                                return;
                            }
                        }

                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"success":true}"#,
                        )
                        .await;
                        return;
                    }

                    write_http_response(
                        &mut socket,
                        "HTTP/1.1 404 Not Found",
                        "text/plain",
                        "not found",
                    )
                    .await;
                });
            }
        }
    });

    let transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    transport
        .send(ToAgentMessage::Interrupt)
        .expect("send interrupt");

    let posted = wait_for_posted_bodies_len(&posted_bodies, 3).await;
    assert_eq!(interrupt_attempts.load(Ordering::SeqCst), 2);
    assert!(matches!(
        serde_json::from_str::<ToAgentMessage>(&posted[1]).expect("parse retryable interrupt"),
        ToAgentMessage::Interrupt
    ));
    assert!(matches!(
        serde_json::from_str::<ToAgentMessage>(&posted[2]).expect("parse successful retry"),
        ToAgentMessage::Interrupt
    ));

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_sends_utility_file_read_messages() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "state": {
            "protocol_version": "2026-03-30",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "active_tools": [],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    });

    let (addr, posted_bodies, _request_paths, _request_headers) =
        spawn_remote_headless_server(snapshot.to_string(), Vec::new()).await;

    let transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    transport
        .read_file(
            "read_src".to_string(),
            "src/main.rs".to_string(),
            Some("/tmp/project".to_string()),
            Some(10),
            Some(25),
        )
        .expect("send utility file read");

    let posted = wait_for_posted_bodies_len(&posted_bodies, 2).await;
    assert_eq!(posted.len(), 2);
    let sent = serde_json::from_str::<ToAgentMessage>(&posted[1]).expect("parse sent message");
    assert!(matches!(
        sent,
        ToAgentMessage::UtilityFileRead {
            read_id,
            path,
            cwd,
            offset,
            limit,
        } if read_id == "read_src"
            && path == "src/main.rs"
            && cwd.as_deref() == Some("/tmp/project")
            && offset == Some(10)
            && limit == Some(25)
    ));

    transport.shutdown().expect("shutdown");
}

#[tokio::test]
async fn remote_transport_applies_reset_events_as_snapshots() {
    let initial_snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 1,
        "last_init": {
            "system_prompt": "Initial prompt"
        },
        "state": {
            "protocol_version": "2026-03-30",
            "model": "gpt-5.6",
            "provider": "openai",
            "session_id": "sess_remote",
            "pending_approvals": [],
            "tracked_tools": [],
            "active_tools": [],
            "last_status": "Attached",
            "is_ready": true,
            "is_responding": false
        }
    });
    let reset_event = serde_json::json!({
        "type": "reset",
        "reason": "lagged",
        "snapshot": {
            "protocolVersion": "2026-03-30",
            "session_id": "sess_remote",
            "cursor": 2,
            "last_init": {
                "system_prompt": "Reset prompt"
            },
            "state": {
                "protocol_version": "2026-03-30",
                "model": "gpt-5.6",
                "provider": "openai",
                "session_id": "sess_remote",
                "pending_approvals": [],
                "tracked_tools": [],
                "active_tools": [],
                "last_status": "Reset snapshot",
                "is_ready": true,
                "is_responding": false
            }
        }
    })
    .to_string();

    let (addr, _posted_bodies, _request_paths, _request_headers) =
        spawn_remote_headless_server(initial_snapshot.to_string(), vec![reset_event]).await;

    let config = RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    };

    let mut transport = RemoteAgentTransport::connect(config)
        .await
        .expect("connect");

    let incoming = transport.recv_incoming().await.expect("incoming reset");
    match incoming {
        RemoteIncoming::Reset { reason, .. } => {
            assert_eq!(reason, "lagged");
        }
        other => panic!("expected remote reset, got {other:?}"),
    }

    assert_eq!(
        transport.state().last_status.as_deref(),
        Some("Reset snapshot")
    );
    assert_eq!(
        transport
            .last_init()
            .and_then(|init| init.system_prompt.as_deref()),
        Some("Reset prompt")
    );
}
