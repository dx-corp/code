use super::*;
use crate::headless::messages::{
    ActiveFileWatch, ActiveUtilityCommand, PendingApproval, UtilityCommandTerminalMode,
};
use crate::headless::session::{SessionEntry, SessionReader};
use crate::headless::{
    ActiveTool, HeadlessErrorType, StreamingResponse, TokenUsage, UtilityCommandShellMode,
};
use std::collections::VecDeque;
use std::fs;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[cfg(unix)]
use std::{os::unix::fs::PermissionsExt, path::Path};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

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

async fn spawn_remote_headless_server(
    snapshot_json: String,
    sse_events: Vec<String>,
) -> (
    std::net::SocketAddr,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<Vec<(String, String)>>>>,
    Arc<Mutex<Vec<(String, String)>>>,
) {
    spawn_remote_headless_server_with_options(snapshot_json, sse_events, None, None, false, false)
        .await
}

async fn spawn_remote_headless_server_with_options(
    snapshot_json: String,
    sse_events: Vec<String>,
    disconnect_response_delay: Option<Duration>,
    lifecycle_markers: Option<Arc<Mutex<Vec<String>>>>,
    enforce_connection_capability: bool,
    fail_disconnect: bool,
) -> (
    std::net::SocketAddr,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<Vec<(String, String)>>>>,
    Arc<Mutex<Vec<(String, String)>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posted_bodies = Arc::new(Mutex::new(Vec::new()));
    let request_headers = Arc::new(Mutex::new(Vec::new()));
    let request_bodies = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(VecDeque::from(sse_events)));
    let snapshot_value =
        serde_json::from_str::<serde_json::Value>(&snapshot_json).expect("valid snapshot json");
    let snapshot_session_id = snapshot_value
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("sess_remote")
        .to_string();

    tokio::spawn({
        let posted_bodies = Arc::clone(&posted_bodies);
        let request_headers = Arc::clone(&request_headers);
        let request_bodies = Arc::clone(&request_bodies);
        let events = Arc::clone(&events);
        let snapshot_session_id = snapshot_session_id.clone();
        let lifecycle_markers = lifecycle_markers.clone();
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let posted_bodies = Arc::clone(&posted_bodies);
                let request_headers = Arc::clone(&request_headers);
                let request_bodies = Arc::clone(&request_bodies);
                let events = Arc::clone(&events);
                let snapshot_json = snapshot_json.clone();
                let snapshot_session_id = snapshot_session_id.clone();
                let disconnect_response_delay = disconnect_response_delay;
                let lifecycle_markers = lifecycle_markers.clone();

                tokio::spawn(async move {
                    let Some((path, headers, body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    request_headers.lock().await.push(headers);
                    request_bodies
                        .lock()
                        .await
                        .push((path.clone(), body.clone()));

                    if path == "/api/headless/connections" {
                        if let Some(markers) = lifecycle_markers.as_ref() {
                            markers.lock().await.push("bootstrap".to_string());
                        }
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid connection request");
                        if enforce_connection_capability
                            && request
                                .get("connectionId")
                                .and_then(serde_json::Value::as_str)
                                == Some("conn_remote")
                            && request
                                .get("connectionCapability")
                                .and_then(serde_json::Value::as_str)
                                != Some("cap_remote")
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 403 Forbidden",
                                "application/json",
                                r#"{"error":"existing headless connection requires its private capability"}"#,
                            )
                            .await;
                            return;
                        }
                        let body = serde_json::json!({
                            "session_id": snapshot_session_id,
                            "connection_id": "conn_remote",
                            "connection_capability": enforce_connection_capability
                                .then_some("cap_remote"),
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
                        if let Some(markers) = lifecycle_markers.as_ref() {
                            markers.lock().await.push("disconnect:start".to_string());
                        }
                        if let Some(delay) = disconnect_response_delay {
                            tokio::time::sleep(delay).await;
                        }
                        if fail_disconnect {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 503 Service Unavailable",
                                "application/json",
                                r#"{"error":"connection teardown unavailable"}"#,
                            )
                            .await;
                            return;
                        }
                        write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                r#"{"success":true,"connection_id":"conn_remote","controller_connection_id":null,"disconnected_subscription_ids":["sub_remote"]}"#,
                            )
                            .await;
                        if let Some(markers) = lifecycle_markers.as_ref() {
                            markers.lock().await.push("disconnect:end".to_string());
                        }
                        return;
                    }

                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid subscription request");
                        if enforce_connection_capability
                            && request
                                .get("connectionCapability")
                                .and_then(serde_json::Value::as_str)
                                != Some("cap_remote")
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 403 Forbidden",
                                "application/json",
                                r#"{"error":"existing headless connection requires its private capability"}"#,
                            )
                            .await;
                            return;
                        }
                        let body = serde_json::json!({
                            "connection_id": "conn_remote",
                            "connection_capability": enforce_connection_capability
                                .then_some("cap_remote"),
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
                        while let Some(event) = events.lock().await.pop_front() {
                            let payload = format!("data: {event}\n\n");
                            if socket.write_all(payload.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
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

    (addr, posted_bodies, request_headers, request_bodies)
}

#[test]
fn test_supervisor_config_defaults() {
    let config = SupervisorConfig::default();
    assert_eq!(config.max_reconnect_attempts, 5);
    assert_eq!(config.reconnect_delay, Duration::from_secs(1));
    assert!((config.reconnect_jitter_factor - 0.25).abs() < f64::EPSILON);
    assert_eq!(config.max_reconnect_elapsed, Duration::from_mins(10));
    assert!(config.auto_reconnect);
}

#[test]
fn reconnect_jitter_sample_stays_within_expected_bounds() {
    let base_delay = Duration::from_secs(4);

    assert_eq!(
        jittered_reconnect_delay_for_sample(base_delay, 0.25, -1.0),
        Duration::from_secs(3)
    );
    assert_eq!(
        jittered_reconnect_delay_for_sample(base_delay, 0.25, 1.0),
        Duration::from_secs(5)
    );
    assert_eq!(
        jittered_reconnect_delay_for_sample(base_delay, 0.25, 0.0),
        base_delay
    );
}

#[test]
fn test_health_status() {
    assert_eq!(HealthStatus::Healthy, HealthStatus::Healthy);
    assert_ne!(HealthStatus::Healthy, HealthStatus::Unhealthy);
}

#[test]
fn health_transitions_after_silence() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig {
        health_check_interval: Duration::from_secs(30),
        health_check_timeout: Duration::from_secs(5),
        ..SupervisorConfig::default()
    });
    let start = Instant::now();
    supervisor.health_status = HealthStatus::Healthy;
    supervisor.last_response = Some(start);

    assert!(
        supervisor
            .due_health_transition(start + Duration::from_secs(29))
            .is_none()
    );
    assert!(matches!(
        supervisor.due_health_transition(start + Duration::from_secs(30)),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Degraded
        })
    ));
    assert_eq!(supervisor.health(), HealthStatus::Degraded);
    assert!(
        supervisor
            .due_health_transition(start + Duration::from_secs(34))
            .is_none()
    );
    assert!(matches!(
        supervisor.due_health_transition(start + Duration::from_secs(35)),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Unhealthy
        })
    ));
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
}

#[test]
fn remote_viewer_skips_silence_health_timeouts() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig {
        health_check_interval: Duration::from_secs(30),
        health_check_timeout: Duration::from_secs(5),
        remote: Some(RemoteTransportConfig {
            role: Some("viewer".to_string()),
            ..RemoteTransportConfig::default()
        }),
        ..SupervisorConfig::default()
    });
    let start = Instant::now();
    supervisor.health_status = HealthStatus::Healthy;
    supervisor.last_response = Some(start);

    assert!(supervisor.next_health_deadline().is_none());
    assert!(
        supervisor
            .due_health_transition(start + Duration::from_secs(35))
            .is_none()
    );
    assert_eq!(supervisor.health(), HealthStatus::Healthy);
}

#[test]
fn compacting_remote_sessions_use_extended_silence_deadline() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig {
        health_check_interval: Duration::from_secs(30),
        health_check_timeout: Duration::from_secs(5),
        remote: Some(RemoteTransportConfig {
            role: Some("controller".to_string()),
            ..RemoteTransportConfig::default()
        }),
        ..SupervisorConfig::default()
    });
    let start = Instant::now();
    supervisor.health_status = HealthStatus::Healthy;
    supervisor.last_response = Some(start);
    supervisor.state.is_responding = true;
    supervisor.state.last_status = Some(" compacting ".to_string());

    assert_eq!(
        supervisor.next_health_deadline(),
        Some(start + REMOTE_COMPACTION_SILENCE_TIMEOUT)
    );
    assert!(
        supervisor
            .due_health_transition(start + Duration::from_secs(35))
            .is_none()
    );
    assert_eq!(supervisor.health(), HealthStatus::Healthy);
}

#[test]
fn completed_remote_responses_do_not_keep_compaction_timeout_override() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig {
        health_check_interval: Duration::from_secs(30),
        health_check_timeout: Duration::from_secs(5),
        remote: Some(RemoteTransportConfig {
            role: Some("controller".to_string()),
            ..RemoteTransportConfig::default()
        }),
        ..SupervisorConfig::default()
    });
    let start = Instant::now();
    supervisor.health_status = HealthStatus::Healthy;
    supervisor.last_response = Some(start);
    supervisor.state.is_responding = false;
    supervisor.state.last_status = Some("compacting".to_string());

    assert_eq!(
        supervisor.next_health_deadline(),
        Some(start + Duration::from_secs(30))
    );
    assert!(matches!(
        supervisor.due_health_transition(start + Duration::from_secs(30)),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Degraded
        })
    ));
}

#[test]
fn compacting_remote_sessions_become_unhealthy_after_extended_timeout() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig {
        health_check_interval: Duration::from_secs(30),
        health_check_timeout: Duration::from_secs(5),
        remote: Some(RemoteTransportConfig {
            role: Some("controller".to_string()),
            ..RemoteTransportConfig::default()
        }),
        auto_reconnect: true,
        ..SupervisorConfig::default()
    });
    let start = Instant::now();
    supervisor.health_status = HealthStatus::Healthy;
    supervisor.last_response = Some(start);
    supervisor.state.is_responding = true;
    supervisor.state.last_status = Some("compacting".to_string());

    assert!(matches!(
        supervisor.due_health_transition(start + REMOTE_COMPACTION_SILENCE_TIMEOUT),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Unhealthy
        })
    ));
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
    assert!(supervisor.pending_auto_reconnect);
}

#[test]
fn incoming_heartbeat_restores_healthy_status() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    supervisor.health_status = HealthStatus::Degraded;
    supervisor.last_response = Some(
        Instant::now()
            .checked_sub(Duration::from_secs(90))
            .expect("monotonic clock supports subtraction"),
    );

    supervisor.mark_response_received(true);
    assert_eq!(supervisor.health(), HealthStatus::Healthy);
    assert!(matches!(
        supervisor.poll(),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
}

#[test]
fn retryable_transport_disconnect_preserves_transient_progress_state() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    supervisor.state.current_response = Some(StreamingResponse::new("resp_disconnect".into()));
    supervisor.state.is_responding = true;
    let approval = PendingApproval {
        call_id: "call_disconnect".to_string(),
        tool_execution_id: None,
        request_id: Some("req_disconnect".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "git push" }),
        started_at_ms: Some(1_771_000_000_000),
    };
    let client_tool = PendingApproval {
        call_id: "call_client_disconnect".to_string(),
        tool_execution_id: None,
        request_id: Some("req_client_disconnect".to_string()),
        tool: "client_tool".to_string(),
        args: serde_json::json!({ "name": "artifacts" }),
        started_at_ms: None,
    };
    let user_input = PendingApproval {
        call_id: "call_user_disconnect".to_string(),
        tool_execution_id: None,
        request_id: Some("req_user_disconnect".to_string()),
        tool: "ask_user".to_string(),
        args: serde_json::json!({ "question": "continue?" }),
        started_at_ms: None,
    };
    let tool_retry = PendingApproval {
        call_id: "call_retry_disconnect".to_string(),
        tool_execution_id: None,
        request_id: Some("req_retry_disconnect".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "retry" }),
        started_at_ms: None,
    };
    supervisor.state.pending_approvals.push(approval.clone());
    supervisor
        .state
        .pending_client_tools
        .push(client_tool.clone());
    supervisor
        .state
        .pending_user_inputs
        .push(user_input.clone());
    supervisor
        .state
        .pending_tool_retries
        .push(tool_retry.clone());
    supervisor
        .state
        .tracked_tools
        .insert(approval.call_id.clone(), approval);
    supervisor
        .state
        .tracked_tools
        .insert(client_tool.call_id.clone(), client_tool);
    supervisor
        .state
        .tracked_tools
        .insert(user_input.call_id.clone(), user_input);
    supervisor
        .state
        .tracked_tools
        .insert(tool_retry.call_id.clone(), tool_retry);
    supervisor.state.active_tools.insert(
        "call_disconnect".to_string(),
        ActiveTool {
            call_id: "call_disconnect".to_string(),
            tool: "read".to_string(),
            output: "partial".to_string(),
            started: Instant::now(),
        },
    );
    supervisor.state.active_utility_commands.insert(
        "cmd_disconnect".to_string(),
        ActiveUtilityCommand {
            command_id: "cmd_disconnect".to_string(),
            command: "sleep 10".to_string(),
            cwd: Some("/tmp/project".to_string()),
            shell_mode: UtilityCommandShellMode::Shell,
            terminal_mode: UtilityCommandTerminalMode::Pipe,
            pid: Some(42),
            columns: None,
            rows: None,
            owner_connection_id: Some("conn_remote".to_string()),
            output: "partial".to_string(),
        },
    );
    supervisor.state.active_file_watches.insert(
        "watch_disconnect".to_string(),
        ActiveFileWatch {
            watch_id: "watch_disconnect".to_string(),
            root_dir: "/tmp/project".to_string(),
            include_patterns: None,
            exclude_patterns: None,
            debounce_ms: 250,
            owner_connection_id: Some("conn_remote".to_string()),
        },
    );

    assert!(matches!(
        supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed),
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));

    assert!(supervisor.pending_auto_reconnect);
    assert!(supervisor.state().current_response.is_some());
    assert_eq!(supervisor.state().pending_approvals.len(), 1);
    assert_eq!(supervisor.state().pending_client_tools.len(), 1);
    assert_eq!(supervisor.state().pending_user_inputs.len(), 1);
    assert_eq!(supervisor.state().pending_tool_retries.len(), 1);
    assert_eq!(supervisor.state().active_tools.len(), 1);
    assert_eq!(supervisor.state().active_utility_commands.len(), 1);
    assert_eq!(supervisor.state().active_file_watches.len(), 1);
    assert_eq!(supervisor.state().tracked_tools.len(), 4);
    assert!(supervisor.state().is_responding);
}

#[test]
fn manual_disconnect_clears_transient_progress_state() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    supervisor.state.current_response = Some(StreamingResponse::new("resp_manual".into()));
    supervisor.state.is_responding = true;
    let approval = PendingApproval {
        call_id: "call_manual".to_string(),
        tool_execution_id: None,
        request_id: Some("req_manual".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "git push" }),
        started_at_ms: Some(1_771_000_000_000),
    };
    let client_tool = PendingApproval {
        call_id: "call_client_manual".to_string(),
        tool_execution_id: None,
        request_id: Some("req_client_manual".to_string()),
        tool: "client_tool".to_string(),
        args: serde_json::json!({ "name": "artifacts" }),
        started_at_ms: None,
    };
    let user_input = PendingApproval {
        call_id: "call_user_manual".to_string(),
        tool_execution_id: None,
        request_id: Some("req_user_manual".to_string()),
        tool: "ask_user".to_string(),
        args: serde_json::json!({ "question": "continue?" }),
        started_at_ms: None,
    };
    let tool_retry = PendingApproval {
        call_id: "call_retry_manual".to_string(),
        tool_execution_id: None,
        request_id: Some("req_retry_manual".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "retry" }),
        started_at_ms: None,
    };
    supervisor.state.pending_approvals.push(approval.clone());
    supervisor
        .state
        .pending_client_tools
        .push(client_tool.clone());
    supervisor
        .state
        .pending_user_inputs
        .push(user_input.clone());
    supervisor
        .state
        .pending_tool_retries
        .push(tool_retry.clone());
    supervisor
        .state
        .tracked_tools
        .insert(approval.call_id.clone(), approval);
    supervisor
        .state
        .tracked_tools
        .insert(client_tool.call_id.clone(), client_tool);
    supervisor
        .state
        .tracked_tools
        .insert(user_input.call_id.clone(), user_input);
    supervisor
        .state
        .tracked_tools
        .insert(tool_retry.call_id.clone(), tool_retry);
    supervisor.state.active_tools.insert(
        "call_manual".to_string(),
        ActiveTool {
            call_id: "call_manual".to_string(),
            tool: "write".to_string(),
            output: "partial".to_string(),
            started: Instant::now(),
        },
    );
    supervisor.state.active_utility_commands.insert(
        "cmd_manual".to_string(),
        ActiveUtilityCommand {
            command_id: "cmd_manual".to_string(),
            command: "tail -f log".to_string(),
            cwd: Some("/tmp/project".to_string()),
            shell_mode: UtilityCommandShellMode::Direct,
            terminal_mode: UtilityCommandTerminalMode::Pty,
            pid: Some(7),
            columns: Some(80),
            rows: Some(24),
            owner_connection_id: Some("conn_remote".to_string()),
            output: "partial".to_string(),
        },
    );
    supervisor.state.active_file_watches.insert(
        "watch_manual".to_string(),
        ActiveFileWatch {
            watch_id: "watch_manual".to_string(),
            root_dir: "/tmp/project".to_string(),
            include_patterns: Some(vec!["src/**".to_string()]),
            exclude_patterns: None,
            debounce_ms: 250,
            owner_connection_id: Some("conn_remote".to_string()),
        },
    );

    supervisor.disconnect();

    assert!(supervisor.state().current_response.is_none());
    assert!(supervisor.state().pending_approvals.is_empty());
    assert!(supervisor.state().pending_client_tools.is_empty());
    assert!(supervisor.state().pending_user_inputs.is_empty());
    assert!(supervisor.state().pending_tool_retries.is_empty());
    assert!(supervisor.state().active_tools.is_empty());
    assert!(supervisor.state().active_utility_commands.is_empty());
    assert!(supervisor.state().active_file_watches.is_empty());
    assert!(supervisor.state().tracked_tools.is_empty());
    assert!(!supervisor.state().is_responding);
}

#[cfg(unix)]
#[tokio::test]
async fn send_resets_health_deadline_before_remote_reply_arrives() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_test_headless_script(temp.path()).expect("script");

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.health_check_interval = Duration::from_millis(10);
    config.health_check_timeout = Duration::from_millis(10);
    config.auto_reconnect = false;
    let degraded_timeout = config.health_check_interval + config.health_check_timeout;

    let mut supervisor = AgentSupervisor::new(config);
    supervisor.connect().await.expect("connect");

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    let _ = supervisor.recv().await.expect("ready");

    supervisor.health_status = HealthStatus::Degraded;
    let stale_response_at = Instant::now()
        .checked_sub(Duration::from_millis(25))
        .expect("monotonic clock supports subtraction");
    supervisor.last_response = Some(stale_response_at);

    supervisor.prompt("hello after idle").expect("prompt");

    let stale_deadline = stale_response_at + degraded_timeout;
    assert!(supervisor.due_health_transition(stale_deadline).is_none());
    assert_eq!(supervisor.health(), HealthStatus::Degraded);
}

#[test]
fn transport_disconnect_schedules_auto_reconnect_when_enabled() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let event = supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed);

    assert!(matches!(
        event,
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));
    assert_eq!(supervisor.health(), HealthStatus::Reconnecting);
    assert!(supervisor.pending_auto_reconnect);
}

#[test]
fn test_supervisor_builder() {
    let supervisor = SupervisorBuilder::new()
        .cli_path("/usr/bin/composer")
        .cwd("/home/user/project")
        .env("MAESTRO_EVALOPS_ACCESS_TOKEN", "delegated-token")
        .max_reconnect_attempts(10)
        .auto_reconnect(false)
        .build();

    assert!(!supervisor.is_connected());
    assert_eq!(supervisor.health(), HealthStatus::Unknown);
    assert!(supervisor.state().model.is_none());
    assert_eq!(
        supervisor.config.transport.env,
        vec![(
            "MAESTRO_EVALOPS_ACCESS_TOKEN".to_string(),
            "delegated-token".to_string(),
        )]
    );
}

#[test]
fn test_supervisor_builder_restores_session_replay() {
    let replay = SessionReplay {
        state: AgentState {
            model: Some("claude-3-opus".to_string()),
            provider: Some("anthropic".to_string()),
            is_ready: true,
            ..AgentState::default()
        },
        last_init: Some(InitConfig {
            system_prompt: Some("You are Maestro".to_string()),
            append_system_prompt: None,
            thinking_level: Some(super::super::messages::ThinkingLevel::High),
            approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
            history: None,
            code_mode: None,
            tool_grant: None,
        }),
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    };

    let supervisor = SupervisorBuilder::new().session_replay(replay).build();
    assert_eq!(supervisor.state().model.as_deref(), Some("claude-3-opus"));
    assert_eq!(supervisor.state().provider.as_deref(), Some("anthropic"));
    assert!(supervisor.state().is_ready);
}

#[test]
fn session_replay_does_not_override_explicit_remote_session_id() {
    let replay = SessionReplay {
        state: AgentState {
            session_id: Some("sess_replayed".to_string()),
            is_ready: true,
            ..AgentState::default()
        },
        last_init: None,
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    };

    let supervisor = SupervisorBuilder::new()
        .remote_base_url("http://127.0.0.1:8080")
        .remote_session_id("sess_explicit")
        .session_replay(replay)
        .build();

    assert_eq!(
        supervisor
            .config
            .remote
            .as_ref()
            .and_then(|remote| remote.session_id.as_deref()),
        Some("sess_explicit")
    );
    assert_eq!(
        supervisor.state().session_id.as_deref(),
        Some("sess_replayed")
    );
}

#[test]
fn snapshot_incoming_preserves_existing_event_queue_order() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let _ = supervisor.event_tx.send(SupervisorEvent::Connected);

    let event = supervisor.handle_transport_incoming(ManagedIncoming::Snapshot {
        state: Box::new(AgentState {
            session_id: Some("sess_snapshot".to_string()),
            ..AgentState::default()
        }),
        last_init: None,
    });

    assert!(event.is_none());
    assert!(matches!(
        supervisor.poll(),
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.poll(),
        Some(SupervisorEvent::StateHydrated {
            session_id: Some(ref session_id)
        }) if session_id == "sess_snapshot"
    ));
}

#[test]
fn semantic_restore_is_not_sent_without_a_saved_init_boundary() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    supervisor.semantic_conversation = Some(vec![]);

    // If replay attempted to send RestoreConversation this would fail because
    // no transport is attached. A facade-only session must not restore provider
    // history before Init has established the runtime boundary.
    assert!(supervisor.replay_saved_semantic_conversation().is_ok());
}

#[test]
fn unsupported_live_semantic_snapshot_clears_reconnect_history() {
    let temp = tempfile::tempdir().expect("tempdir");
    let recorder = SessionRecorder::new(temp.path()).expect("recorder");
    let mut supervisor =
        AgentSupervisor::new(SupervisorConfig::default()).with_session_recorder(recorder);
    let supported = vec![maestro_ai::Message {
        role: maestro_ai::Role::User,
        content: maestro_ai::MessageContent::text("first turn"),
    }];
    assert!(
        supervisor
            .apply_agent_message(FromAgentMessage::ConversationSnapshot {
                processed_queue_ids: vec![7, 9],
                protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                    .to_owned(),
                messages: supported,
            })
            .is_none(),
        "private checkpoints must not become supervisor events"
    );
    assert!(supervisor.semantic_conversation.is_some());

    supervisor.apply_agent_message(FromAgentMessage::ConversationSnapshot {
        processed_queue_ids: vec![7, 9],
        protocol_version: "evalops.maestro.semantic-conversation.v999".to_owned(),
        messages: vec![],
    });
    assert!(supervisor.semantic_conversation.is_none());
}

#[test]
fn replay_without_recorder_clears_history_after_unsupported_live_snapshot() {
    let replay = SessionReplay {
        state: AgentState::default(),
        last_init: Some(InitConfig {
            system_prompt: Some("restore boundary".to_owned()),
            append_system_prompt: None,
            thinking_level: None,
            approval_mode: None,
            history: None,
            code_mode: None,
            tool_grant: None,
        }),
        semantic_conversation: Some(vec![maestro_ai::Message {
            role: maestro_ai::Role::User,
            content: maestro_ai::MessageContent::text("stale reconnect history"),
        }]),
        last_workspace_capability_set: None,
        last_process_budget: None,
    };
    let mut supervisor =
        AgentSupervisor::new(SupervisorConfig::default()).with_session_replay(replay);

    supervisor.apply_agent_message(FromAgentMessage::ConversationSnapshot {
        processed_queue_ids: vec![7, 9],
        protocol_version: "evalops.maestro.semantic-conversation.v999".to_owned(),
        messages: vec![],
    });

    assert!(
        supervisor.semantic_conversation.is_none(),
        "an unsupported live checkpoint must clear replayed history before reconnect"
    );
}

#[test]
fn test_supervisor_builder_remote_config_helpers() {
    let supervisor = SupervisorBuilder::new()
        .remote_base_url("http://127.0.0.1:8080")
        .remote_api_key("secret")
        .remote_session_id("sess_remote")
        .remote_opt_out_notification("status")
        .build();

    let remote = supervisor.config.remote.expect("remote config");
    assert_eq!(remote.base_url, "http://127.0.0.1:8080");
    assert_eq!(remote.api_key.as_deref(), Some("secret"));
    assert_eq!(remote.session_id.as_deref(), Some("sess_remote"));
    assert_eq!(remote.opt_out_notifications, vec!["status".to_string()]);
}

#[test]
fn resume_recorded_session_restores_replay_and_recorder() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path();
    let mut recorder = SessionRecorder::new(sessions_dir).expect("recorder");
    let session_id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("Saved system prompt".to_string()),
            append_system_prompt: None,
            thinking_level: Some(super::super::messages::ThinkingLevel::Medium),
            approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
            history: None,
        })
        .expect("record init");
    recorder
        .record_received(&FromAgentMessage::Ready {
            protocol_version: Some("2026-03-30".to_string()),
            model: "gpt-5.6".to_string(),
            provider: "openai".to_string(),
            session_id: Some("sess_saved".to_string()),
        })
        .expect("record ready");
    recorder.flush().expect("flush");
    drop(recorder);

    let supervisor = SupervisorBuilder::new()
        .resume_recorded_session(sessions_dir, &session_id)
        .expect("resume builder")
        .build();

    assert_eq!(supervisor.state().model.as_deref(), Some("gpt-5.6"));
    assert_eq!(supervisor.state().provider.as_deref(), Some("openai"));
    assert_eq!(supervisor.state().session_id.as_deref(), Some("sess_saved"));
    assert!(supervisor.session_recorder.is_some());
    assert_eq!(
        supervisor
            .last_init
            .as_ref()
            .and_then(|init| init.system_prompt.as_deref()),
        Some("Saved system prompt")
    );
}

/// Regression test: `resume_recorded_session` must build its replay
/// snapshot from the already-resumed `SessionRecorder`, not a second,
/// independent `SessionReader::load`. The old code loaded a `SessionReader`
/// first (rotating aside the corrupt metadata file as a side effect of its
/// own tolerant load), then called `SessionRecorder::resume`, which by then
/// saw a *missing* -- not corrupt -- metadata file, skipped its own
/// rebuild-from-JSONL path, and quietly reset the session's historical
/// title and message count in the recorder's in-memory metadata, which the
/// next `flush()` would then persist over the real history.
#[test]
fn resume_recorded_session_rebuilds_metadata_after_corruption() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path();
    let mut recorder = SessionRecorder::new(sessions_dir).expect("recorder");
    let session_id = recorder.id().to_string();
    recorder
        .record_sent(&ToAgentMessage::Prompt {
            content: "Recovered title".to_string(),
            attachments: None,
            managed_inference_authorization: None,
        })
        .expect("record prompt");
    recorder.flush().expect("flush");
    drop(recorder);

    // Simulate a crash mid-write of the metadata file: truncated, invalid
    // JSON, exactly what `write_atomic` prevents going forward but a prior
    // build (or a non-atomic-write code path) could have left behind.
    let meta_path = sessions_dir.join(format!("{session_id}.meta.json"));
    std::fs::write(&meta_path, "{\"id\":\"partial").expect("write torn metadata");

    let supervisor = SupervisorBuilder::new()
        .resume_recorded_session(sessions_dir, &session_id)
        .expect("resume builder must tolerate corrupt metadata")
        .build();

    let recorder = supervisor
        .session_recorder
        .as_ref()
        .expect("recorder must be present after resume");
    assert_eq!(
        recorder.metadata().title.as_deref(),
        Some("Recovered title"),
        "title must be rebuilt from the JSONL log, not reset to default"
    );
    assert_eq!(recorder.metadata().message_count, 1);
}

#[test]
fn session_recorder_keeps_last_init_in_sync_with_sent_messages() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path();
    let mut recorder = SessionRecorder::new(sessions_dir).expect("recorder");
    recorder
        .record_sent(&ToAgentMessage::Init {
            system_prompt: Some("Saved system prompt".to_string()),
            append_system_prompt: None,
            thinking_level: Some(super::super::messages::ThinkingLevel::Medium),
            approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
            history: None,
        })
        .expect("record init");
    recorder.flush().expect("flush");
    let session_id = recorder.id().to_string();
    drop(recorder);

    let replay = SessionReader::load(sessions_dir, &session_id)
        .expect("load session")
        .replay();
    let recorder = SessionRecorder::resume(sessions_dir, &session_id).expect("resume recorder");

    assert_eq!(recorder.last_init(), replay.last_init.as_ref());
}

#[test]
fn session_recorder_failures_are_reported_once_until_a_write_recovers() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());

    assert!(
        supervisor
            .session_recorder_error_to_report(Err(std::io::Error::other("disk full")))
            .is_some()
    );
    assert!(
        supervisor
            .session_recorder_error_to_report(Err(std::io::Error::other("disk still full")))
            .is_none()
    );
    assert!(
        supervisor
            .session_recorder_error_to_report(Ok(()))
            .is_none()
    );
    assert!(
        supervisor
            .session_recorder_error_to_report(Err(std::io::Error::other("disk full again")))
            .is_some()
    );
}

/// Regression test for the recorder-diagnostic blocking finding: a plain
/// `writeln!(std::io::stderr(), ...)` on this hot path takes stderr's
/// process-global lock and performs a real write syscall, which blocks the
/// caller (here, `send`/`apply_snapshot`/etc.) if a headless parent pipes
/// stderr and stops draining it. Diagnostics use bounded `try_send`, so even
/// a full queue must return immediately rather than waiting for queue space.
#[test]
fn report_diagnostic_nonblocking_drops_when_its_queue_is_full() {
    let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
    assert!(enqueue_diagnostic(&sender, "fills queue".to_string()));

    let start = std::time::Instant::now();
    assert!(!enqueue_diagnostic(&sender, "must be dropped".to_string()));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(100),
        "a full diagnostic queue must not block the caller"
    );
}

#[test]
fn dropped_recorder_diagnostic_rearms_failure_reporting() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let reports = std::cell::Cell::new(0);

    supervisor.report_session_recorder_result_with(
        Err(std::io::Error::other("disk full")),
        "record sent message",
        |_: String| {
            reports.set(reports.get() + 1);
            false
        },
    );
    supervisor.report_session_recorder_result_with(
        Err(std::io::Error::other("disk still full")),
        "record sent message",
        |_: String| {
            reports.set(reports.get() + 1);
            true
        },
    );
    supervisor.report_session_recorder_result_with(
        Err(std::io::Error::other("disk remains full")),
        "record sent message",
        |_: String| {
            reports.set(reports.get() + 1);
            true
        },
    );

    assert_eq!(reports.get(), 2, "a dropped diagnostic must rearm once");
}

#[test]
fn report_diagnostic_nonblocking_reuses_one_process_worker() {
    let first = diagnostic_sender().expect("diagnostic worker should start");
    let second = diagnostic_sender().expect("diagnostic worker should be reused");
    assert!(std::ptr::eq(first, second));
}

#[test]
fn terminal_diagnostics_strip_control_sequences() {
    let message = "write failed for \u{1b}]0;spoofed\u{7}path\rforged\nline".to_string();
    assert_eq!(
        prepare_diagnostic_for_stderr(message, true),
        "write failed for ]0;spoofedpath\\rforged\\nline"
    );
}

#[test]
fn redirected_diagnostics_preserve_machine_readable_bytes() {
    let message = "write failed for \u{1b}]0;spoofed\u{7}path".to_string();
    assert_eq!(
        prepare_diagnostic_for_stderr(message.clone(), false),
        message
    );
}

#[test]
fn apply_snapshot_keeps_session_recorder_state_in_sync() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path();
    let recorder = SessionRecorder::new(sessions_dir).expect("recorder");
    let mut supervisor =
        AgentSupervisor::new(SupervisorConfig::default()).with_session_recorder(recorder);
    let snapshot_state = AgentState {
        model: Some("gpt-5.6".to_string()),
        provider: Some("openai".to_string()),
        session_id: Some("sess_remote".to_string()),
        is_ready: true,
        ..AgentState::default()
    };
    let snapshot_init = InitConfig {
        system_prompt: Some("Persisted prompt".to_string()),
        append_system_prompt: None,
        thinking_level: Some(super::super::messages::ThinkingLevel::High),
        approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
        history: None,
        code_mode: None,
        tool_grant: None,
    };

    supervisor.apply_snapshot(snapshot_state.clone(), Some(snapshot_init.clone()));

    let recorder = supervisor
        .session_recorder
        .as_ref()
        .expect("session recorder");
    assert_eq!(recorder.replay_state().model, snapshot_state.model);
    assert_eq!(recorder.replay_state().provider, snapshot_state.provider);
    assert_eq!(
        recorder.replay_state().session_id,
        snapshot_state.session_id
    );
    assert_eq!(recorder.replay_state().is_ready, snapshot_state.is_ready);
    assert_eq!(recorder.last_init(), Some(&snapshot_init));
    assert_eq!(supervisor.last_init.as_ref(), Some(&snapshot_init));
}

#[test]
fn disconnect_writes_one_checkpoint_after_clearing_disconnect_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path();
    let recorder = SessionRecorder::new(sessions_dir).expect("recorder");
    let session_id = recorder.id().to_string();
    let mut supervisor =
        AgentSupervisor::new(SupervisorConfig::default()).with_session_recorder(recorder);
    let approval = PendingApproval {
        call_id: "call_disconnect".to_string(),
        tool_execution_id: None,
        request_id: Some("req_disconnect".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "git push" }),
        started_at_ms: Some(1_771_000_000_000),
    };

    supervisor.state.current_response = Some(StreamingResponse::new("resp_disconnect".into()));
    supervisor.state.is_responding = true;
    supervisor.state.pending_approvals.push(approval.clone());
    supervisor
        .state
        .tracked_tools
        .insert(approval.call_id.clone(), approval);
    supervisor.state.active_tools.insert(
        "call_disconnect".to_string(),
        ActiveTool {
            call_id: "call_disconnect".to_string(),
            tool: "grep".to_string(),
            output: "partial".to_string(),
            started: Instant::now(),
        },
    );

    supervisor.disconnect();
    supervisor
        .session_recorder
        .as_mut()
        .expect("session recorder")
        .flush()
        .expect("flush");
    drop(supervisor);

    let reader = SessionReader::load(sessions_dir, &session_id).expect("load session");
    let checkpoints = reader
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Checkpoint { state, .. } => Some(state.as_ref()),
            SessionEntry::Sent { .. } | SessionEntry::Received { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    let checkpoint = checkpoints[0];
    assert!(checkpoint.current_response.is_none());
    assert!(!checkpoint.is_responding);
    assert!(checkpoint.pending_approvals.is_empty());
    assert!(checkpoint.tracked_tools.is_empty());
    assert!(checkpoint.active_tools.is_empty());
}

#[test]
fn agent_event_to_message_preserves_headless_metadata() {
    let ready = agent_event_to_message(&AgentEvent::Ready {
        protocol_version: Some("2026-03-30".to_string()),
        model: "claude-3-opus".to_string(),
        provider: "anthropic".to_string(),
        session_id: Some("sess_123".to_string()),
    });
    assert!(matches!(
        ready,
        super::super::messages::FromAgentMessage::Ready {
            protocol_version: Some(ref version),
            session_id: Some(ref session_id),
            ..
        } if version == "2026-03-30" && session_id == "sess_123"
    ));

    let response_end = agent_event_to_message(&AgentEvent::ResponseEnd {
        response_id: "resp_1".to_string(),
        usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: Some(0.1),
            total_tokens: Some(30),
            model_id: Some("claude-sonnet".to_string()),
            provider: Some("anthropic".to_string()),
        }),
        tools_summary: None,
        duration_ms: Some(2400),
        ttft_ms: Some(150),
        full_text: Some("Hello".to_string()),
    });
    assert!(matches!(
        response_end,
        super::super::messages::FromAgentMessage::ResponseEnd {
            duration_ms: Some(2400),
            ttft_ms: Some(150),
            ..
        }
    ));

    let provider_error = agent_event_to_message(&AgentEvent::ProviderError {
        kind: maestro_ai::ProviderStreamErrorKind::OutputTokenExhaustion,
        message: "output exhausted".to_string(),
    });
    assert!(matches!(
        provider_error,
        super::super::messages::FromAgentMessage::ProviderError {
            kind: maestro_ai::ProviderStreamErrorKind::OutputTokenExhaustion,
            ref message,
        } if message == "output exhausted"
    ));

    let receipt = crate::agent::protocol::ToolExecution::from_legacy(
        "call_1",
        "codex_file_change",
        crate::agent::protocol::ExecutionSource::Native,
        crate::agent::protocol::ToolResult::success("completed"),
    )
    .receipt;
    let tool_end = agent_event_to_message(&AgentEvent::ToolEnd {
        call_id: "call_1".to_string(),
        tool_execution_id: Some("tool-execution-1".to_string()),
        success: true,
        duration: None,
        receipt: Some(receipt),
    });
    assert!(matches!(
        tool_end,
        super::super::messages::FromAgentMessage::ToolEnd {
            call_id,
            tool_execution_id: Some(ref tool_execution_id),
            tool: Some(ref tool),
            success: true,
            ..
        } if call_id == "call_1"
            && tool_execution_id == "tool-execution-1"
            && tool == "codex_file_change"
    ));

    let error = agent_event_to_message(&AgentEvent::Error {
        request_id: None,
        message: "cancelled".to_string(),
        fatal: false,
        terminal: true,
        error_type: Some(HeadlessErrorType::Cancelled),
    });
    assert!(matches!(
        error,
        super::super::messages::FromAgentMessage::Error {
            terminal: true,
            error_type: Some(HeadlessErrorType::Cancelled),
            ..
        }
    ));
}

#[test]
fn drain_available_agent_messages_skips_supervisor_lifecycle_events() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    supervisor
        .event_tx
        .send(SupervisorEvent::Connected)
        .expect("connected event");
    supervisor
        .event_tx
        .send(SupervisorEvent::Agent(Box::new(AgentEvent::Status {
            message: "queued status".to_string(),
        })))
        .expect("status event");
    supervisor
        .event_tx
        .send(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy,
        })
        .expect("health event");
    supervisor
        .event_tx
        .send(SupervisorEvent::Agent(Box::new(
            AgentEvent::ResponseStart {
                response_id: "resp_drain".to_string(),
            },
        )))
        .expect("response start event");

    let messages = supervisor.drain_available_agent_messages();

    assert_eq!(messages.len(), 2);
    assert!(matches!(
        messages[0],
        super::super::messages::FromAgentMessage::Status { ref message }
            if message == "queued status"
    ));
    assert!(matches!(
        messages[1],
        super::super::messages::FromAgentMessage::ResponseStart { ref response_id }
            if response_id == "resp_drain"
    ));
    assert!(supervisor.poll().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_drain_waits_for_a_delayed_response_acknowledgement() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    assert!(supervisor.register_response_acknowledgement("request-delayed"));
    let event_tx = supervisor.event_tx.clone();
    let supervisor = Arc::new(std::sync::Mutex::new(supervisor));
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        event_tx
            .send(SupervisorEvent::ResponseAccepted {
                request_id: "request-delayed".to_string(),
            })
            .expect("delayed response acknowledgement");
    });

    let (messages, acknowledgement) = AgentSupervisor::wait_for_response_acknowledgement_async(
        Arc::clone(&supervisor),
        "request-delayed".to_string(),
        Duration::from_millis(200),
    )
    .await;

    sender.join().expect("acknowledgement sender");
    assert_eq!(acknowledgement, ResponseAcknowledgement::Consumed);
    assert!(messages.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_gated_response_stays_queued_instead_of_failing_and_dispatching_twice() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    assert!(supervisor.register_response_acknowledgement("gated-response"));
    let event_tx = supervisor.event_tx.clone();
    let supervisor = Arc::new(std::sync::Mutex::new(supervisor));
    let executions = Arc::new(AtomicUsize::new(0));
    let delayed_executions = Arc::clone(&executions);
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(650));
        delayed_executions.fetch_add(1, Ordering::SeqCst);
        event_tx
            .send(SupervisorEvent::ResponseAccepted {
                request_id: "gated-response".to_string(),
            })
            .expect("late native consumption acknowledgement");
    });

    let (messages, acknowledgement) = AgentSupervisor::wait_for_response_acknowledgement_async(
        Arc::clone(&supervisor),
        "gated-response".to_string(),
        Duration::from_millis(500),
    )
    .await;

    assert_eq!(acknowledgement, ResponseAcknowledgement::Queued);
    assert!(messages.is_empty());
    sender.join().expect("delayed gated response");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let (_, acknowledgement) = supervisor
        .lock()
        .expect("supervisor")
        .wait_for_response_acknowledgement("gated-response");
    assert_eq!(acknowledgement, ResponseAcknowledgement::Consumed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_delayed_ack_waiters_do_not_starve_reader_progress() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    assert!(supervisor.register_response_acknowledgement("concurrent-a"));
    assert!(supervisor.register_response_acknowledgement("concurrent-b"));
    let event_tx = supervisor.event_tx.clone();
    let supervisor = Arc::new(std::sync::Mutex::new(supervisor));
    let waiter_a = tokio::spawn(AgentSupervisor::wait_for_response_acknowledgement_async(
        Arc::clone(&supervisor),
        "concurrent-a".to_string(),
        Duration::from_millis(300),
    ));
    let waiter_b = tokio::spawn(AgentSupervisor::wait_for_response_acknowledgement_async(
        Arc::clone(&supervisor),
        "concurrent-b".to_string(),
        Duration::from_millis(300),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    event_tx
        .send(SupervisorEvent::ResponseAccepted {
            request_id: "concurrent-b".to_string(),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    event_tx
        .send(SupervisorEvent::ResponseAccepted {
            request_id: "concurrent-a".to_string(),
        })
        .unwrap();

    assert_eq!(waiter_a.await.unwrap().1, ResponseAcknowledgement::Consumed);
    assert_eq!(waiter_b.await.unwrap().1, ResponseAcknowledgement::Consumed);
}

#[test]
fn out_of_order_response_acknowledgements_are_buffered_by_request_id() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    assert!(supervisor.register_response_acknowledgement("request-a"));
    assert!(supervisor.register_response_acknowledgement("request-b"));
    supervisor
        .event_tx
        .send(SupervisorEvent::ResponseAccepted {
            request_id: "request-b".to_string(),
        })
        .expect("B acknowledgement");
    supervisor
        .event_tx
        .send(SupervisorEvent::ResponseAccepted {
            request_id: "request-a".to_string(),
        })
        .expect("A acknowledgement");

    let (_, acknowledgement_a) =
        supervisor.drain_agent_messages_until_ack(Some("request-a"), Duration::from_millis(20));
    let (_, acknowledgement_b) =
        supervisor.drain_agent_messages_until_ack(Some("request-b"), Duration::from_millis(20));

    assert_eq!(acknowledgement_a, ResponseAcknowledgement::Consumed);
    assert_eq!(acknowledgement_b, ResponseAcknowledgement::Consumed);
    assert!(supervisor.pending_response_acknowledgements.is_empty());
}

#[test]
fn acknowledgement_caps_drop_unowned_ids_without_evicting_expected_ownership() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    for index in 0..MAX_RESPONSE_ACKNOWLEDGEMENTS {
        assert!(supervisor.register_response_acknowledgement(&format!("owned-{index}")));
    }
    assert!(!supervisor.register_response_acknowledgement("owned-overflow"));
    for index in 0..(MAX_RESPONSE_ACKNOWLEDGEMENTS * 2) {
        supervisor
            .event_tx
            .send(SupervisorEvent::ResponseAccepted {
                request_id: format!("unowned-{index}"),
            })
            .unwrap();
    }
    let _ = supervisor.drain_available_agent_messages();
    assert!(supervisor.pending_response_acknowledgements.is_empty());
    assert_eq!(
        supervisor.expected_response_acknowledgements.len(),
        MAX_RESPONSE_ACKNOWLEDGEMENTS
    );

    supervisor
        .event_tx
        .send(SupervisorEvent::ResponseAccepted {
            request_id: "owned-0".to_string(),
        })
        .unwrap();
    let _ = supervisor.drain_available_agent_messages();
    assert!(supervisor.has_response_acknowledgement("owned-0"));
    assert_eq!(
        supervisor.expected_response_acknowledgements.len(),
        MAX_RESPONSE_ACKNOWLEDGEMENTS
    );
}

#[test]
fn stale_generation_cleanup_preserves_reused_request_acknowledgement() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let stale_generation = supervisor.transport_generation();
    assert!(supervisor.register_response_acknowledgement("reused-request"));
    for index in 1..MAX_RESPONSE_ACKNOWLEDGEMENTS {
        assert!(supervisor.register_response_acknowledgement(&format!("owned-{index}")));
    }

    supervisor.transport_generation += 1;
    assert!(supervisor.register_response_acknowledgement("reused-request"));
    supervisor.discard_response_acknowledgement("reused-request", stale_generation);
    assert_eq!(
        supervisor.response_acknowledgement_count(),
        MAX_RESPONSE_ACKNOWLEDGEMENTS
    );

    supervisor
        .event_tx
        .send(SupervisorEvent::ResponseAccepted {
            request_id: "reused-request".to_string(),
        })
        .unwrap();
    let _ = supervisor.drain_available_agent_messages();
    assert!(supervisor.take_response_acknowledgement("reused-request"));
    assert_eq!(
        supervisor.response_acknowledgement_count(),
        MAX_RESPONSE_ACKNOWLEDGEMENTS - 1
    );
}

#[cfg(unix)]
fn create_test_headless_script(dir: &Path) -> std::io::Result<std::path::PathBuf> {
    let script_path = dir.join("fake-maestro-headless.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
log_file="${MAESTRO_TEST_LOG:-}"
if [ -n "$log_file" ]; then
  : > "$log_file"
fi
printf '{"type":"ready","model":"test","provider":"test"}\n'
while IFS= read -r line; do
  if [ -n "$log_file" ]; then
    printf '%s\n' "$line" >> "$log_file"
  fi
done
"#,
    )?;

    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;
    Ok(script_path)
}

#[cfg(unix)]
fn create_delayed_ack_headless_script(dir: &Path) -> std::io::Result<std::path::PathBuf> {
    let script_path = dir.join("fake-maestro-delayed-ack.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
log_file="${MAESTRO_TEST_LOG:-}"
: > "$log_file"
printf '{"type":"ready","model":"test","provider":"test"}\n'
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log_file"
  case "$line" in
    *'"type":"tool_response"'*)
      sleep 0.65
      printf '{"type":"response_accepted","request_id":"gated-call"}\n'
      ;;
  esac
done
"#,
    )?;
    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;
    Ok(script_path)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_native_tool_consumption_is_queued_success_with_one_dispatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_delayed_ack_headless_script(temp.path()).expect("script");
    let log_path = temp.path().join("messages.log");
    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.transport.env.push((
        "MAESTRO_TEST_LOG".to_string(),
        log_path.display().to_string(),
    ));
    config.auto_reconnect = false;
    let mut supervisor = AgentSupervisor::new(config);
    supervisor.connect().await.expect("connect");
    let _ = supervisor.recv().await.expect("connected");
    let _ = supervisor.recv().await.expect("healthy");
    let _ = supervisor.recv().await.expect("ready");

    let (_messages, acknowledgement) = supervisor
        .send_and_drain_agent_messages_with_ack(ToAgentMessage::ToolResponse {
            call_id: "gated-call".to_string(),
            tool_execution_id: Some("gated-execution".to_string()),
            approved: true,
            result: None,
        })
        .expect("queue gated tool response");

    assert_eq!(acknowledgement, ResponseAcknowledgement::Queued);
    let supervisor = Arc::new(std::sync::Mutex::new(supervisor));
    let (_messages, acknowledgement) = AgentSupervisor::wait_for_response_acknowledgement_async(
        Arc::clone(&supervisor),
        "gated-call".to_string(),
        Duration::from_millis(800),
    )
    .await;
    assert_eq!(acknowledgement, ResponseAcknowledgement::Consumed);
    assert_eq!(
        fs::read_to_string(log_path)
            .expect("message log")
            .lines()
            .filter(|line| line.contains("\"type\":\"tool_response\""))
            .count(),
        1,
        "the queued response must not be dispatched a second time while awaiting consumption"
    );
    supervisor.lock().expect("supervisor").shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[cfg(unix)]
fn create_streaming_headless_script(dir: &Path) -> std::io::Result<std::path::PathBuf> {
    let script_path = dir.join("fake-maestro-headless-stream.sh");
    fs::write(
        &script_path,
        r#"#!/bin/sh
printf '{"type":"ready","protocol_version":"2026-03-30","model":"test","provider":"test","session_id":"sess_ready"}\n'
printf '{"type":"session_info","session_id":"sess_state","cwd":"/tmp/project","git_branch":"main"}\n'
printf '{"type":"response_start","response_id":"resp_1"}\n'
printf '{"type":"response_chunk","response_id":"resp_1","content":"Partial reply","is_thinking":false}\n'
while IFS= read -r line; do
  :
done
"#,
    )?;

    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;
    Ok(script_path)
}

#[cfg(unix)]
#[tokio::test]
async fn supervisor_recv_updates_replayed_state_from_transport_events() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_streaming_headless_script(temp.path()).expect("script");

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.auto_reconnect = false;

    let mut supervisor = AgentSupervisor::new(config);
    supervisor.connect().await.expect("connect");

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    for _ in 0..4 {
        let event = supervisor.recv().await.expect("event");
        assert!(matches!(event, SupervisorEvent::Agent(_)));
    }

    assert_eq!(
        supervisor.state().protocol_version.as_deref(),
        Some("2026-03-30")
    );
    assert_eq!(supervisor.state().model.as_deref(), Some("test"));
    assert_eq!(supervisor.state().provider.as_deref(), Some("test"));
    assert_eq!(supervisor.state().session_id.as_deref(), Some("sess_state"));
    assert_eq!(supervisor.state().cwd.as_deref(), Some("/tmp/project"));
    assert_eq!(supervisor.state().git_branch.as_deref(), Some("main"));
    assert!(supervisor.state().is_ready);
    assert!(supervisor.state().is_responding);
    assert_eq!(
        supervisor
            .state()
            .current_response
            .as_ref()
            .map(|response| response.text.as_str()),
        Some("Partial reply")
    );

    supervisor.disconnect();
}

#[cfg(unix)]
#[tokio::test]
async fn connect_replays_saved_init_from_restored_session_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log_path = temp.path().join("agent-stdin.log");
    let script_path = create_test_headless_script(temp.path()).expect("script");
    fs::write(&log_path, "").expect("create log");

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.transport.env.push((
        "MAESTRO_TEST_LOG".to_string(),
        log_path.to_string_lossy().into_owned(),
    ));
    config.auto_reconnect = false;

    let init = InitConfig {
        system_prompt: Some("system prompt".to_string()),
        append_system_prompt: Some("appendix".to_string()),
        thinking_level: Some(super::super::messages::ThinkingLevel::High),
        approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
        history: None,
        code_mode: None,
        tool_grant: None,
    };
    let replay = SessionReplay {
        state: AgentState {
            session_id: Some("sess_replayed".to_string()),
            last_status: Some("Resuming session".to_string()),
            is_ready: true,
            ..AgentState::default()
        },
        last_init: Some(init.clone()),
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    };

    let mut supervisor = AgentSupervisor::new(config).with_session_replay(replay);
    assert_eq!(
        supervisor.state().session_id.as_deref(),
        Some("sess_replayed")
    );
    assert_eq!(
        supervisor.state().last_status.as_deref(),
        Some("Resuming session")
    );
    assert!(supervisor.state().is_ready);

    supervisor.connect().await.expect("connect");
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    let ready_event = tokio::time::timeout(Duration::from_secs(10), supervisor.recv())
        .await
        .expect("ready event timed out")
        .expect("ready event");
    assert!(matches!(ready_event, SupervisorEvent::Agent(_)));

    for _ in 0..80 {
        let log_contents = fs::read_to_string(&log_path).expect("read log");
        if log_contents.contains(r#""type":"init""#) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    supervisor.disconnect();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let logged_inits: Vec<_> = fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|line| {
            let message = serde_json::from_str::<ToAgentMessage>(line).expect("parse message");
            match message {
                ToAgentMessage::Init {
                    system_prompt,
                    append_system_prompt,
                    thinking_level,
                    approval_mode,
                    history: _,
                } => Some((
                    system_prompt,
                    append_system_prompt,
                    thinking_level,
                    approval_mode,
                )),
                _ => None,
            }
        })
        .collect();

    assert_eq!(
        logged_inits,
        vec![(
            init.system_prompt,
            init.append_system_prompt,
            init.thinking_level,
            init.approval_mode,
        )]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconnect_replays_last_init_and_matching_accepted_capability_set() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_test_headless_script(temp.path()).expect("script");
    let sessions_dir = temp.path().join("sessions");
    let recorder = SessionRecorder::new(&sessions_dir).expect("recorder");
    let session_id = recorder.id().to_string();

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.auto_reconnect = false;

    let init = InitConfig {
        system_prompt: Some("system prompt".to_string()),
        append_system_prompt: Some("appendix".to_string()),
        thinking_level: Some(super::super::messages::ThinkingLevel::High),
        approval_mode: Some(super::super::messages::ApprovalMode::Prompt),
        history: None,
        code_mode: None,
        tool_grant: None,
    };

    let mut supervisor = AgentSupervisor::new(config).with_session_recorder(recorder);
    supervisor.connect().await.expect("connect");
    supervisor.init(init.clone()).expect("initial init");
    let capability = ApplyWorkspaceCapabilitySet {
        organization_id: "org-1".to_string(),
        workspace_id: "workspace-1".to_string(),
        runner_session_id: "runner-1".to_string(),
        runtime_generation: 7,
        activation_generation: 3,
        workspace_snapshot_digest: "sha256:snapshot".to_string(),
        workspace_skill_set_digest: "sha256:skills".to_string(),
        capability_set_digest: "sha256:catalog".to_string(),
        workspace_instructions: vec!["Use the workspace review skill.".to_string()],
        admitted_catalog: Vec::new(),
        admission_receipt_id: "admission-3".to_string(),
    };
    supervisor
        .send(ToAgentMessage::ApplyWorkspaceCapabilitySet {
            request: capability.clone(),
        })
        .expect("send capability set");
    let receipt = WorkspaceCapabilitySetApplied {
        schema_version: "evalops.maestro.workspace-capability-set.v1".to_string(),
        organization_id: capability.organization_id.clone(),
        workspace_id: capability.workspace_id.clone(),
        runner_session_id: capability.runner_session_id.clone(),
        runtime_generation: capability.runtime_generation,
        activation_generation: capability.activation_generation,
        effective_catalog_digest: capability.capability_set_digest.clone(),
        accepted_entry_digests: Vec::new(),
        rejected_entries: Vec::new(),
        replay_cursor: workspace_capability_replay_cursor(&capability),
        applied_at: 123,
        controller_binding_sha256: "sha256:binding".to_string(),
        provider_prompt_sha256: "sha256:provider-prompt".to_string(),
        staged_for_next_turn: false,
        idempotent: false,
    };
    let _ =
        supervisor.apply_agent_message(FromAgentMessage::WorkspaceCapabilitySetApplied { receipt });

    supervisor.disconnect();
    tokio::time::sleep(Duration::from_millis(50)).await;

    supervisor.reconnect().await.expect("reconnect");
    supervisor.disconnect();
    supervisor.flush_session().expect("flush");

    let logged_inits: Vec<_> = SessionReader::load(&sessions_dir, &session_id)
        .expect("load session")
        .sent_messages()
        .into_iter()
        .filter_map(|message| match message {
            ToAgentMessage::Init {
                system_prompt,
                append_system_prompt,
                thinking_level,
                approval_mode,
                history: _,
            } => Some((
                system_prompt.clone(),
                append_system_prompt.clone(),
                *thinking_level,
                *approval_mode,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(logged_inits.len(), 2);
    for (system_prompt, append_system_prompt, thinking_level, approval_mode) in logged_inits {
        assert_eq!(system_prompt, init.system_prompt);
        assert_eq!(append_system_prompt, init.append_system_prompt);
        assert_eq!(thinking_level, init.thinking_level);
        assert_eq!(approval_mode, init.approval_mode);
    }

    let logged_capabilities: Vec<_> = SessionReader::load(&sessions_dir, &session_id)
        .expect("reload session")
        .sent_messages()
        .into_iter()
        .filter_map(|message| match message {
            ToAgentMessage::ApplyWorkspaceCapabilitySet { request } => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(logged_capabilities, vec![capability.clone(), capability]);
}

#[tokio::test]
async fn remote_connect_hydrates_state_without_replaying_init() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
        "last_init": {
            "system_prompt": "Persisted prompt",
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
    let status_event = serde_json::json!({
        "type": "message",
        "cursor": 1,
        "message": {
            "type": "status",
            "message": "Remote status"
        }
    })
    .to_string();
    let (addr, posted_bodies, request_headers, _request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![status_event]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .remote_api_key("secret")
        .remote_session_id("sess_remote")
        .build();

    supervisor.connect().await.expect("connect");

    assert_eq!(supervisor.state().model.as_deref(), Some("gpt-5.6"));
    assert_eq!(supervisor.state().provider.as_deref(), Some("openai"));
    assert_eq!(
        supervisor.state().session_id.as_deref(),
        Some("sess_remote")
    );
    assert_eq!(supervisor.state().last_status.as_deref(), Some("Attached"));

    assert!(matches!(
        supervisor.poll(),
        Some(SupervisorEvent::StateHydrated {
            session_id: Some(ref session_id)
        }) if session_id == "sess_remote"
    ));
    assert!(matches!(
        supervisor.poll(),
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.poll(),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Agent(agent_event))
            if matches!(
                *agent_event,
                AgentEvent::Status { ref message } if message == "Remote status"
            )
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;
    let posted_bodies = posted_bodies.lock().await.clone();
    assert_eq!(posted_bodies.len(), 1);
    let sent =
        serde_json::from_str::<ToAgentMessage>(&posted_bodies[0]).expect("parse sent message");
    assert!(matches!(sent, ToAgentMessage::Hello { .. }));
    let headers = request_headers.lock().await.clone();
    assert!(headers.first().is_some_and(|request| {
        request
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer secret")
    }));

    supervisor.disconnect();
}

#[tokio::test]
async fn recv_prioritizes_supervisor_events_before_remote_agent_messages() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
        "last_init": {
            "system_prompt": "Persisted prompt",
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
    let status_event = serde_json::json!({
        "type": "message",
        "cursor": 1,
        "message": {
            "type": "status",
            "message": "Remote status"
        }
    })
    .to_string();
    let (addr, _posted_bodies, _request_headers, _request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![status_event]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .remote_api_key("secret")
        .remote_session_id("sess_remote")
        .build();

    supervisor.connect().await.expect("connect");

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::StateHydrated {
            session_id: Some(ref session_id)
        }) if session_id == "sess_remote"
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Agent(agent_event))
            if matches!(
                *agent_event,
                AgentEvent::Status { ref message } if message == "Remote status"
            )
    ));

    supervisor.disconnect();
}

#[cfg(unix)]
#[tokio::test]
async fn recv_auto_reconnects_after_transport_disconnect() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_streaming_headless_script(temp.path()).expect("script");

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.reconnect_delay = Duration::from_millis(5);
    config.max_reconnect_attempts = 1;
    config.auto_reconnect = true;

    let mut supervisor = AgentSupervisor::new(config);
    supervisor.connect().await.expect("connect");

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    for _ in 0..4 {
        let _ = supervisor.recv().await.expect("initial agent event");
    }

    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed);
    assert!(matches!(
        disconnect,
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Reconnecting {
            attempt: 1,
            max_attempts: 1
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Agent(agent_event))
            if matches!(*agent_event, AgentEvent::Ready { .. })
    ));

    supervisor.disconnect();
}

#[tokio::test]
async fn failed_auto_reconnect_emits_disconnected_instead_of_hanging() {
    let mut config = SupervisorConfig::default();
    config.transport.cli_path = "/definitely/missing/maestro-headless".to_string();
    config.reconnect_delay = Duration::from_millis(1);
    config.max_reconnect_attempts = 1;
    config.auto_reconnect = true;

    let mut supervisor = AgentSupervisor::new(config);
    supervisor.pending_auto_reconnect = true;

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Reconnecting {
            attempt: 1,
            max_attempts: 1
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Unhealthy
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Disconnected { ref error })
            if error.contains("Failed to spawn agent")
    ));
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
    assert!(!supervisor.pending_auto_reconnect);
    assert!(!supervisor.is_connected());
}

#[tokio::test]
async fn non_retryable_remote_disconnect_does_not_schedule_auto_reconnect() {
    let mut supervisor = SupervisorBuilder::new().build();

    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::RemoteStatus {
        status: 409,
        retryable: false,
        kind: RemoteErrorKind::ControllerLeaseConflict,
        message: "remote request failed with status 409 Conflict".to_string(),
    });
    assert!(matches!(
        disconnect,
        SupervisorEvent::Disconnected { ref error }
            if error.contains("409 Conflict")
    ));
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
    assert!(!supervisor.pending_auto_reconnect);
    assert!(supervisor.poll().is_none());
}

#[tokio::test]
async fn non_retryable_remote_disconnect_shuts_down_transport() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _posted_bodies, _request_headers, _request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let transport = RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: format!("http://{addr}"),
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");
    let cancel_token = transport.cancel_token();

    let mut supervisor = SupervisorBuilder::new().build();
    supervisor.transport = Some(ManagedTransport::Remote(Box::new(transport)));

    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::RemoteStatus {
        status: 409,
        retryable: false,
        kind: RemoteErrorKind::ControllerLeaseConflict,
        message: "remote request failed with status 409 Conflict".to_string(),
    });

    assert!(matches!(
        disconnect,
        SupervisorEvent::Disconnected { ref error }
            if error.contains("409 Conflict")
    ));
    tokio::time::timeout(Duration::from_secs(1), cancel_token.cancelled())
        .await
        .expect("cancel token should be cancelled");
    assert!(supervisor.transport.is_none());
}

#[test]
fn stale_reference_disconnects_respect_retry_budget() {
    let mut supervisor = SupervisorBuilder::new().max_reconnect_attempts(0).build();

    for attempt in 1..=MAX_STALE_REMOTE_REFERENCE_RETRIES {
        let disconnect =
                supervisor.handle_transport_disconnect(AsyncTransportError::RemoteStatus {
                    status: 404,
                    retryable: true,
                    kind: RemoteErrorKind::StaleSubscriber,
                    message: "remote request failed with status 404 Not Found: {\"error\":\"Headless subscriber not found\"}".to_string(),
                });
        assert!(matches!(
            disconnect,
            SupervisorEvent::HealthChanged {
                status: HealthStatus::Reconnecting
            }
        ));
        assert_eq!(supervisor.stale_reference_retries, attempt);
        assert!(supervisor.pending_auto_reconnect);
        supervisor.pending_auto_reconnect = false;
    }

    let disconnect =
            supervisor.handle_transport_disconnect(AsyncTransportError::RemoteStatus {
                status: 404,
                retryable: true,
                kind: RemoteErrorKind::StaleSubscriber,
                message: "remote request failed with status 404 Not Found: {\"error\":\"Headless subscriber not found\"}".to_string(),
            });
    assert!(matches!(
        disconnect,
        SupervisorEvent::Disconnected { ref error }
            if error.contains("Headless subscriber not found")
    ));
    assert_eq!(
        supervisor.stale_reference_retries,
        MAX_STALE_REMOTE_REFERENCE_RETRIES + 1
    );
    assert!(!supervisor.pending_auto_reconnect);
}

#[test]
fn stale_session_disconnects_respect_retry_budget() {
    let mut supervisor = SupervisorBuilder::new().max_reconnect_attempts(0).build();

    for attempt in 1..=MAX_STALE_REMOTE_REFERENCE_RETRIES {
        let disconnect =
                supervisor.handle_transport_disconnect(AsyncTransportError::RemoteStatus {
                    status: 404,
                    retryable: true,
                    kind: RemoteErrorKind::StaleSession,
                    message: "remote request failed with status 404 Not Found: {\"error\":\"Headless session not found\"}".to_string(),
                });
        assert!(matches!(
            disconnect,
            SupervisorEvent::HealthChanged {
                status: HealthStatus::Reconnecting
            }
        ));
        assert_eq!(supervisor.stale_reference_retries, attempt);
        assert!(supervisor.pending_auto_reconnect);
        supervisor.pending_auto_reconnect = false;
    }

    let disconnect =
            supervisor.handle_transport_disconnect(AsyncTransportError::RemoteStatus {
                status: 404,
                retryable: true,
                kind: RemoteErrorKind::StaleSession,
                message: "remote request failed with status 404 Not Found: {\"error\":\"Headless session not found\"}".to_string(),
            });
    assert!(matches!(
        disconnect,
        SupervisorEvent::Disconnected { ref error }
            if error.contains("Headless session not found")
    ));
    assert_eq!(
        supervisor.stale_reference_retries,
        MAX_STALE_REMOTE_REFERENCE_RETRIES + 1
    );
    assert!(!supervisor.pending_auto_reconnect);
}

#[tokio::test]
async fn remote_reconnect_stale_session_errors_respect_retry_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let bootstrap_attempts = Arc::new(AtomicUsize::new(0));
    let subscribe_attempts = Arc::new(AtomicUsize::new(0));

    tokio::spawn({
        let bootstrap_attempts = Arc::clone(&bootstrap_attempts);
        let subscribe_attempts = Arc::clone(&subscribe_attempts);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let bootstrap_attempts = Arc::clone(&bootstrap_attempts);
                let subscribe_attempts = Arc::clone(&subscribe_attempts);
                tokio::spawn(async move {
                    let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    if path == "/api/headless/connections" {
                        bootstrap_attempts.fetch_add(1, Ordering::SeqCst);
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 200 OK",
                            "application/json",
                            r#"{"session_id":"sess_remote","connection_id":"conn_remote"}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
                        subscribe_attempts.fetch_add(1, Ordering::SeqCst);
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 404 Not Found",
                            "application/json",
                            r#"{"error":"Headless session not found"}"#,
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

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(0)
        .reconnect_delay(Duration::from_millis(1))
        .build();

    let error = supervisor
        .reconnect()
        .await
        .expect_err("reconnect should stop after stale-session retry budget is exhausted");
    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 404,
            retryable: true,
            kind: RemoteErrorKind::StaleSession,
            ..
        }
    ));
    assert_eq!(
        bootstrap_attempts.load(Ordering::SeqCst),
        (MAX_STALE_REMOTE_REFERENCE_RETRIES + 1) as usize
    );
    assert_eq!(
        subscribe_attempts.load(Ordering::SeqCst),
        (MAX_STALE_REMOTE_REFERENCE_RETRIES + 1) as usize
    );
    assert_eq!(
        supervisor.stale_reference_retries,
        MAX_STALE_REMOTE_REFERENCE_RETRIES + 1
    );
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
    assert!(!supervisor.pending_auto_reconnect);
}

#[tokio::test]
async fn successful_remote_connect_resets_stale_reference_retry_budget() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _posted_bodies, _request_headers, _request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .build();
    supervisor.stale_reference_retries = MAX_STALE_REMOTE_REFERENCE_RETRIES;

    supervisor.connect().await.expect("connect");
    assert_eq!(supervisor.stale_reference_retries, 0);
}

#[tokio::test]
async fn remote_auto_reconnect_reuses_bootstrapped_session_id() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
        "last_init": {
            "system_prompt": "Persisted prompt",
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
    let (addr, _posted_bodies, _request_headers, request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    assert_eq!(
        supervisor
            .config
            .remote
            .as_ref()
            .and_then(|config| config.session_id.as_deref()),
        Some("sess_remote")
    );

    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed);
    assert!(matches!(
        disconnect,
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::StateHydrated {
            session_id: Some(ref session_id)
        }) if session_id == "sess_remote"
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Reconnecting {
            attempt: 1,
            max_attempts: 1
        })
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::StateHydrated {
            session_id: Some(ref session_id)
        }) if session_id == "sess_remote"
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));

    let request_bodies = request_bodies.lock().await.clone();
    let connection_requests = request_bodies
        .into_iter()
        .filter(|(path, _body)| path == "/api/headless/connections")
        .map(|(_path, body)| {
            serde_json::from_str::<serde_json::Value>(&body).expect("valid connection body")
        })
        .collect::<Vec<_>>();
    assert_eq!(connection_requests.len(), 2);
    assert!(connection_requests[0].get("sessionId").is_none());
    assert_eq!(
        connection_requests[1]
            .get("sessionId")
            .and_then(serde_json::Value::as_str),
        Some("sess_remote")
    );

    supervisor.disconnect();
}

#[tokio::test]
async fn remote_reconnect_stops_after_non_retryable_bootstrap_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let bootstrap_attempts = Arc::new(AtomicUsize::new(0));

    tokio::spawn({
        let bootstrap_attempts = Arc::clone(&bootstrap_attempts);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let bootstrap_attempts = Arc::clone(&bootstrap_attempts);
                tokio::spawn(async move {
                    let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    if path == "/api/headless/connections" {
                        bootstrap_attempts.fetch_add(1, Ordering::SeqCst);
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 409 Conflict",
                            "application/json",
                            r#"{"error":"Controller lease is already held by another connection"}"#,
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

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(5)
        .reconnect_delay(Duration::from_millis(1))
        .build();

    let error = supervisor
        .reconnect()
        .await
        .expect_err("reconnect should fail");
    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 409,
            retryable: false,
            ..
        }
    ));
    assert_eq!(bootstrap_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
    assert!(!supervisor.pending_auto_reconnect);
}

#[tokio::test]
async fn remote_reconnect_stops_after_elapsed_time_budget_for_retryable_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let bootstrap_attempts = Arc::new(AtomicUsize::new(0));

    tokio::spawn({
        let bootstrap_attempts = Arc::clone(&bootstrap_attempts);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let bootstrap_attempts = Arc::clone(&bootstrap_attempts);
                tokio::spawn(async move {
                    let Some((path, _headers, _body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    if path == "/api/headless/connections" {
                        bootstrap_attempts.fetch_add(1, Ordering::SeqCst);
                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 500 Internal Server Error",
                            "application/json",
                            r#"{"error":"temporary upstream failure"}"#,
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

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(0)
        .reconnect_delay(Duration::from_millis(5))
        .max_reconnect_elapsed(Duration::from_millis(500))
        .build();

    let error = supervisor
        .reconnect()
        .await
        .expect_err("reconnect should stop after the elapsed time budget");
    assert!(matches!(
        error,
        AsyncTransportError::RemoteStatus {
            status: 500,
            retryable: true,
            ..
        }
    ));
    assert!(bootstrap_attempts.load(Ordering::SeqCst) >= 1);
    assert_eq!(supervisor.health(), HealthStatus::Unhealthy);
    assert!(!supervisor.pending_auto_reconnect);
}

#[tokio::test]
async fn remote_auto_reconnect_reuses_previous_connection_id_without_take_control() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _posted_bodies, _request_headers, request_bodies) =
        spawn_remote_headless_server_with_options(
            snapshot.to_string(),
            vec![],
            None,
            None,
            true,
            true,
        )
        .await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed);
    assert!(matches!(
        disconnect,
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));

    supervisor
        .reconnect()
        .await
        .expect("reconnect with the server-minted private capability");

    let request_bodies = request_bodies.lock().await.clone();
    let connection_requests = request_bodies
        .into_iter()
        .filter(|(path, _body)| path == "/api/headless/connections")
        .map(|(_path, body)| {
            serde_json::from_str::<serde_json::Value>(&body).expect("valid connection body")
        })
        .collect::<Vec<_>>();
    assert_eq!(connection_requests.len(), 2);
    assert_eq!(
        connection_requests[1]
            .get("connectionId")
            .and_then(serde_json::Value::as_str),
        Some("conn_remote")
    );
    assert_eq!(
        connection_requests[1]
            .get("connectionCapability")
            .and_then(serde_json::Value::as_str),
        Some("cap_remote")
    );
    assert!(
        connection_requests[1].get("takeControl").is_none(),
        "unexpected remote reconnects should reclaim their prior connection id before forcing controller takeover"
    );

    supervisor.disconnect();
}

#[tokio::test]
async fn remote_reconnect_retains_fallback_authority_after_subscribe_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connection_requests = Arc::new(Mutex::new(Vec::new()));
    let subscribe_attempts = Arc::new(AtomicUsize::new(0));
    let fallback_authority = Arc::new(Mutex::new(None::<(String, String)>));

    tokio::spawn({
        let connection_requests = Arc::clone(&connection_requests);
        let subscribe_attempts = Arc::clone(&subscribe_attempts);
        let fallback_authority = Arc::clone(&fallback_authority);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let connection_requests = Arc::clone(&connection_requests);
                let subscribe_attempts = Arc::clone(&subscribe_attempts);
                let fallback_authority = Arc::clone(&fallback_authority);
                tokio::spawn(async move {
                    let Some((path, _headers, body)) = read_http_request(&mut socket).await else {
                        return;
                    };
                    if path == "/api/headless/connections" {
                        let request = serde_json::from_str::<serde_json::Value>(&body)
                            .expect("valid connection request");
                        connection_requests.lock().await.push(request.clone());
                        let request_count = connection_requests.lock().await.len();
                        let connection_id = request
                            .get("connectionId")
                            .and_then(serde_json::Value::as_str);
                        let connection_capability = request
                            .get("connectionCapability")
                            .and_then(serde_json::Value::as_str);

                        if connection_id == Some("conn_stale")
                            && connection_capability == Some("cap_stale")
                            && request_count == 1
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 404 Not Found",
                                "application/json",
                                r#"{"error":"Headless connection not found"}"#,
                            )
                            .await;
                            return;
                        }
                        if request_count == 2
                            && connection_id.is_some_and(|value| value != "conn_stale")
                            && connection_capability.is_some_and(|value| value != "cap_stale")
                        {
                            let authority = (
                                connection_id.expect("fallback connection id").to_string(),
                                connection_capability
                                    .expect("fallback connection capability")
                                    .to_string(),
                            );
                            *fallback_authority.lock().await = Some(authority.clone());
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                &serde_json::json!({
                                    "session_id": "sess_remote",
                                    "connection_id": authority.0,
                                    "connection_capability": authority.1,
                                })
                                .to_string(),
                            )
                            .await;
                            return;
                        }
                        if request_count == 3
                            && fallback_authority.lock().await.as_ref().is_some_and(
                                |(fallback_connection_id, fallback_connection_capability)| {
                                    connection_id == Some(fallback_connection_id.as_str())
                                        && connection_capability
                                            == Some(fallback_connection_capability.as_str())
                                },
                            )
                        {
                            write_http_response(
                                &mut socket,
                                "HTTP/1.1 200 OK",
                                "application/json",
                                &serde_json::json!({
                                    "session_id": "sess_remote",
                                    "connection_id": connection_id,
                                    "connection_capability": connection_capability,
                                })
                                .to_string(),
                            )
                            .await;
                            return;
                        }

                        write_http_response(
                            &mut socket,
                            "HTTP/1.1 409 Conflict",
                            "application/json",
                            r#"{"error":"Controller lease is already held by another connection"}"#,
                        )
                        .await;
                        return;
                    }
                    if path.starts_with("/api/headless/sessions/") && path.ends_with("/subscribe") {
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
                        let authority = fallback_authority
                            .lock()
                            .await
                            .clone()
                            .expect("accepted fallback authority");
                        let response = serde_json::json!({
                            "connection_id": authority.0,
                            "connection_capability": authority.1,
                            "subscription_id": "sub_fresh",
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

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(2)
        .reconnect_delay(Duration::from_millis(1))
        .build();
    supervisor.remote_resume_authority = Some(RemoteConnectionResumeAuthority {
        connection_id: "conn_stale".to_string(),
        connection_capability: Some("cap_stale".to_string()),
    });

    supervisor
        .reconnect()
        .await
        .expect("retry should reclaim the freshly bootstrapped connection");

    let connection_requests = connection_requests.lock().await.clone();
    assert_eq!(connection_requests.len(), 3);
    let fallback_authority = fallback_authority
        .lock()
        .await
        .clone()
        .expect("accepted fallback authority");
    assert_eq!(
        connection_requests[2]
            .get("connectionId")
            .and_then(serde_json::Value::as_str),
        Some(fallback_authority.0.as_str())
    );
    assert_eq!(
        connection_requests[2]
            .get("connectionCapability")
            .and_then(serde_json::Value::as_str),
        Some(fallback_authority.1.as_str())
    );
    assert_ne!(fallback_authority.0, "conn_stale");
    assert_ne!(fallback_authority.1, "cap_stale");
    assert!(
        connection_requests
            .iter()
            .all(|request| request.get("takeControl").is_none()),
        "recovery must not take over a controller lease"
    );

    supervisor.disconnect();
}

#[tokio::test]
async fn clean_remote_disconnect_does_not_force_take_control_on_manual_reconnect() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _posted_bodies, _request_headers, request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    supervisor.disconnect();
    tokio::time::sleep(Duration::from_millis(50)).await;
    supervisor.reconnect().await.expect("manual reconnect");

    let request_bodies = request_bodies.lock().await.clone();
    let connection_requests = request_bodies
        .into_iter()
        .filter(|(path, _body)| path == "/api/headless/connections")
        .map(|(_path, body)| {
            serde_json::from_str::<serde_json::Value>(&body).expect("valid connection body")
        })
        .collect::<Vec<_>>();
    assert_eq!(connection_requests.len(), 2);
    let first_connection_id = connection_requests[0]["connectionId"]
        .as_str()
        .expect("initial generated connection id");
    let fresh_connection_id = connection_requests[1]["connectionId"]
        .as_str()
        .expect("fresh generated connection id");
    let fresh_connection_capability = connection_requests[1]["connectionCapability"]
        .as_str()
        .expect("fresh generated connection capability");
    assert_ne!(
        fresh_connection_id, first_connection_id,
        "clean disconnects must not reuse the remembered connection id"
    );
    assert!(fresh_connection_id.starts_with("conn_"));
    assert!(fresh_connection_capability.starts_with("cap_"));
    assert!(
        connection_requests[1].get("takeControl").is_none(),
        "clean disconnects should not force controller takeover on the next manual reconnect"
    );

    supervisor.disconnect();
}

#[tokio::test]
async fn explicit_disconnect_after_failed_reconnect_clears_private_resume_authority() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _, _, request_bodies) = spawn_remote_headless_server_with_options(
        snapshot.to_string(),
        vec![],
        None,
        None,
        true,
        true,
    )
    .await;
    let base_url = format!("http://{addr}");

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(base_url.clone())
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed);
    assert!(matches!(
        disconnect,
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));

    supervisor
        .config
        .remote
        .as_mut()
        .expect("remote config")
        .base_url = "http://127.0.0.1:1".to_string();
    supervisor
        .reconnect()
        .await
        .expect_err("reconnect should fail while the endpoint is unavailable");
    supervisor
        .config
        .remote
        .as_mut()
        .expect("remote config")
        .base_url = base_url;

    supervisor.disconnect();
    supervisor.connect().await.expect("fresh connect");

    let connection_requests = request_bodies
        .lock()
        .await
        .iter()
        .filter(|(path, _body)| path == "/api/headless/connections")
        .map(|(_path, body)| {
            serde_json::from_str::<serde_json::Value>(body).expect("valid connection body")
        })
        .collect::<Vec<_>>();
    assert_eq!(connection_requests.len(), 2);
    let fresh_connection_id = connection_requests[1]["connectionId"]
        .as_str()
        .expect("fresh generated connection id");
    let fresh_connection_capability = connection_requests[1]["connectionCapability"]
        .as_str()
        .expect("fresh generated connection capability");
    assert_ne!(fresh_connection_id, "conn_remote");
    assert_ne!(fresh_connection_capability, "cap_remote");
    assert!(fresh_connection_id.starts_with("conn_"));
    assert!(fresh_connection_capability.starts_with("cap_"));
    assert!(connection_requests[1].get("takeControl").is_none());

    supervisor.disconnect();
}

#[tokio::test]
async fn manual_remote_reconnect_shuts_down_existing_transport() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _posted_bodies, _request_headers, _request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    let cancel_token = supervisor
        .transport
        .as_ref()
        .and_then(|transport| match transport {
            ManagedTransport::Remote(transport) => Some(transport.cancel_token()),
            ManagedTransport::Local(_) => None,
        })
        .expect("remote transport");

    supervisor.reconnect().await.expect("manual reconnect");

    tokio::time::timeout(Duration::from_secs(1), cancel_token.cancelled())
        .await
        .expect("manual reconnect should shut down the previous transport");
    assert!(supervisor.is_connected());

    supervisor.disconnect();
}

#[tokio::test]
async fn manual_remote_reconnect_disconnects_before_next_bootstrap() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let (addr, _posted_bodies, _request_headers, request_bodies) =
        spawn_remote_headless_server(snapshot.to_string(), vec![]).await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    supervisor.reconnect().await.expect("manual reconnect");

    let request_paths = request_bodies
        .lock()
        .await
        .iter()
        .map(|(path, _body)| path.clone())
        .collect::<Vec<_>>();
    let disconnect_index = request_paths
        .iter()
        .position(|path| path.ends_with("/disconnect"))
        .expect("disconnect request");
    let second_bootstrap_index = request_paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| (path == "/api/headless/connections").then_some(index))
        .nth(1)
        .expect("second bootstrap request");
    assert!(
        disconnect_index < second_bootstrap_index,
        "manual reconnect should disconnect the stale transport before bootstrapping a replacement"
    );

    supervisor.disconnect();
}

#[tokio::test]
async fn auto_remote_reconnect_waits_for_disconnect_completion_before_next_bootstrap() {
    let snapshot = serde_json::json!({
        "protocolVersion": "2026-03-30",
        "session_id": "sess_remote",
        "cursor": 0,
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
    let lifecycle_markers = Arc::new(Mutex::new(Vec::new()));
    let (addr, _posted_bodies, _request_headers, _request_bodies) =
        spawn_remote_headless_server_with_options(
            snapshot.to_string(),
            vec![],
            Some(Duration::from_millis(50)),
            Some(Arc::clone(&lifecycle_markers)),
            false,
            false,
        )
        .await;

    let mut supervisor = SupervisorBuilder::new()
        .remote_base_url(format!("http://{addr}"))
        .max_reconnect_attempts(1)
        .reconnect_delay(Duration::from_millis(5))
        .build();

    supervisor.connect().await.expect("connect");
    let disconnect = supervisor.handle_transport_disconnect(AsyncTransportError::ChannelClosed);
    assert!(matches!(
        disconnect,
        SupervisorEvent::HealthChanged {
            status: HealthStatus::Reconnecting
        }
    ));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let markers = lifecycle_markers.lock().await.clone();
            let bootstrap_count = markers
                .iter()
                .filter(|marker| *marker == "bootstrap")
                .count();
            let saw_disconnect_end = markers.iter().any(|marker| marker == "disconnect:end");
            if bootstrap_count >= 2 && saw_disconnect_end {
                break;
            }
            let _ = supervisor.recv().await.expect("supervisor event");
        }
    })
    .await
    .expect("auto reconnect should complete");

    let lifecycle_markers = lifecycle_markers.lock().await.clone();
    let disconnect_end_index = lifecycle_markers
        .iter()
        .position(|marker| marker == "disconnect:end")
        .expect("disconnect completion marker");
    let second_bootstrap_index = lifecycle_markers
        .iter()
        .enumerate()
        .filter_map(|(index, marker)| (marker == "bootstrap").then_some(index))
        .nth(1)
        .expect("second bootstrap marker");
    assert!(
        disconnect_end_index < second_bootstrap_index,
        "auto reconnect should wait for remote disconnect completion before bootstrapping a replacement"
    );

    supervisor.disconnect();
}

#[cfg(unix)]
#[tokio::test]
async fn unhealthy_silence_schedules_auto_reconnect_for_next_recv() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_streaming_headless_script(temp.path()).expect("script");

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.health_check_interval = Duration::from_millis(10);
    config.health_check_timeout = Duration::from_millis(10);
    config.reconnect_delay = Duration::from_millis(5);
    config.max_reconnect_attempts = 1;
    config.auto_reconnect = true;

    let mut supervisor = AgentSupervisor::new(config);
    supervisor.connect().await.expect("connect");

    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::Connected)
    ));
    assert!(matches!(
        supervisor.recv().await,
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Healthy
        })
    ));
    for _ in 0..4 {
        let _ = supervisor.recv().await.expect("initial agent event");
    }

    supervisor.health_status = HealthStatus::Degraded;
    supervisor.last_response = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(25))
            .expect("monotonic clock supports subtraction"),
    );
    supervisor.state.current_response = Some(StreamingResponse::new("resp_silence".into()));
    supervisor.state.is_responding = true;
    let approval = PendingApproval {
        call_id: "call_silence".to_string(),
        tool_execution_id: None,
        request_id: Some("req_silence".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "git push" }),
        started_at_ms: Some(1_771_000_000_000),
    };
    let client_tool = PendingApproval {
        call_id: "call_client_silence".to_string(),
        tool_execution_id: None,
        request_id: Some("req_client_silence".to_string()),
        tool: "client_tool".to_string(),
        args: serde_json::json!({ "name": "artifacts" }),
        started_at_ms: None,
    };
    let user_input = PendingApproval {
        call_id: "call_user_silence".to_string(),
        tool_execution_id: None,
        request_id: Some("req_user_silence".to_string()),
        tool: "ask_user".to_string(),
        args: serde_json::json!({ "question": "continue?" }),
        started_at_ms: None,
    };
    let tool_retry = PendingApproval {
        call_id: "call_retry_silence".to_string(),
        tool_execution_id: None,
        request_id: Some("req_retry_silence".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({ "command": "retry" }),
        started_at_ms: None,
    };
    supervisor.state.pending_approvals.push(approval.clone());
    supervisor
        .state
        .pending_client_tools
        .push(client_tool.clone());
    supervisor
        .state
        .pending_user_inputs
        .push(user_input.clone());
    supervisor
        .state
        .pending_tool_retries
        .push(tool_retry.clone());
    supervisor
        .state
        .tracked_tools
        .insert(approval.call_id.clone(), approval);
    supervisor
        .state
        .tracked_tools
        .insert(client_tool.call_id.clone(), client_tool);
    supervisor
        .state
        .tracked_tools
        .insert(user_input.call_id.clone(), user_input);
    supervisor
        .state
        .tracked_tools
        .insert(tool_retry.call_id.clone(), tool_retry);
    supervisor.state.active_tools.insert(
        "call_silence".to_string(),
        ActiveTool {
            call_id: "call_silence".to_string(),
            tool: "grep".to_string(),
            output: "partial".to_string(),
            started: Instant::now(),
        },
    );
    supervisor.state.active_utility_commands.insert(
        "cmd_silence".to_string(),
        ActiveUtilityCommand {
            command_id: "cmd_silence".to_string(),
            command: "sleep 10".to_string(),
            cwd: Some("/tmp/project".to_string()),
            shell_mode: UtilityCommandShellMode::Shell,
            terminal_mode: UtilityCommandTerminalMode::Pipe,
            pid: Some(99),
            columns: None,
            rows: None,
            owner_connection_id: Some("conn_remote".to_string()),
            output: "partial".to_string(),
        },
    );
    supervisor.state.active_file_watches.insert(
        "watch_silence".to_string(),
        ActiveFileWatch {
            watch_id: "watch_silence".to_string(),
            root_dir: "/tmp/project".to_string(),
            include_patterns: None,
            exclude_patterns: Some(vec!["dist/**".to_string()]),
            debounce_ms: 500,
            owner_connection_id: Some("conn_remote".to_string()),
        },
    );

    assert!(matches!(
        supervisor.due_health_transition(Instant::now()),
        Some(SupervisorEvent::HealthChanged {
            status: HealthStatus::Unhealthy
        })
    ));
    assert!(!supervisor.is_connected());
    assert!(supervisor.pending_auto_reconnect);
    assert!(supervisor.state().current_response.is_some());
    assert_eq!(supervisor.state().pending_approvals.len(), 1);
    assert_eq!(supervisor.state().pending_client_tools.len(), 1);
    assert_eq!(supervisor.state().pending_user_inputs.len(), 1);
    assert_eq!(supervisor.state().pending_tool_retries.len(), 1);
    assert_eq!(supervisor.state().active_tools.len(), 1);
    assert_eq!(supervisor.state().active_utility_commands.len(), 1);
    assert_eq!(supervisor.state().active_file_watches.len(), 1);
    assert_eq!(supervisor.state().tracked_tools.len(), 4);
    assert!(supervisor.state().is_responding);
}

#[test]
fn supervisor_drain_preserves_governed_tool_request_authority() {
    let request = serde_json::json!({
        "type": "governed_client_tool_request",
        "call_id": "call-1",
        "tool_execution_id": "execution-1",
        "tool": "computer.shell",
        "args": {"command": "ls -la", "idempotency_key": "inspect-sources"},
        "provider_tool_name": "computer_shell",
        "tool_id": "platform.capability.computer.shell.v1",
        "connection_binding_id": "binding-1",
        "client_instance_id": "platform-worker.operating-runtime",
        "grant_id": "grant-1",
        "grant_version": 1,
        "grant_hash": "grant-hash",
        "turn_digest": "turn-digest",
        "definition_digest": "definition-digest",
        "args_digest": "args-digest",
        "owner_lease_epoch": 7,
        "idempotency_key": "inspect-sources"
    });
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let event = supervisor
        .apply_agent_message(serde_json::from_value(request.clone()).expect("governed request"))
        .expect("pending control request must leave the supervisor");
    assert_eq!(supervisor.state.pending_client_tools.len(), 1);
    assert_eq!(
        supervisor.state.governed_client_tool_bindings["call-1"].owner_lease_epoch,
        7
    );
    supervisor
        .event_tx
        .send(event)
        .expect("enqueue control request");
    let messages = supervisor.drain_available_agent_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        serde_json::to_value(&messages[0]).expect("wire request"),
        request
    );
    assert!(supervisor.drain_available_agent_messages().is_empty());
}

#[test]
fn supervisor_drain_preserves_client_and_server_requests() {
    for request in [
        serde_json::json!({
            "type": "client_tool_request", "call_id": "call-1",
            "tool_execution_id": "execution-1", "tool": "external.read", "args": {}
        }),
        serde_json::json!({
            "type": "server_request", "request_id": "request-1",
            "request_type": "approval", "call_id": "call-1",
            "tool_execution_id": "execution-1", "tool": "external.write",
            "args": {"value": 1}, "reason": "requires approval"
        }),
        serde_json::json!({
            "type": "server_request_resolved", "request_id": "request-1",
            "request_type": "approval", "call_id": "call-1",
            "resolution": "denied", "resolved_by": "policy"
        }),
    ] {
        let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
        let event = supervisor
            .apply_agent_message(serde_json::from_value(request.clone()).expect("control request"))
            .expect("control request must leave the supervisor");
        supervisor
            .event_tx
            .send(event)
            .expect("enqueue control request");
        let messages = supervisor.drain_available_agent_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            serde_json::to_value(&messages[0]).expect("wire request"),
            request
        );
    }
}

#[test]
fn supervisor_managed_authorization_request_waits_for_native_acknowledgement() {
    let mut supervisor = AgentSupervisor::new(SupervisorConfig::default());
    let event = supervisor
        .apply_agent_message(FromAgentMessage::ManagedAuthorizationRequest {
            request_id: "invocation-1".into(),
        })
        .expect("authorization request must reach controller");
    supervisor.event_tx.send(event).unwrap();
    assert!(
        matches!(supervisor.drain_available_agent_messages().as_slice(),
        [FromAgentMessage::ManagedAuthorizationRequest { request_id }] if request_id == "invocation-1")
    );
    let response = ToAgentMessage::ManagedAuthorizationResult {
        request_id: "invocation-1".into(),
        authorization: maestro_runtime_contracts::ManagedInferenceAuthorization::new("opaque"),
    };
    assert_eq!(response_ack_request_id(&response), Some("invocation-1"));
    supervisor.state.handle_sent_message(&response);
    assert_eq!(
        supervisor.state.pending_managed_authorizations,
        ["invocation-1"]
    );
    supervisor.apply_agent_message(FromAgentMessage::ResponseAccepted {
        request_id: "other".into(),
    });
    assert_eq!(
        supervisor.state.pending_managed_authorizations,
        ["invocation-1"]
    );
    supervisor.apply_agent_message(FromAgentMessage::ResponseAccepted {
        request_id: "invocation-1".into(),
    });
    assert!(supervisor.state.pending_managed_authorizations.is_empty());
}
