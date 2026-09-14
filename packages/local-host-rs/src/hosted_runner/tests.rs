use reqwest::StatusCode;
use std::fs;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tempfile::tempdir;
use tokio::sync::Notify;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::config::HostedRunnerWorkloadIdentityConfig;
use super::*;
use crate::headless::RemoteTransportConfig;
use crate::headless::messages::{CodexSubagentContinuityEdge, ToolRetryDecisionAction};
use crate::headless::{NativeToolCapability, PendingApproval};
use crate::hosted_runner::rendezvous_protocol::RendezvousMode;

mod transport_flush;

#[test]
fn append_turn_dispatch_preserves_managed_inference_authorization() {
    let message = append_turn_message(AppendTurnRequest {
        protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
        turn_id: "turn-managed".to_string(),
        kind: ThreadTurnKind::UserMessage,
        content: "managed prompt".to_string(),
        attachments: None,
        code_mode: None,
        tool_grant: None,
        managed_inference_authorization: Some(crate::agent::ManagedInferenceAuthorization::new(
            "signed-capability-marker",
        )),
    });

    assert!(matches!(
        message,
        ToAgentMessage::Prompt {
            managed_inference_authorization: Some(authorization),
            ..
        } if serde_json::to_value(&authorization).ok()
            == Some(serde_json::json!("signed-capability-marker"))
    ));
}

#[tokio::test]
async fn initial_identity_exchanges_are_polled_concurrently() {
    let server_started = Arc::new(tokio::sync::Notify::new());
    let client_started = Arc::new(tokio::sync::Notify::new());

    let server = {
        let server_started = server_started.clone();
        let client_started = client_started.clone();
        async move {
            server_started.notify_one();
            client_started.notified().await;
            Ok::<_, &'static str>("server")
        }
    };
    let client = {
        let server_started = server_started.clone();
        let client_started = client_started.clone();
        async move {
            client_started.notify_one();
            server_started.notified().await;
            Ok::<_, &'static str>("client")
        }
    };

    let identities = tokio::time::timeout(
        Duration::from_millis(100),
        join_initial_identity_exchanges(server, client),
    )
    .await
    .expect("both exchange futures must be polled")
    .expect("both exchanges succeed");

    assert_eq!(identities, ("server", "client"));
}

#[tokio::test]
async fn hosted_runner_preparation_is_reversible_before_activation() {
    let workspace = tempdir().expect("workspace");
    let prepared = prepare_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("prepare hosted runner");
    let local_addr = prepared.local_addr;

    assert!(prepared.identity_runtime.is_none());
    drop(prepared);

    // The full workspace suite opens thousands of ephemeral listeners in
    // parallel. Another test can claim this port between the drop above and
    // our first rebind attempt, so allow that transient race to clear while
    // still requiring this exact address to become available promptly.
    let rebound = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match TcpListener::bind(local_addr).await {
                Ok(listener) => break Ok(listener),
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .expect("dropping preparation releases listener promptly")
    .expect("dropping preparation releases listener");
    drop(rebound);
}

#[tokio::test]
async fn oversized_receipt_identity_fails_before_listener_bind() {
    let workspace = tempdir().expect("workspace");
    let mut config = test_config(workspace.path().to_path_buf());
    config.runner_session_id = "r".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1);

    let error = match prepare_hosted_runner(config).await {
        Ok(_) => panic!("oversized receipt identity must fail before bind"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("runner_session_id"));
    assert!(error.to_string().contains("runtime receipt limit"));
}

#[tokio::test]
async fn whitespace_only_receipt_identities_fail_before_listener_bind() {
    for field in ["maestro_session_id", "workspace_id", "agent_run_id"] {
        let workspace = tempdir().expect("workspace");
        let config = match field {
            "maestro_session_id" => {
                test_config(workspace.path().to_path_buf()).with_maestro_session_id(" \t ")
            }
            "workspace_id" => test_config(workspace.path().to_path_buf()).with_workspace_id(" \t "),
            "agent_run_id" => test_config(workspace.path().to_path_buf()).with_agent_run_id(" \t "),
            _ => unreachable!("all test fields are covered above"),
        };
        let error = match prepare_hosted_runner(config).await {
            Ok(_) => panic!("whitespace-only {field} must fail before bind"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(field));
        assert!(error.to_string().contains("must not be empty"));
    }
}

#[tokio::test]
async fn padded_receipt_identities_fail_before_listener_bind() {
    for field in ["maestro_session_id", "workspace_id", "agent_run_id"] {
        let workspace = tempdir().expect("workspace");
        let config = match field {
            "maestro_session_id" => {
                test_config(workspace.path().to_path_buf()).with_maestro_session_id(" session-1 ")
            }
            "workspace_id" => {
                test_config(workspace.path().to_path_buf()).with_workspace_id(" workspace-1 ")
            }
            "agent_run_id" => {
                test_config(workspace.path().to_path_buf()).with_agent_run_id(" run-1 ")
            }
            _ => unreachable!("all test fields are covered above"),
        };
        let error = match prepare_hosted_runner(config).await {
            Ok(_) => panic!("padded {field} must fail before bind"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(field));
        assert!(error.to_string().contains("surrounding whitespace"));
    }
}

#[test]
fn hosted_trace_fields_accept_only_valid_w3c_parent_and_safe_route_classes() {
    let parent = "00-0af7656daaaaaaaaaaaaaaaaaaaaaaaa-b7ad6b7169203331-01";
    let headers = TraceHeaders::from_values(Some(parent), None);
    assert_eq!(headers.traceparent(), Some(parent));
    assert!(!TraceHeaders::from_values(Some("prompt=never-log-this"), None).is_valid());
    assert_eq!(
        route_class("/api/headless/threads/t-1/turns"),
        "/api/headless/threads/:id/turns"
    );
    assert_eq!(
        route_class("/api/headless/sessions/s-1/events"),
        "/api/headless/sessions/:id/events"
    );
}

#[test]
fn status_reason_covers_runner_error_statuses() {
    let codes = [
        HostedRunnerErrorCode::InvalidConfig,
        HostedRunnerErrorCode::InvalidSnapshotManifest,
        HostedRunnerErrorCode::BadRequest,
        HostedRunnerErrorCode::NotFound,
        HostedRunnerErrorCode::StaleSession,
        HostedRunnerErrorCode::StaleConnection,
        HostedRunnerErrorCode::AccessDenied,
        HostedRunnerErrorCode::RuntimeNotReady,
        HostedRunnerErrorCode::LeaseConflict,
        HostedRunnerErrorCode::RuntimeOwnedElsewhere,
        HostedRunnerErrorCode::WorkspaceViolation,
        HostedRunnerErrorCode::UnsupportedCapability,
        HostedRunnerErrorCode::RuntimeFailed,
        HostedRunnerErrorCode::Internal,
    ];

    for code in codes {
        assert_ne!(status_reason(code.http_status()), "OK", "{code:?}");
    }
}

#[test]
fn fatal_runtime_rejects_mutations_as_terminal_runtime_failed() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared.state.lock().expect("state");
        state.ready = false;
        state.runtime_failed = true;
        state.last_status = Some("Runtime failed".to_string());
        state.last_error_type = Some("fatal".to_string());
    }

    let error = shared
        .ensure_mutation_allowed()
        .expect_err("fatal runtime must reject new mutations");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    assert_eq!(error.status, 500);
}

#[test]
fn fatal_agent_event_transitions_the_hosted_runtime_to_terminal_state() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let mut state = shared.state.lock().expect("state");
    shared.publish_message(
        &mut state,
        crate::headless::messages::FromAgentMessage::Error {
            request_id: None,
            message: "managed model credential rejected".to_string(),
            fatal: true,
            terminal: true,
            error_type: Some(crate::headless::messages::HeadlessErrorType::Fatal),
        },
    );

    assert!(state.runtime_failed);
    assert!(!state.ready);
    assert_eq!(state.last_status.as_deref(), Some("Runtime failed"));
    drop(state);
    let error = shared
        .ensure_mutation_allowed()
        .expect_err("fatal agent event must close the mutation boundary");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
}

#[test]
fn runtime_receipt_tracks_generation_bound_durable_lifecycle_boundaries() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let ready = shared.runtime_receipt().expect("initial ready receipt");
    assert_eq!(ready.kind, RuntimeReceiptKind::Ready);
    assert_eq!(ready.lifecycle_state, RuntimeLifecycleState::ExecutionReady);
    assert_eq!(ready.runtime_generation, 0);
    assert!(ready.flush_watermark > 0);
    assert_eq!(ready.replay_cursor, 0);
    assert!(ready.capability_digest.starts_with("sha256:"));

    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "receipt-terminal-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "receipt terminal".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "receipt-terminal".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
    }

    let terminal = shared.runtime_receipt().expect("terminal receipt");
    assert_eq!(terminal.kind, RuntimeReceiptKind::Terminal);
    assert_eq!(
        terminal.terminal,
        Some(RuntimeTerminalClassification::Completed)
    );
    assert!(terminal.replay_cursor > ready.replay_cursor);
    assert!(terminal.flush_watermark > ready.flush_watermark);
    let encoded = serde_json::to_string(&terminal).expect("receipt json");
    assert!(!encoded.contains("sentinel"));
    assert!(!encoded.contains("token"));

    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "receipt-cancelled-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "receipt cancelled".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::Error {
                request_id: None,
                message: "cancelled by controller".to_string(),
                fatal: false,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Cancelled),
            },
        );
    }
    let cancelled = shared.runtime_receipt().expect("cancelled receipt");
    assert_eq!(cancelled.kind, RuntimeReceiptKind::Terminal);
    assert_eq!(cancelled.lifecycle_state, RuntimeLifecycleState::Active);
    assert_eq!(
        cancelled.terminal,
        Some(RuntimeTerminalClassification::Cancelled)
    );
    assert!(cancelled.flush_watermark > terminal.flush_watermark);

    {
        let mut state = shared.state.lock().expect("state");
        shared.publish_message(
            &mut state,
            FromAgentMessage::Error {
                request_id: None,
                message: "redacted runtime failure detail".to_string(),
                fatal: true,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Fatal),
            },
        );
    }
    let failed = shared.runtime_receipt().expect("failed receipt");
    assert_eq!(failed.kind, RuntimeReceiptKind::Failed);
    assert_eq!(failed.lifecycle_state, RuntimeLifecycleState::Failed);
    assert_eq!(failed.error_type.as_deref(), Some("fatal"));
    assert!(failed.flush_watermark > cancelled.flush_watermark);
}

#[test]
fn late_terminal_event_does_not_override_first_terminal_receipt() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "first-terminal-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "first terminal wins".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::ProviderError {
                kind: maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
                message: "provider stopped the turn".to_string(),
            },
        );
        let first = state.runtime_receipt.as_ref().expect("first receipt");
        assert_eq!(
            first.terminal,
            Some(RuntimeTerminalClassification::ProviderFailed)
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "late-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
        let after_late = state
            .runtime_receipt
            .as_ref()
            .expect("receipt after late event");
        assert_eq!(
            after_late.terminal,
            Some(RuntimeTerminalClassification::ProviderFailed)
        );
    }
}

#[test]
fn late_ready_event_does_not_replace_a_durable_terminal_receipt() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "late-ready-terminal-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "terminal before delayed ready".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "late-ready-terminal-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
        let terminal = state
            .runtime_receipt
            .clone()
            .expect("terminal receipt before delayed ready");

        shared.publish_message(
            &mut state,
            FromAgentMessage::Ready {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                model: "delayed-ready-model".to_string(),
                provider: "delayed-ready-provider".to_string(),
                session_id: None,
            },
        );

        let after_ready = state
            .runtime_receipt
            .as_ref()
            .expect("terminal receipt after delayed ready");
        assert_eq!(after_ready.kind, RuntimeReceiptKind::Terminal);
        assert_eq!(after_ready.terminal, terminal.terminal);
        assert_eq!(after_ready.replay_cursor, terminal.replay_cursor);
    }
}

#[test]
fn invalid_live_ready_binding_fails_closed_and_clears_stale_receipt() {
    for invalid_field in ["model", "provider"] {
        let workspace = tempdir().expect("workspace");
        let invalid_model = "m".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1);
        let invalid_provider = "p".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1);
        let config = test_config(workspace.path().to_path_buf());
        let executor = Arc::new(StatefulRuntimeExecutor::new(AgentState::default()));
        let shared = SharedRunner::new_with_message_executor_and_restore(
            config.clone(),
            executor.clone(),
            None,
        );
        assert_eq!(
            shared
                .runtime_receipt()
                .as_ref()
                .map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Ready)
        );

        let model = if invalid_field == "model" {
            invalid_model.clone()
        } else {
            "valid-model".to_string()
        };
        let provider = if invalid_field == "provider" {
            invalid_provider.clone()
        } else {
            "valid-provider".to_string()
        };
        *executor.state.lock().expect("state") = AgentState {
            model: Some(invalid_model.clone()),
            provider: Some(invalid_provider.clone()),
            ..AgentState::default()
        };
        {
            let mut state = shared.state.lock().expect("state");
            shared.publish_message(
                &mut state,
                FromAgentMessage::Ready {
                    protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                    model,
                    provider,
                    session_id: None,
                },
            );
            assert!(
                !state.ready,
                "invalid {invalid_field} must not remain ready"
            );
            assert!(state.runtime_failed);
            assert!(state.runtime_receipt.is_none());
            assert_eq!(
                state.last_error_type.as_deref(),
                Some("invalid_runtime_receipt_identity")
            );
            let snapshot = shared.public_snapshot(&state);
            assert!(snapshot.state.model.is_none());
            assert!(snapshot.state.provider.is_none());
        }
        assert!(!shared.identity().ready);

        drop(shared);
        let loaded = ThreadJournal::load(workspace.path(), "sess_test", config.runtime_generation)
            .expect("load invalid binding journal");
        assert!(loaded.persisted_runtime_model.is_none());
        assert!(loaded.persisted_runtime_provider.is_none());
        let journal_events =
            serde_json::to_string(&loaded.events).expect("serialize journal events");
        assert!(!journal_events.contains(&invalid_model));
        assert!(!journal_events.contains(&invalid_provider));
        drop(loaded);

        let restarted = SharedRunner::new_with_message_executor_and_restore(
            config,
            Arc::new(StatefulRuntimeExecutor::new(AgentState::default())),
            None,
        );
        let state = restarted.state.lock().expect("restarted state");
        assert!(!state.ready);
        assert!(state.runtime_failed);
        assert_eq!(
            state.last_error_type.as_deref(),
            Some("invalid_runtime_receipt_identity")
        );
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Failed)
        );
        let receipt = state.runtime_receipt.as_ref().expect("failed receipt");
        assert!(receipt.model.is_none());
        assert!(receipt.provider.is_none());
        assert!(state.runtime_model_binding.is_none());
        assert!(state.runtime_provider_binding.is_none());
    }
}

#[tokio::test]
async fn oversized_supervisor_binding_fails_before_listener_bind_and_ready_receipt() {
    for (field, model, provider) in [
        (
            "model",
            "m".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1),
            "valid-provider".to_string(),
        ),
        (
            "provider",
            "valid-model".to_string(),
            "p".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1),
        ),
    ] {
        let workspace = tempdir().expect("workspace");
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve listener address");
        let bind_addr = reserved.local_addr().expect("reserved listener address");
        let mut config = test_config(workspace.path().to_path_buf());
        config.bind_addr = bind_addr;
        let executor = Arc::new(StatefulRuntimeExecutor::new(AgentState {
            model: Some(model),
            provider: Some(provider),
            ..AgentState::default()
        }));

        let error = match start_hosted_runner_with_message_executor(config, executor).await {
            Ok(_) => panic!("invalid supervisor binding must fail before listener bind"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(field));
        assert!(error.to_string().contains("runtime receipt limit"));

        assert_eq!(
            reserved.local_addr().expect("reserved listener address"),
            bind_addr,
            "validation must fail while the listener address remains reserved",
        );
    }
}

#[test]
fn runtime_failure_receipt_preserves_origin_category() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    shared.publish_runtime_error(
        "event_pump_failed",
        HostedRunnerError::internal("native headless agent disconnected"),
    );

    let receipt = shared.runtime_receipt().expect("runtime failure receipt");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(receipt.error_type.as_deref(), Some("event_pump_failed"));
}

#[test]
fn ready_event_cannot_replace_a_durable_runtime_failure() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    shared.publish_runtime_error(
        "event_pump_failed",
        HostedRunnerError::internal("native headless agent disconnected"),
    );
    {
        let mut state = shared.state.lock().expect("state");
        shared.publish_message(
            &mut state,
            FromAgentMessage::Ready {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                model: "reconnected-model".to_string(),
                provider: "reconnected-provider".to_string(),
                session_id: None,
            },
        );
    }
    let receipt = shared
        .runtime_receipt()
        .expect("failed receipt remains published");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(receipt.error_type.as_deref(), Some("event_pump_failed"));
    let state = shared.state.lock().expect("state");
    assert!(state.runtime_failed);
    assert!(!state.ready);
}

#[test]
fn same_generation_restart_recovers_idle_runtime_failure_and_fences_attachments() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    shared.publish_runtime_error(
        "event_pump_failed",
        HostedRunnerError::internal("native headless agent disconnected"),
    );
    drop(shared);

    let restarted = SharedRunner::new(config);
    let receipt = restarted
        .runtime_receipt()
        .expect("failed receipt survives restart without a turn");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(receipt.lifecycle_state, RuntimeLifecycleState::Failed);
    assert_eq!(receipt.error_type.as_deref(), Some("event_pump_failed"));
    let state = restarted.state.lock().expect("restarted state");
    assert!(!state.ready);
    assert!(!state.draining);
    assert!(state.runtime_failed);
    drop(state);
    let error = restarted
        .ensure_attachable()
        .expect_err("a failed generation must not become attachable after restart");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
}

#[tokio::test]
async fn retryable_dispatch_failure_does_not_fence_same_generation_restart() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let handle = start_hosted_runner_with_message_executor(
        config.clone(),
        Arc::new(RetryableDispatchFailureExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_retryable_dispatch").await;

    let response = client
        .post(format!(
            "{}/api/headless/threads/mrs_test/turns",
            handle.base_url()
        ))
        .header(
            "x-maestro-headless-connection-id",
            "conn_retryable_dispatch",
        )
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .header("x-maestro-runtime-generation", "0")
        .json(&json!({
            "protocolVersion": THREAD_PROTOCOL_VERSION,
            "turnId": "turn-retryable-dispatch",
            "kind": "user_message",
            "content": "dispatch may be retried"
        }))
        .send()
        .await
        .expect("dispatch response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    {
        let state = handle.shared.state.lock().expect("live state");
        assert!(
            state.ready,
            "a turn dispatch failure must not fence runtime readiness"
        );
        assert!(!state.runtime_failed);
        assert_eq!(
            state
                .thread
                .turn("turn-retryable-dispatch")
                .expect("failed turn remains auditable")
                .phase,
            ThreadPhase::Failed
        );
    }
    handle.shutdown().await;

    let restarted = start_hosted_runner_with_message_executor(
        config,
        Arc::new(RetryableDispatchFailureExecutor),
    )
    .await
    .expect("restart hosted runner");
    let receipt = restarted.shared.runtime_receipt().expect("restart receipt");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Ready);
    assert_eq!(
        receipt.lifecycle_state,
        RuntimeLifecycleState::ExecutionReady
    );
    {
        let state = restarted.shared.state.lock().expect("restarted state");
        assert!(state.ready);
        assert!(!state.runtime_failed);
    }
    restarted.shutdown().await;
}

#[test]
fn same_generation_restart_scans_past_failed_steer_for_interrupted_run() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    {
        let mut state = shared.state.lock().expect("state");
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "turn-active".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "active run".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            1,
        );
        state.thread.mark_dispatched(2);
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "turn-failed-steer".to_string(),
                kind: ThreadTurnKind::Steer,
                content: "retryable steer".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            3,
        );
        state.thread.mark_failed(4);
        assert_eq!(state.thread.phase(), ThreadPhase::Running);
        shared
            .persist_thread_for_request(&state)
            .expect("persist active run and failed steer");
    }
    drop(shared);

    let restarted = SharedRunner::new(config);
    let receipt = restarted
        .runtime_receipt()
        .expect("interrupted run must remain auditable after restart");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Terminal);
    assert_eq!(
        receipt.terminal,
        Some(RuntimeTerminalClassification::Interrupted)
    );
}

#[test]
fn same_generation_restart_does_not_resurrect_terminal_after_failed_user_turn() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    {
        let mut state = shared.state.lock().expect("state");
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "turn-completed-user".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "completed user turn".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            1,
        );
        state.thread.mark_dispatched(2);
        state.thread.apply_agent_message(
            &FromAgentMessage::TurnCompleted {
                response_id: "response-completed-user".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
            3,
        );
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "turn-failed-user".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "failed user turn".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            4,
        );
        state.thread.mark_failed(5);
        shared
            .persist_thread_for_request(&state)
            .expect("persist completed and failed user turns");
    }
    drop(shared);

    let restarted = SharedRunner::new(config);
    let receipt = restarted
        .runtime_receipt()
        .expect("restart receipt remains available");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Ready);
    assert_eq!(receipt.terminal, None);
    let state = restarted.state.lock().expect("restarted state");
    assert!(state.ready);
    assert!(!state.runtime_failed);
}

#[test]
fn replacement_generation_does_not_replay_old_lifecycle_envelopes_on_restart() {
    let workspace = tempdir().expect("workspace");
    let old_config = test_config(workspace.path().to_path_buf());
    let old = SharedRunner::new(old_config);
    {
        let mut state = old.state.lock().expect("old state");
        state.ready = false;
        state.draining = true;
        state.last_status = Some("Drained".to_string());
        assert!(old.publish_snapshot(&mut state));
    }
    drop(old);

    let replacement_config = test_config(workspace.path().to_path_buf()).with_runtime_generation(1);
    let replacement = SharedRunner::new(replacement_config.clone());
    let receipt = replacement
        .runtime_receipt()
        .expect("replacement generation receipt");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Ready);
    assert!(
        !replacement
            .state
            .lock()
            .expect("replacement state")
            .draining
    );
    drop(replacement);

    let restarted = SharedRunner::new(replacement_config);
    let receipt = restarted
        .runtime_receipt()
        .expect("replacement restart receipt");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Ready);
    assert!(!restarted.state.lock().expect("restarted state").draining);
}

#[tokio::test]
async fn runtime_receipt_endpoint_and_state_field_are_additive() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf())
        .with_auth_token("sentinel-runtime-receipt-auth-token");
    let handle = start_hosted_runner(config)
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let receipt: serde_json::Value = client
        .get(format!(
            "{}{}",
            handle.base_url(),
            HOSTED_RUNNER_RECEIPT_PATH
        ))
        .send()
        .await
        .expect("receipt response")
        .json()
        .await
        .expect("receipt json");
    assert_eq!(receipt["kind"], "ready");
    assert_eq!(receipt["lifecycleState"], "execution_ready");
    assert!(receipt["flushWatermark"].as_u64().unwrap_or_default() > 0);
    assert!(
        !receipt
            .to_string()
            .contains("sentinel-runtime-receipt-auth-token")
    );
    assert!(!receipt.to_string().contains("token"));

    let health: serde_json::Value = client
        .get(format!("{}/readyz", handle.base_url()))
        .send()
        .await
        .expect("health response")
        .json()
        .await
        .expect("health json");
    assert_eq!(health, json!({"ok": true}));
    assert!(health.get("runtime_receipt").is_none());

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .header(
            "authorization",
            "Bearer sentinel-runtime-receipt-auth-token",
        )
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["runtime_receipt"], receipt);
    assert!(state["state"].is_object());
    handle.shutdown().await;
}

#[test]
fn failed_terminal_journal_write_does_not_publish_or_replay_the_terminal() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let mut events = shared.events.subscribe();
    shared.fail_next_thread_persistences(1);

    let mut state = shared.state.lock().expect("state");
    shared.publish_message(
        &mut state,
        FromAgentMessage::TurnCompleted {
            response_id: "response_uncommitted".to_string(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        },
    );

    assert_eq!(state.cursor, 0, "failed boundary must restore its cursor");
    assert!(state.runtime_failed);
    assert!(!state.ready);
    assert!(state.envelopes.iter().all(|envelope| !matches!(
        envelope,
        StreamEnvelope::Message { message, .. }
            if matches!(message.as_ref(), FromAgentMessage::TurnCompleted { .. })
    )));
    assert!(state.envelopes.iter().any(|envelope| matches!(
        envelope,
        StreamEnvelope::Snapshot { snapshot }
            if snapshot.state.last_status.as_deref() == Some("Runtime failed")
    )));
    assert!(
        state.runtime_receipt.is_none(),
        "an uncommitted lifecycle failure must not expose the prior Ready receipt"
    );
    // A later terminal message from the same drained executor batch must not
    // replace the failed boundary with terminal evidence.
    shared.publish_message(
        &mut state,
        FromAgentMessage::TurnCompleted {
            response_id: "response_trailing_after_failure".to_string(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        },
    );
    shared.retry_thread_persistence(&mut state);
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Failed)
    );
    assert!(state.envelopes.iter().all(|envelope| !matches!(
        envelope,
        StreamEnvelope::Message { message, .. }
            if matches!(message.as_ref(), FromAgentMessage::TurnCompleted { .. })
    )));
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

struct FailingTerminalFlushExecutor {
    flush_calls: Arc<AtomicUsize>,
}

impl HostedRunnerHeadlessMessageExecutor for FailingTerminalFlushExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "session flush failure fixture",
        ))
    }

    fn flush_session(&self) -> Result<Option<PathBuf>, HostedRunnerError> {
        self.flush_calls.fetch_add(1, Ordering::AcqRel);
        Err(HostedRunnerError::internal(
            "scripted session recorder flush failure",
        ))
    }
}

#[test]
fn terminal_receipt_waits_for_session_recorder_flush() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let flush_calls = Arc::new(AtomicUsize::new(0));
    let shared = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(FailingTerminalFlushExecutor {
            flush_calls: flush_calls.clone(),
        }),
        None,
    );
    let mut events = shared.events.subscribe();

    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "session-flush-failure-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "terminal requires a flushed session".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "session-flush-failure-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );

        assert_eq!(flush_calls.load(Ordering::Acquire), 1);
        assert!(state.runtime_failed);
        assert!(!state.ready);
        assert_eq!(
            state.last_error_type.as_deref(),
            Some("session_recorder_flush_failed")
        );
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Failed)
        );
        assert!(state.envelopes.iter().any(|envelope| {
            matches!(
                envelope,
                StreamEnvelope::Snapshot { snapshot }
                    if snapshot.state.last_status.as_deref() == Some("Runtime failed")
            )
        }));
        assert!(state.envelopes.iter().all(|envelope| !matches!(
            envelope,
            StreamEnvelope::Message { message, .. }
                if matches!(message.as_ref(), FromAgentMessage::TurnCompleted { .. })
        )));
    }

    let live_receipt = shared.runtime_receipt().expect("failed receipt");
    assert_eq!(live_receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(
        live_receipt.error_type.as_deref(),
        Some("session_recorder_flush_failed")
    );
    assert!(matches!(
        events.try_recv(),
        Ok(StreamEnvelope::Snapshot { snapshot })
            if snapshot.state.last_status.as_deref() == Some("Runtime failed")
    ));

    drop(shared);
    let restarted = SharedRunner::new(config);
    let restarted_receipt = restarted
        .runtime_receipt()
        .expect("failed flush boundary must survive restart");
    assert_eq!(restarted_receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(
        restarted_receipt.error_type.as_deref(),
        Some("session_recorder_flush_failed")
    );
}

#[test]
fn failed_session_flush_does_not_replay_terminal_when_failure_fence_write_fails() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let flush_calls = Arc::new(AtomicUsize::new(0));
    let shared = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(FailingTerminalFlushExecutor {
            flush_calls: flush_calls.clone(),
        }),
        None,
    );
    shared.fail_next_thread_persistences(1);

    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "session-flush-fence-write-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "do not replay an unflushed terminal".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "session-flush-fence-write-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
        assert_eq!(flush_calls.load(Ordering::Acquire), 1);
        assert!(state.runtime_receipt.is_none());
        assert!(state.envelopes.iter().all(|envelope| !matches!(
            envelope,
            StreamEnvelope::Message { message, .. }
                if matches!(message.as_ref(), FromAgentMessage::TurnCompleted { .. })
        )));
        assert!(
            shared
                .thread_persistence_retry_pending
                .load(Ordering::Acquire)
        );
    }

    drop(shared);
    let restarted = SharedRunner::new(config);
    let restarted_receipt = restarted
        .runtime_receipt()
        .expect("prior ready boundary remains the only durable evidence");
    assert_eq!(restarted_receipt.kind, RuntimeReceiptKind::Ready);
    assert_ne!(restarted_receipt.kind, RuntimeReceiptKind::Terminal);
}

#[test]
fn terminal_receipt_keeps_durable_model_provider_binding_across_restart() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(StatefulRuntimeExecutor::new(AgentState {
            model: Some("durable-model".to_string()),
            provider: Some("durable-provider".to_string()),
            ..AgentState::default()
        })),
        None,
    );
    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "durable-binding-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "persist the model binding".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "durable-binding-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::Ready {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                model: "late-ready-model".to_string(),
                provider: "late-ready-provider".to_string(),
                session_id: None,
            },
        );
    }
    let live_receipt = shared.runtime_receipt().expect("live terminal receipt");
    assert_eq!(live_receipt.model.as_deref(), Some("durable-model"));
    assert_eq!(live_receipt.provider.as_deref(), Some("durable-provider"));
    drop(shared);

    let restarted = SharedRunner::new(config);
    let restarted_receipt = restarted
        .runtime_receipt()
        .expect("terminal receipt after restart");
    assert_eq!(restarted_receipt.kind, RuntimeReceiptKind::Terminal);
    assert_eq!(restarted_receipt.model.as_deref(), Some("durable-model"));
    assert_eq!(
        restarted_receipt.provider.as_deref(),
        Some("durable-provider")
    );
}

#[test]
fn next_generation_does_not_reuse_persisted_model_provider_binding() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(StatefulRuntimeExecutor::new(AgentState {
            model: Some("generation-zero-model".to_string()),
            provider: Some("generation-zero-provider".to_string()),
            ..AgentState::default()
        })),
        None,
    );
    drop(shared);

    let next_generation = SharedRunner::new(config.with_runtime_generation(1));
    let receipt = next_generation
        .runtime_receipt()
        .expect("next-generation ready receipt");
    assert_eq!(receipt.model.as_deref(), Some("rust-hosted-runner"));
    assert_eq!(receipt.provider.as_deref(), Some("rust"));
}

#[test]
fn fatal_runtime_error_retries_failed_boundary_after_pump_stop() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    shared.fail_next_thread_persistences(1);

    shared.publish_runtime_error(
        "event_pump_failed",
        HostedRunnerError::internal("event pump stopped"),
    );

    let receipt = shared
        .runtime_receipt()
        .expect("synchronous retry must publish failed receipt");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(receipt.error_type.as_deref(), Some("event_pump_failed"));
    drop(shared);

    let restarted = SharedRunner::new(config);
    let restarted_receipt = restarted
        .runtime_receipt()
        .expect("failed boundary must survive restart");
    assert_eq!(restarted_receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(
        restarted_receipt.error_type.as_deref(),
        Some("event_pump_failed")
    );
}

#[tokio::test]
async fn fatal_runtime_error_recovery_continues_after_inline_retry_failure() {
    struct FailingDrainExecutor;
    impl HostedRunnerHeadlessMessageExecutor for FailingDrainExecutor {
        fn execute(
            &self,
            _context: &HostedRunnerHeadlessMessageContext,
            _message: ToAgentMessage,
        ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
            Ok(HostedRunnerHeadlessMessageResult::transport_only(
                Vec::new(),
                "scripted",
            ))
        }

        fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
            Err(HostedRunnerError::internal("scripted drain failure"))
        }
    }

    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(FailingDrainExecutor),
        None,
    );
    // The fatal event write and the inline recovery retry both fail. The
    // independent recovery supervisor must complete the durable failure
    // boundary after the event-pump error path returns.
    shared.fail_next_thread_persistences(2);

    assert_eq!(pump_tick(&shared), PumpTick::Stop);

    let receipt = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(receipt) = shared.runtime_receipt() {
                if receipt.kind == RuntimeReceiptKind::Failed {
                    break receipt;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("independent recovery supervisor must converge");
    assert_eq!(receipt.error_type.as_deref(), Some("event_pump_failed"));
    assert!(
        !shared
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
    );
    drop(shared);

    let restarted = SharedRunner::new(config);
    let restarted_receipt = restarted
        .runtime_receipt()
        .expect("recovered failure boundary must survive restart");
    assert_eq!(restarted_receipt.kind, RuntimeReceiptKind::Failed);
    assert_eq!(
        restarted_receipt.error_type.as_deref(),
        Some("event_pump_failed")
    );
}

#[tokio::test]
async fn shutdown_cancels_persistence_recovery_before_releasing_journal() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let handle = start_hosted_runner_with_message_executor(
        config.clone(),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");

    handle
        .shared
        .stop_event_pump()
        .await
        .expect("stop event pump");
    handle.shared.fail_next_thread_persistences(usize::MAX);
    handle.shared.publish_runtime_error(
        "shutdown_persistence_failure",
        HostedRunnerError::internal("journal remains unwritable during shutdown test"),
    );
    assert!(
        handle
            .shared
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !handle
            .shared
            .thread_persistence_recovery_running
            .load(Ordering::Acquire)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("persistence recovery supervisor must start");

    handle.shutdown().await;

    let restarted =
        start_hosted_runner_with_message_executor(config, Arc::new(ScriptedRuntimeExecutor))
            .await
            .expect("shutdown must release the journal lock before restart");
    restarted.shutdown().await;
}

#[tokio::test]
async fn stopped_event_pump_schedules_deferred_persistence_recovery() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let handle =
        start_hosted_runner_with_message_executor(config, Arc::new(ScriptedRuntimeExecutor))
            .await
            .expect("start hosted runner");

    handle
        .shared
        .stop_event_pump()
        .await
        .expect("stop event pump");
    handle.shared.fail_next_thread_persistences(2);
    {
        let mut state = handle.shared.state.lock().expect("runner state");
        assert!(!handle.shared.publish_snapshot(&mut state));
    }
    assert!(
        handle
            .shared
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
    );

    tokio::time::timeout(Duration::from_secs(2), async {
        while handle
            .shared
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deferred persistence must recover after the event pump stops");

    handle.shutdown().await;
}

#[test]
fn failed_snapshot_persistence_clears_ready_receipt_and_arms_retry() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    shared.fail_next_thread_persistences(1);

    let mut state = shared.state.lock().expect("state");
    assert!(!shared.publish_snapshot(&mut state));
    assert!(state.runtime_failed);
    assert!(state.runtime_receipt.is_none());
    shared.retry_thread_persistence(&mut state);
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Failed)
    );
}

#[test]
fn failed_snapshot_persistence_replaces_a_terminal_receipt_after_retry() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared.state.lock().expect("state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "snapshot-terminal-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "terminal before snapshot failure".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "snapshot-terminal-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Terminal)
        );

        shared.fail_next_thread_persistences(1);
        assert!(!shared.publish_snapshot(&mut state));
        assert!(state.runtime_failed);
        assert!(state.runtime_receipt.is_none());
        shared.retry_thread_persistence(&mut state);
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Failed)
        );
    }
}

#[test]
fn published_turn_terminals_survive_journal_reload_with_their_replay_envelopes() {
    for terminal in [
        FromAgentMessage::TurnCompleted {
            response_id: "response_completed".to_string(),
            coding_completion: None,
            coding_child_records: Vec::new(),
        },
        FromAgentMessage::ProviderError {
            kind: maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
            message: "provider rejected the response".to_string(),
        },
    ] {
        let workspace = tempdir().expect("workspace");
        let config = test_config(workspace.path().to_path_buf());
        let shared = SharedRunner::new(config.clone());
        let expected_completed = matches!(terminal, FromAgentMessage::TurnCompleted { .. });
        {
            let mut state = shared.state.lock().expect("state");
            state.thread.append(
                AppendTurnRequest {
                    protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                    turn_id: "reload-terminal-turn".to_string(),
                    kind: ThreadTurnKind::UserMessage,
                    content: "durably terminal turn".to_string(),
                    attachments: None,
                    code_mode: None,
                    tool_grant: None,
                    managed_inference_authorization: None,
                },
                1,
            );
            state.thread.mark_dispatched(2);
            shared.publish_message(&mut state, terminal);
        }
        drop(shared);

        let reloaded = SharedRunner::new(config);
        let state = reloaded.state.lock().expect("reloaded state");
        assert!(state.envelopes.iter().any(|envelope| match envelope {
            StreamEnvelope::Message { message, .. } if expected_completed => {
                matches!(message.as_ref(), FromAgentMessage::TurnCompleted { .. })
            }
            StreamEnvelope::Message { message, .. } => matches!(
                message.as_ref(),
                FromAgentMessage::ProviderError {
                    kind: maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
                    ..
                }
            ),
            _ => false,
        }));
        let receipt = state.runtime_receipt.as_ref().expect("reloaded receipt");
        assert_eq!(receipt.kind, RuntimeReceiptKind::Terminal);
        assert_eq!(
            receipt.terminal,
            Some(if expected_completed {
                RuntimeTerminalClassification::Completed
            } else {
                RuntimeTerminalClassification::ProviderFailed
            })
        );
    }
}

#[test]
fn startup_receipt_fences_old_generations_and_prefers_a_restored_interrupted_turn() {
    let workspace = tempdir().expect("workspace");
    let first_config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(first_config.clone());
    {
        let mut state = shared.state.lock().expect("state");
        let interrupted_turn_cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "completed-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "completed work".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            0,
        );
        state.thread.mark_dispatched(1);
        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "response-completed".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "interrupted-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "crashed work".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            interrupted_turn_cursor,
        );
        shared
            .persist_thread_for_request(&state)
            .expect("persist interrupted turn");
    }
    drop(shared);

    let same_generation = SharedRunner::new(first_config);
    let same_generation_receipt = same_generation
        .runtime_receipt()
        .expect("same-generation startup receipt");
    assert_eq!(same_generation_receipt.kind, RuntimeReceiptKind::Terminal);
    assert_eq!(
        same_generation_receipt.terminal,
        Some(RuntimeTerminalClassification::Interrupted)
    );
    drop(same_generation);

    let next_generation =
        SharedRunner::new(test_config(workspace.path().to_path_buf()).with_runtime_generation(1));
    let next_generation_receipt = next_generation
        .runtime_receipt()
        .expect("next-generation startup receipt");
    assert_eq!(next_generation_receipt.kind, RuntimeReceiptKind::Ready);
    assert_eq!(next_generation_receipt.terminal, None);
    drop(next_generation);

    let next_generation_restart =
        SharedRunner::new(test_config(workspace.path().to_path_buf()).with_runtime_generation(1));
    let next_generation_restart_receipt = next_generation_restart
        .runtime_receipt()
        .expect("rebinding the old journal must not resurrect its terminal turn");
    assert_eq!(
        next_generation_restart_receipt.kind,
        RuntimeReceiptKind::Ready
    );
    assert_eq!(next_generation_restart_receipt.terminal, None);
}

#[test]
fn same_generation_restart_recovers_idle_drained_boundary_and_fences_attachments() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    {
        let mut state = shared.state.lock().expect("state");
        state.ready = false;
        state.draining = true;
        state.last_status = Some("Drained".to_string());
        state.thread.set_snapshot_lineage("snapshot:drain-test");
        assert!(shared.publish_snapshot(&mut state));
    }
    drop(shared);

    let restarted = SharedRunner::new(config);
    let receipt = restarted
        .runtime_receipt()
        .expect("drained receipt survives restart without a turn");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Drained);
    assert_eq!(receipt.lifecycle_state, RuntimeLifecycleState::Drained);
    assert_eq!(receipt.runtime_generation, 0);
    assert_eq!(
        receipt.snapshot_lineage.as_deref(),
        Some("snapshot:drain-test")
    );
    let state = restarted.state.lock().expect("restarted state");
    assert!(!state.ready);
    assert!(state.draining);
    assert_eq!(state.last_status.as_deref(), Some("Drained"));
    drop(state);
    let error = restarted
        .ensure_attachable()
        .expect_err("a drained generation must not become attachable after restart");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeNotReady);
}

#[test]
fn terminal_error_classifications_survive_journal_reload() {
    let cases = [
        (
            FromAgentMessage::Error {
                request_id: None,
                message: "cancelled".to_string(),
                fatal: false,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Cancelled),
            },
            RuntimeReceiptKind::Terminal,
            Some(RuntimeTerminalClassification::Cancelled),
            None,
        ),
        (
            FromAgentMessage::Error {
                request_id: None,
                message: "protocol".to_string(),
                fatal: false,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Protocol),
            },
            RuntimeReceiptKind::Terminal,
            Some(RuntimeTerminalClassification::Protocol),
            None,
        ),
        (
            FromAgentMessage::Error {
                request_id: None,
                message: "tool".to_string(),
                fatal: false,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Tool),
            },
            RuntimeReceiptKind::Terminal,
            Some(RuntimeTerminalClassification::Tool),
            None,
        ),
        (
            FromAgentMessage::Error {
                request_id: None,
                message: "transient".to_string(),
                fatal: false,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Transient),
            },
            RuntimeReceiptKind::Terminal,
            Some(RuntimeTerminalClassification::Transient),
            None,
        ),
        (
            FromAgentMessage::Error {
                request_id: None,
                message: "untyped".to_string(),
                fatal: false,
                terminal: true,
                error_type: None,
            },
            RuntimeReceiptKind::Terminal,
            Some(RuntimeTerminalClassification::NonFatal),
            None,
        ),
        (
            FromAgentMessage::Error {
                request_id: None,
                message: "fatal".to_string(),
                fatal: true,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Fatal),
            },
            RuntimeReceiptKind::Failed,
            None,
            Some("fatal"),
        ),
    ];

    for (message, expected_kind, expected_terminal, expected_error_type) in cases {
        let workspace = tempdir().expect("workspace");
        let config = test_config(workspace.path().to_path_buf());
        let shared = SharedRunner::new(config.clone());
        {
            let mut state = shared.state.lock().expect("state");
            let cursor = state.cursor;
            state.thread.append(
                AppendTurnRequest {
                    protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                    turn_id: "classification-turn".to_string(),
                    kind: ThreadTurnKind::UserMessage,
                    content: "durable classification".to_string(),
                    attachments: None,
                    code_mode: None,
                    tool_grant: None,
                    managed_inference_authorization: None,
                },
                cursor,
            );
            state.thread.mark_dispatched(cursor.saturating_add(1));
            shared.publish_message(&mut state, message);
        }
        drop(shared);

        let reloaded = SharedRunner::new(config);
        let receipt = reloaded.runtime_receipt().expect("reloaded receipt");
        assert_eq!(receipt.kind, expected_kind);
        assert_eq!(receipt.terminal, expected_terminal);
        assert_eq!(receipt.error_type.as_deref(), expected_error_type);
    }
}

#[test]
fn ordinary_not_ready_runtime_remains_retryable() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared.state.lock().expect("state");
        state.ready = false;
        state.runtime_failed = false;
    }

    let error = shared
        .ensure_mutation_allowed()
        .expect_err("ordinary not-ready runtime must remain retryable");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeNotReady);
    assert_eq!(error.status, 503);
}

#[test]
fn new_connection_adopts_private_capability_for_idempotent_retry() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let mut state = shared.state.lock().expect("state");
    let client_capability = "cap_00112233445566778899aabbccddeeff";
    let upsert = |connection_capability: Option<&str>| ConnectionUpsert {
        connection_id: "conn_idempotent".to_string(),
        connection_capability: connection_capability.map(str::to_string),
        connection_capability_required: true,
        role: ConnectionRole::Viewer,
        client_protocol_version: None,
        client_info: None,
        capabilities: None,
        opt_out_notifications: vec![],
        take_control: false,
    };

    let accepted = upsert_connection(&mut state, upsert(Some(client_capability)))
        .expect("new connection should accept client-generated private authority");
    assert_eq!(accepted.as_deref(), Some(client_capability));

    let retried = upsert_connection(&mut state, upsert(Some(client_capability)))
        .expect("same private authority should make creation retry idempotent");
    assert_eq!(retried.as_deref(), Some(client_capability));

    let attacker = upsert_connection(&mut state, upsert(None))
        .expect_err("public connection id alone must not authorize a retry");
    assert_eq!(attacker.code, HostedRunnerErrorCode::AccessDenied);

    let downgrade = upsert_connection(
        &mut state,
        ConnectionUpsert {
            connection_id: "conn_idempotent".to_string(),
            connection_capability: None,
            connection_capability_required: false,
            role: ConnectionRole::Viewer,
            client_protocol_version: Some("2026-04-02".to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: vec![],
            take_control: false,
        },
    )
    .expect_err("secure connection authority must not downgrade to legacy");
    assert_eq!(downgrade.code, HostedRunnerErrorCode::AccessDenied);
}

#[tokio::test]
async fn hosted_runner_identity_binding_rejects_thread_ids_and_accepts_runner_session_id() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let mismatched = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "thread-canary-1",
            "connectionId": "conn-thread-canary",
            "role": "controller"
        }))
        .send()
        .await
        .expect("mismatched connection response");
    assert_eq!(mismatched.status(), StatusCode::NOT_FOUND);
    let mismatch_body: serde_json::Value =
        mismatched.json().await.expect("mismatched connection json");
    assert_eq!(mismatch_body["error_type"], "stale_session");
    assert_eq!(
        mismatch_body["details"]["identity_binding"]["requested_session_id"],
        "thread-canary-1"
    );
    assert_eq!(
        mismatch_body["details"]["identity_binding"]["expected_runner_session_id"],
        "mrs_test"
    );
    assert_eq!(
        mismatch_body["details"]["identity_binding"]["bound_maestro_session_id"],
        "sess_test"
    );

    let journal_path = workspace
        .path()
        .join(".maestro")
        .join("hosted-runner")
        .join("threads");
    let journal_path = std::fs::read_dir(journal_path)
        .expect("identity failure journal directory")
        .map(|entry| entry.expect("identity failure journal entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("identity failure journal");
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal_path).expect("identity failure journal"))
            .expect("identity failure journal json");
    assert_eq!(
        journal["identity_binding_failures"][0]["requested_session_id"],
        "thread-canary-1"
    );

    let connection = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "mrs_test",
            "connectionId": "conn-runner-session",
            "role": "controller"
        }))
        .send()
        .await
        .expect("runner session connection response");
    assert_eq!(connection.status(), StatusCode::OK);

    let state = client
        .get(format!(
            "{}/api/headless/sessions/mrs_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("runner session state response");
    assert_eq!(state.status(), StatusCode::OK);
    let state_body: serde_json::Value = state.json().await.expect("runner session state json");
    assert_eq!(state_body["session_id"], "sess_test");

    handle.shutdown().await;

    let restarted = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("restart hosted runner");
    let reloaded_journal: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&journal_path).expect("reloaded identity failure journal"),
    )
    .expect("reloaded identity failure journal json");
    assert_eq!(
        reloaded_journal["identity_binding_failures"]
            .as_array()
            .expect("reloaded identity failure list")
            .len(),
        1
    );

    let runner_thread = client
        .get(format!(
            "{}/api/headless/threads/mrs_test",
            restarted.base_url()
        ))
        .send()
        .await
        .expect("runner alias thread response");
    assert_eq!(runner_thread.status(), StatusCode::OK);
    let runner_thread_body: serde_json::Value =
        runner_thread.json().await.expect("runner thread json");
    assert_eq!(runner_thread_body["thread_id"], "mrs_test");
    assert_eq!(runner_thread_body["runtime"]["session_id"], "sess_test");

    let runner_thread_events = client
        .get(format!(
            "{}/api/headless/threads/mrs_test/events?cursor=0",
            restarted.base_url()
        ))
        .send()
        .await
        .expect("runner alias thread events response");
    assert_eq!(runner_thread_events.status(), StatusCode::OK);

    let maestro_alias_thread = client
        .get(format!(
            "{}/api/headless/threads/sess_test",
            restarted.base_url()
        ))
        .send()
        .await
        .expect("thread response");
    assert_eq!(maestro_alias_thread.status(), StatusCode::NOT_FOUND);

    let restarted_state = client
        .get(format!(
            "{}/api/headless/sessions/mrs_test/state",
            restarted.base_url()
        ))
        .send()
        .await
        .expect("restarted runner session state response");
    assert_eq!(restarted_state.status(), StatusCode::OK);

    restarted.shutdown().await;
}

#[test]
fn hosted_runner_events_validate_the_route_identity_boundary() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));

    handle_events(
        shared.clone(),
        "mrs_test",
        HashMap::new(),
        EventsRouteIdentity::Thread,
    )
    .expect("thread events should accept the runner session id");

    let maestro_alias = match handle_events(
        shared.clone(),
        "sess_test",
        HashMap::new(),
        EventsRouteIdentity::Thread,
    ) {
        Ok(_) => panic!("thread events must reject the Maestro-session alias"),
        Err(error) => error,
    };
    assert_eq!(maestro_alias.code, HostedRunnerErrorCode::StaleSession);
    handle_events(
        shared,
        "mrs_test",
        HashMap::new(),
        EventsRouteIdentity::Session,
    )
    .expect("session events should accept the runner session id");
}

#[test]
fn identity_binding_failure_ids_are_bounded_without_changing_short_ids() {
    let short = "thread-canary-1";
    assert_eq!(bounded_identity_binding_id(short), short);

    let oversized = "x".repeat(MAX_IDENTITY_BINDING_FAILURE_ID_BYTES + 128);
    let bounded = bounded_identity_binding_id(&oversized);
    assert_eq!(bounded.len(), MAX_IDENTITY_BINDING_FAILURE_ID_BYTES);
    assert!(bounded.ends_with('…'));
    assert_eq!(
        bounded,
        format!(
            "{}…",
            "x".repeat(MAX_IDENTITY_BINDING_FAILURE_ID_BYTES - "…".len())
        )
    );
}

#[derive(Debug)]
struct ScriptedRuntimeExecutor;

struct RetryableDispatchFailureExecutor;

impl HostedRunnerHeadlessMessageExecutor for RetryableDispatchFailureExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Err(HostedRunnerError::internal(
            "transient supervisor unavailable",
        ))
    }
}

struct SessionArtifactExecutor {
    session_file: std::path::PathBuf,
}

impl HostedRunnerHeadlessMessageExecutor for SessionArtifactExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "session artifact fixture does not execute agent messages",
        ))
    }

    fn flush_session(&self) -> Result<Option<std::path::PathBuf>, HostedRunnerError> {
        Ok(Some(self.session_file.clone()))
    }
}

impl HostedRunnerHeadlessMessageExecutor for ScriptedRuntimeExecutor {
    fn execute(
        &self,
        context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        match message {
            ToAgentMessage::Prompt { content, .. } => {
                assert_eq!(context.session_id, "sess_test");
                assert!(
                    matches!(context.connection_id.as_str(), "conn_exec" | "conn_second"),
                    "unexpected scripted executor connection: {}",
                    context.connection_id
                );
                assert!(context.subscription_id.as_deref().is_some());
                if content == "client-tool-boundary" {
                    return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                        vec![
                            FromAgentMessage::ClientToolRequest {
                                call_id: "client-call".into(),
                                tool_execution_id: None,
                                tool: "bash".into(),
                                args: serde_json::json!({
                                    "command": "curl -H 'Authorization: Bearer client-execution-secret' example.test",
                                    "nested": {"value": "client-byte-faithful"}
                                }),
                            },
                            FromAgentMessage::ServerRequest {
                                request_id: "server-client-tool".into(),
                                request_type: ServerRequestType::ClientTool,
                                call_id: "server-call".into(),
                                tool_execution_id: None,
                                tool: "bash".into(),
                                args: serde_json::json!({
                                    "command": "curl -H 'Authorization: Bearer server-execution-secret' example.test",
                                    "nested": ["server-byte-faithful", 7]
                                }),
                                reason: "run on the authenticated controller".into(),
                                started_at_ms: None,
                            },
                        ],
                        "published executable client tool requests",
                    ));
                }
                Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                    vec![
                        FromAgentMessage::ResponseStart {
                            response_id: "resp-hosted-1".to_string(),
                        },
                        FromAgentMessage::ResponseChunk {
                            response_id: "resp-hosted-1".to_string(),
                            content: format!("runtime: {content}"),
                            is_thinking: false,
                        },
                        FromAgentMessage::ResponseEnd {
                            response_id: "resp-hosted-1".to_string(),
                            usage: None,
                            tools_summary: None,
                            duration_ms: Some(7),
                            ttft_ms: Some(3),
                        },
                    ],
                    "Rust hosted runner message handled by runtime executor",
                ))
            }
            _ => Ok(HostedRunnerHeadlessMessageResult::transport_only(
                Vec::new(),
                "scripted executor ignored message",
            )),
        }
    }
}

struct StatefulRuntimeExecutor {
    state: Mutex<AgentState>,
}

impl StatefulRuntimeExecutor {
    fn new(state: AgentState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }
}

impl HostedRunnerHeadlessMessageExecutor for StatefulRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "accepted by stateful runtime",
        ))
    }

    fn state(&self) -> Result<Option<AgentState>, HostedRunnerError> {
        Ok(Some(self.state.lock().expect("state").clone()))
    }
}

struct CompletingRuntimeExecutor {
    state: Mutex<AgentState>,
}

#[derive(Debug, Default)]
struct RecordingThreadExecutor {
    prompts: Mutex<Vec<String>>,
}

#[derive(Debug, Default)]
struct ResponseRecordingExecutor {
    messages: Mutex<Vec<ToAgentMessage>>,
}

#[derive(Debug, Default)]
struct PendingThreadExecutor {
    prompts: Mutex<Vec<String>>,
}

#[derive(Debug, Default)]
struct TerminalThenPendingExecutor {
    calls: Mutex<usize>,
}

#[derive(Default)]
struct DispatchPersistenceFailureExecutor {
    shared: Mutex<Option<SharedRunner>>,
    prompts: Mutex<Vec<String>>,
}

#[derive(Debug, Default)]
struct SteeringLifecycleExecutor {
    queued: Mutex<Vec<FromAgentMessage>>,
}

impl SteeringLifecycleExecutor {
    fn end_model_response(&self) {
        self.queued.lock().expect("steering lifecycle events").push(
            FromAgentMessage::ResponseEnd {
                response_id: "response-steered".to_string(),
                usage: None,
                tools_summary: None,
                duration_ms: Some(1),
                ttft_ms: Some(1),
            },
        );
    }

    fn complete_active_run(&self) {
        self.queued.lock().expect("steering lifecycle events").push(
            FromAgentMessage::TurnCompleted {
                response_id: "turn-steered".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );
    }
}

impl HostedRunnerHeadlessMessageExecutor for SteeringLifecycleExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "steering lifecycle fixture accepted message",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        Ok(HostedRunnerDrainResult {
            messages: std::mem::take(&mut *self.queued.lock().expect("steering lifecycle events")),
            consumed_response_keys: Vec::new(),
            rejected_response_keys: Vec::new(),
        })
    }
}

impl HostedRunnerHeadlessMessageExecutor for PendingThreadExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        match message {
            ToAgentMessage::Prompt { content, .. } | ToAgentMessage::Steer { content, .. } => {
                self.prompts.lock().expect("pending prompts").push(content);
            }
            _ => {}
        }
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "thread prompt remains active",
        ))
    }
}

impl HostedRunnerHeadlessMessageExecutor for TerminalThenPendingExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        let mut calls = self.calls.lock().expect("terminal-then-pending calls");
        let messages = if *calls == 0 {
            vec![
                FromAgentMessage::ResponseStart {
                    response_id: "response-turn-a".to_string(),
                },
                FromAgentMessage::ResponseEnd {
                    response_id: "response-turn-a".to_string(),
                    usage: None,
                    tools_summary: None,
                    duration_ms: Some(1),
                    ttft_ms: Some(1),
                },
                FromAgentMessage::TurnCompleted {
                    response_id: "turn-a".to_string(),
                    coding_completion: None,
                    coding_child_records: Vec::new(),
                },
            ]
        } else {
            Vec::new()
        };
        *calls += 1;
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            messages,
            "terminal-then-pending fixture handled prompt",
        ))
    }
}

impl HostedRunnerHeadlessMessageExecutor for DispatchPersistenceFailureExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        if let ToAgentMessage::Prompt { content, .. } = message {
            self.prompts
                .lock()
                .expect("dispatch persistence prompts")
                .push(content);
        }
        self.shared
            .lock()
            .expect("dispatch persistence shared runner")
            .as_ref()
            .expect("shared runner installed before dispatch")
            .fail_next_thread_persistences(1);
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "runtime accepted before dispatch persistence failed",
        ))
    }
}

impl HostedRunnerHeadlessMessageExecutor for RecordingThreadExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        let content = match message {
            ToAgentMessage::Prompt { content, .. } | ToAgentMessage::Steer { content, .. } => {
                content
            }
            _ => {
                return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                    Vec::new(),
                    "thread fixture ignored non-prompt message",
                ));
            }
        };
        self.prompts
            .lock()
            .expect("recorded prompts")
            .push(content.clone());
        let messages = if content == "needs approval" {
            vec![FromAgentMessage::ToolCall {
                call_id: "call-1".to_string(),
                tool_execution_id: None,
                tool: "bash".to_string(),
                args: json!({"command": "deploy"}),
                requires_approval: true,
            }]
        } else {
            vec![
                FromAgentMessage::ResponseStart {
                    response_id: format!("response-{content}"),
                },
                FromAgentMessage::ResponseEnd {
                    response_id: format!("response-{content}"),
                    usage: None,
                    tools_summary: None,
                    duration_ms: Some(1),
                    ttft_ms: Some(1),
                },
                FromAgentMessage::TurnCompleted {
                    response_id: format!("turn-{content}"),
                    coding_completion: None,
                    coding_child_records: Vec::new(),
                },
            ]
        };
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            messages,
            "thread fixture handled prompt",
        ))
    }
}

impl HostedRunnerHeadlessMessageExecutor for ResponseRecordingExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        self.messages
            .lock()
            .expect("recorded responses")
            .push(message);
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "response fixture handled message",
        ))
    }
}

#[derive(Debug, Default)]
struct PumpOnlyRuntimeExecutor {
    queued: Arc<Mutex<Vec<FromAgentMessage>>>,
}

#[derive(Debug)]
struct BlockedPromptRuntimeExecutor {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<AtomicBool>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl BlockedPromptRuntimeExecutor {
    fn new() -> Self {
        Self {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            started: Arc::new(AtomicBool::new(false)),
            release: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn release(&self) {
        let (released, wake) = &*self.release;
        *released.lock().expect("release state") = true;
        wake.notify_all();
    }
}

impl HostedRunnerHeadlessMessageExecutor for BlockedPromptRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        if matches!(message, ToAgentMessage::Prompt { .. }) || is_control_response_message(&message)
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.store(true, Ordering::SeqCst);
            let (released, wake) = &*self.release;
            let mut released = released.lock().expect("release state");
            while !*released {
                released = wake.wait(released).expect("release state");
            }
            return Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                vec![FromAgentMessage::ResponseStart {
                    response_id: "response-blocked".to_string(),
                }],
                "released blocked prompt",
            ));
        }
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "ignored by blocked prompt executor",
        ))
    }
}

#[derive(Debug)]
struct FailingPumpRuntimeExecutor;

impl HostedRunnerHeadlessMessageExecutor for FailingPumpRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "ignored by failing pump executor",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        Err(HostedRunnerError::internal("pump drain failed"))
    }
}

impl HostedRunnerHeadlessMessageExecutor for PumpOnlyRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        if matches!(message, ToAgentMessage::Prompt { .. }) {
            self.queued.lock().expect("queued messages").extend([
                FromAgentMessage::ResponseStart {
                    response_id: "response-1".to_string(),
                },
                FromAgentMessage::ResponseChunk {
                    response_id: "response-1".to_string(),
                    content: "persisted".to_string(),
                    is_thinking: false,
                },
                FromAgentMessage::ResponseEnd {
                    response_id: "response-1".to_string(),
                    usage: None,
                    tools_summary: None,
                    duration_ms: None,
                    ttft_ms: None,
                },
            ]);
        }
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "queued events for the hosted runner event pump",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        Ok(HostedRunnerDrainResult {
            messages: std::mem::take(&mut *self.queued.lock().expect("queued messages")),
            consumed_response_keys: Vec::new(),
            rejected_response_keys: Vec::new(),
        })
    }
}

#[derive(Debug)]
struct NotifyingPumpRuntimeExecutor {
    queued: Arc<Mutex<Vec<FromAgentMessage>>>,
    notification: Arc<Notify>,
}

impl Default for NotifyingPumpRuntimeExecutor {
    fn default() -> Self {
        Self {
            queued: Arc::new(Mutex::new(Vec::new())),
            notification: Arc::new(Notify::new()),
        }
    }
}

impl NotifyingPumpRuntimeExecutor {
    fn queue_ready_event(&self) {
        self.queued
            .lock()
            .expect("queued ready event")
            .push(FromAgentMessage::Ready {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                model: "test-model".to_string(),
                provider: "test-provider".to_string(),
                session_id: Some("sess_test".to_string()),
            });
        self.notification.notify_one();
    }
}

impl HostedRunnerHeadlessMessageExecutor for NotifyingPumpRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "notification fixture accepted message",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        Ok(HostedRunnerDrainResult {
            messages: std::mem::take(&mut *self.queued.lock().expect("queued ready event")),
            consumed_response_keys: Vec::new(),
            rejected_response_keys: Vec::new(),
        })
    }

    fn event_notification(&self) -> Result<Option<Arc<Notify>>, HostedRunnerError> {
        Ok(Some(Arc::clone(&self.notification)))
    }
}

impl CompletingRuntimeExecutor {
    fn new(state: AgentState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }
}

impl HostedRunnerHeadlessMessageExecutor for CompletingRuntimeExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        if let ToAgentMessage::ClientToolResult { call_id, .. } = message {
            self.state
                .lock()
                .expect("state")
                .pending_client_tools
                .retain(|pending| pending.call_id != call_id);
        }
        Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
            Vec::new(),
            "completed without outbound messages",
        ))
    }

    fn state(&self) -> Result<Option<AgentState>, HostedRunnerError> {
        Ok(Some(self.state.lock().expect("state").clone()))
    }
}

fn test_config(workspace_root: PathBuf) -> HostedRunnerConfig {
    HostedRunnerConfig {
        runner_session_id: "mrs_test".to_string(),
        workspace_root,
        bind_addr: "127.0.0.1:0".parse().expect("bind addr"),
        runtime_generation: 0,
        owner_instance_id: Some("owner_test".to_string()),
        snapshot_root: None,
        restore_manifest_path: None,
        workspace_id: Some("ws_test".to_string()),
        agent_run_id: Some("run_test".to_string()),
        maestro_session_id: Some("sess_test".to_string()),
        causal_receipt_id: None,
        attach_audience: None,
        auth_token: None,
        workload_identity: None,
        rendezvous: None,
    }
}

#[test]
fn hosted_runner_config_exports_the_native_runtime_boundary() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let boundary = config
        .runtime_boundary()
        .expect("hosted runner config should map to the runtime boundary");

    assert_eq!(boundary.runner_session_id, "mrs_test");
    assert_eq!(
        boundary.workspace_root,
        workspace.path().display().to_string()
    );
    assert_eq!(boundary.bind_address, "127.0.0.1:0");
    assert_eq!(boundary.runtime_generation, 0);
    assert_eq!(boundary.maestro_session_id.as_deref(), Some("sess_test"));
    assert_eq!(
        boundary.auth_mode,
        maestro_runtime::HostedRuntimeAuthMode::None
    );
    assert_eq!(boundary.topology, maestro_runtime::HOSTED_RUNTIME_TOPOLOGY);
}

#[test]
fn hosted_runtime_boundary_maps_static_bearer_without_serializing_secret_material() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf())
        .with_auth_token("sentinel-static-bearer-material");
    let boundary = config
        .runtime_boundary()
        .expect("static bearer config should map to the runtime boundary");
    let serialized = serde_json::to_string(&boundary).expect("serialize runtime boundary");

    assert_eq!(
        boundary.auth_mode,
        maestro_runtime::HostedRuntimeAuthMode::StaticBearer
    );
    assert!(!serialized.contains("sentinel-static-bearer-material"));
    assert!(!serialized.contains("auth_token"));
}

#[test]
fn hosted_runtime_boundary_maps_workload_identity_without_serializing_token_path() {
    let workspace = tempdir().expect("workspace");
    let projected_token_path = workspace.path().join("projected-token-sentinel");
    let projected_ca_path = workspace.path().join("projected-ca-sentinel.pem");
    fs::write(&projected_token_path, "sentinel-projected-token-material")
        .expect("write projected token fixture");
    fs::write(&projected_ca_path, "sentinel-projected-ca-material")
        .expect("write projected CA fixture");
    let mut config = test_config(workspace.path().to_path_buf());
    config.workload_identity = Some(HostedRunnerWorkloadIdentityConfig {
        kubernetes_token_file: projected_token_path.clone(),
        identity_tls_ca_file: projected_ca_path.clone(),
        identity_exchange_url: "https://identity.example.test/exchange"
            .parse()
            .expect("identity exchange URL"),
        organization_id: "org-sentinel".to_string(),
        workspace_id: "workspace-sentinel".to_string(),
        sandbox_id: uuid::Uuid::nil(),
        placement_generation: 7,
    });

    let boundary = config
        .runtime_boundary()
        .expect("workload identity config should map to the runtime boundary");
    let serialized = serde_json::to_string(&boundary).expect("serialize runtime boundary");

    assert_eq!(
        boundary.auth_mode,
        maestro_runtime::HostedRuntimeAuthMode::WorkloadIdentity
    );
    assert!(!serialized.contains("sentinel-projected-token-material"));
    assert!(!serialized.contains(projected_token_path.to_string_lossy().as_ref()));
    assert!(!serialized.contains(projected_ca_path.to_string_lossy().as_ref()));
    assert!(!serialized.contains("projected-token-sentinel"));
}

#[test]
fn hosted_runtime_boundary_rejects_bearer_and_workload_identity_together() {
    let workspace = tempdir().expect("workspace");
    let projected_token_path = workspace.path().join("projected-token-sentinel");
    let projected_ca_path = workspace.path().join("projected-ca-sentinel.pem");
    let mut config = test_config(workspace.path().to_path_buf())
        .with_auth_token("sentinel-static-bearer-material");
    config.workload_identity = Some(HostedRunnerWorkloadIdentityConfig {
        kubernetes_token_file: projected_token_path,
        identity_tls_ca_file: projected_ca_path,
        identity_exchange_url: "https://identity.example.test/exchange"
            .parse()
            .expect("identity exchange URL"),
        organization_id: "org-sentinel".to_string(),
        workspace_id: "workspace-sentinel".to_string(),
        sandbox_id: uuid::Uuid::nil(),
        placement_generation: 7,
    });

    let error = config
        .runtime_boundary()
        .expect_err("conflicting authentication modes must be rejected");
    assert!(error.to_string().contains("static bearer"));
    assert!(error.to_string().contains("workload identity"));
}

#[cfg(unix)]
#[test]
fn resolve_workspace_path_accepts_a_symlinked_workspace_root() {
    let workspace = tempdir().expect("workspace");
    let aliases = tempdir().expect("workspace aliases");
    let alias = aliases.path().join("workspace-link");
    std::os::unix::fs::symlink(workspace.path(), &alias).expect("workspace alias");

    let resolved = resolve_workspace_path(&alias, None, Some("."))
        .expect("a symlinked workspace root remains inside the workspace");
    assert_eq!(
        resolved,
        dunce::canonicalize(workspace.path()).expect("canonical workspace")
    );
}

fn base_hosted_runner_env(workspace_root: &Path) -> HashMap<String, String> {
    HashMap::from([
        (
            "MAESTRO_RUNNER_SESSION_ID".to_string(),
            "mrs_123".to_string(),
        ),
        (
            "MAESTRO_WORKSPACE_ROOT".to_string(),
            workspace_root.display().to_string(),
        ),
    ])
}

fn add_workload_identity_env(env: &mut HashMap<String, String>, token_file: &Path) {
    env.extend([
        (
            "MAESTRO_KUBERNETES_TOKEN_FILE".to_string(),
            token_file.display().to_string(),
        ),
        (
            "MAESTRO_IDENTITY_TLS_CA_FILE".to_string(),
            token_file
                .with_file_name("identity-ca.crt")
                .display()
                .to_string(),
        ),
        (
            "MAESTRO_IDENTITY_EXCHANGE_URL".to_string(),
            "https://identity.evalops.svc/internal/v1/kubernetes-workload-certificates/exchange"
                .to_string(),
        ),
        ("MAESTRO_ORGANIZATION_ID".to_string(), "org-123".to_string()),
        (
            "MAESTRO_WORKSPACE_ID".to_string(),
            "workspace-123".to_string(),
        ),
        (
            "MAESTRO_SANDBOX_ID".to_string(),
            "01234567-89ab-cdef-0123-456789abcdef".to_string(),
        ),
        ("MAESTRO_PLACEMENT_GENERATION".to_string(), "7".to_string()),
    ]);
}

fn add_rendezvous_env(env: &mut HashMap<String, String>) {
    env.extend([
        ("MAESTRO_RENDEZVOUS_MODE".into(), "outbound_shadow".into()),
        ("MAESTRO_RENDEZVOUS_ENDPOINT".into(), "127.0.0.1:9443".into()),
        ("MAESTRO_RENDEZVOUS_SERVER_NAME".into(), "runner-host.evalops.svc".into()),
        (
            "MAESTRO_RENDEZVOUS_IDENTITY_EXCHANGE_URL".into(),
            "https://identity.evalops.svc/internal/v1/kubernetes-workload-certificates/maestro-rendezvous-client/exchange".into(),
        ),
        (
            "MAESTRO_RENDEZVOUS_ACTIVATION_ID".into(),
            "11234567-89ab-cdef-0123-456789abcdef".into(),
        ),
        (
            "MAESTRO_RENDEZVOUS_NONCE".into(),
            "proof-0123456789abcdef0123456789abcdef".into(),
        ),
    ]);
}

#[test]
fn rendezvous_config_requires_explicit_outbound_prefer_gate_and_complete_bootstrap() {
    let workspace = tempdir().unwrap();
    let token_file = workspace.path().join("projected-token");
    let mut env = base_hosted_runner_env(workspace.path());
    add_workload_identity_env(&mut env, &token_file);
    add_rendezvous_env(&mut env);

    let shadow = HostedRunnerConfig::from_env_map(&env).expect("shadow rendezvous config");
    let rendezvous = shadow.rendezvous.expect("rendezvous config");
    assert_eq!(rendezvous.mode, RendezvousMode::OutboundShadow);
    assert_eq!(rendezvous.endpoint, "127.0.0.1:9443");
    assert_eq!(
        format!("{:?}", rendezvous.nonce),
        "RendezvousNonce(<redacted>)"
    );

    env.insert("MAESTRO_RENDEZVOUS_MODE".into(), "outbound".into());
    assert!(HostedRunnerConfig::from_env_map(&env).is_err());
    env.insert("MAESTRO_RENDEZVOUS_OUTBOUND_PREFER".into(), "true".into());
    let outbound = HostedRunnerConfig::from_env_map(&env).expect("explicit outbound prefer");
    assert_eq!(
        outbound.rendezvous.expect("rendezvous config").mode,
        RendezvousMode::Outbound
    );

    env.remove("MAESTRO_RENDEZVOUS_NONCE");
    assert!(HostedRunnerConfig::from_env_map(&env).is_err());
}

#[test]
fn shadow_rendezvous_emits_candidate_only_and_never_admits_commands() {
    use crate::hosted_runner::rendezvous_protocol::*;
    use crate::hosted_runner::rendezvous_runtime::{candidate_eligible_at, process_request};
    let activation_id = uuid::Uuid::new_v4();
    let mut lifecycle = RendezvousLifecycle::new(
        RendezvousMode::OutboundShadow,
        RendezvousIdentity {
            organization_id: "org".into(),
            workspace_id: "workspace".into(),
            sandbox_id: uuid::Uuid::new_v4(),
            placement_generation: 7,
            runner_session_id: "session".into(),
        },
        RendezvousNonce::parse("proof-0123456789abcdef0123456789abcdef").unwrap(),
    );
    lifecycle.set_runtime_ready(true);
    lifecycle.open(activation_id).unwrap();
    lifecycle
        .accept(&RendezvousAccepted {
            activation_id,
            outbound_commands_enabled: false,
            replay_from_sequence: 1,
        })
        .unwrap();
    let accepted_at = std::time::Instant::now();
    assert_eq!(
        candidate_eligible_at(RendezvousMode::OutboundShadow, accepted_at),
        Some(accepted_at)
    );
    let request = RendezvousRequest {
        activation_id,
        sequence: 1,
        idempotency_key: "command-1".into(),
        payload: serde_json::json!({"type":"prompt"}),
    };
    assert!(
        process_request(
            &mut lifecycle,
            &request,
            accepted_at,
            accepted_at,
            accepted_at
        )
        .is_err()
    );
}

#[test]
fn outbound_first_request_records_exact_first_command_and_frame_latency() {
    use crate::hosted_runner::rendezvous_protocol::*;
    use crate::hosted_runner::rendezvous_runtime::process_request;
    let activation_id = uuid::Uuid::new_v4();
    let mut lifecycle = RendezvousLifecycle::new(
        RendezvousMode::Outbound,
        RendezvousIdentity {
            organization_id: "org".into(),
            workspace_id: "workspace".into(),
            sandbox_id: uuid::Uuid::new_v4(),
            placement_generation: 7,
            runner_session_id: "session".into(),
        },
        RendezvousNonce::parse("proof-0123456789abcdef0123456789abcdef").unwrap(),
    );
    lifecycle.set_runtime_ready(true);
    lifecycle.open(activation_id).unwrap();
    lifecycle
        .accept(&RendezvousAccepted {
            activation_id,
            outbound_commands_enabled: true,
            replay_from_sequence: 1,
        })
        .unwrap();
    let activation_at = std::time::Instant::now();
    let command_at = activation_at + std::time::Duration::from_millis(13);
    let frame_at = command_at + std::time::Duration::from_millis(8);
    let request = RendezvousRequest {
        activation_id,
        sequence: 1,
        idempotency_key: "command-1".into(),
        payload: serde_json::json!({"type":"prompt"}),
    };
    let (ack, sample) = process_request(
        &mut lifecycle,
        &request,
        activation_at,
        command_at,
        frame_at,
    )
    .expect("first request");
    assert_eq!(ack.sequence, 1);
    assert_eq!(
        sample.activation_to_first_command,
        std::time::Duration::from_millis(13)
    );
    assert_eq!(
        sample.first_command_to_first_frame,
        std::time::Duration::from_millis(8)
    );
    assert_eq!(
        sample.activation_to_first_frame,
        std::time::Duration::from_millis(21)
    );
}

#[test]
fn inbound_command_path_waits_for_runtime_authority_grant() {
    let workspace = tempdir().unwrap();
    let mut config = test_config(workspace.path().to_path_buf());
    config.rendezvous = Some(config::HostedRunnerRendezvousConfig {
        mode: RendezvousMode::OutboundShadow,
        endpoint: "127.0.0.1:9443".into(),
        server_name: "runner-host.evalops.svc".into(),
        identity_exchange_url: "https://identity.evalops.svc/internal/v1/kubernetes-workload-certificates/maestro-rendezvous-client/exchange".parse().unwrap(),
        activation_id: uuid::Uuid::new_v4(),
        nonce: rendezvous_protocol::RendezvousNonce::parse(
            "proof-0123456789abcdef0123456789abcdef",
        )
        .unwrap(),
    });
    config.rendezvous.as_mut().unwrap().mode = RendezvousMode::Outbound;
    let shared = SharedRunner::new(config);
    assert!(shared.inbound_commands_enabled());

    shared.set_rendezvous_outbound_authority(true);
    assert!(!shared.inbound_commands_enabled());

    shared.set_rendezvous_outbound_authority(false);
    assert!(shared.inbound_commands_enabled());
}

async fn attach_thread_controller(
    client: &reqwest::Client,
    base_url: &str,
    connection_id: &str,
) -> (String, String) {
    let connection: serde_json::Value = client
        .post(format!("{base_url}/api/headless/connections"))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": connection_id,
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .error_for_status()
        .expect("connection status")
        .json()
        .await
        .expect("connection json");
    let connection_capability = connection["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();
    let subscription: serde_json::Value = client
        .post(format!(
            "{base_url}/api/headless/sessions/sess_test/subscribe"
        ))
        .json(&json!({
            "connectionId": connection_id,
            "connectionCapability": connection_capability,
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .error_for_status()
        .expect("subscription status")
        .json()
        .await
        .expect("subscription json");
    (
        connection_capability,
        subscription["subscription_id"]
            .as_str()
            .expect("subscription id")
            .to_string(),
    )
}

#[tokio::test]
async fn hosted_init_reaches_the_native_agent_executor() {
    let workspace = tempfile::tempdir().expect("workspace");
    let executor = Arc::new(ResponseRecordingExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let connection_id = "conn_init_forward";
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), connection_id).await;

    let init_ack: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", connection_id)
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .json(&json!({
            "type": "init",
            "system_prompt": "The private computer is already provisioned."
        }))
        .send()
        .await
        .expect("init response")
        .error_for_status()
        .expect("init status")
        .json()
        .await
        .expect("init acknowledgement");
    assert_eq!(init_ack["execution"], "runtime_handled");
    assert_eq!(init_ack["snapshot"]["last_init"]["type"], "init");

    {
        let messages = executor.messages.lock().expect("recorded init messages");
        assert!(matches!(
            messages.as_slice(),
            [ToAgentMessage::Init {
                system_prompt: Some(prompt),
                approval_mode: None,
                ..
            }] if prompt == "The private computer is already provisioned."
        ));
    }
    handle.shutdown().await;

    let restarted = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(ResponseRecordingExecutor::default()),
    )
    .await
    .expect("restart hosted runner");
    let snapshot: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            restarted.base_url()
        ))
        .send()
        .await
        .expect("restored state response")
        .error_for_status()
        .expect("restored state status")
        .json()
        .await
        .expect("restored state json");
    assert_eq!(
        snapshot["last_init"]["system_prompt"],
        "The private computer is already provisioned."
    );
    restarted.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_runner_forwards_resident_binding_to_native_workspace_activation() {
    const CHILD_ENV: &str = "MAESTRO_HOSTED_WORKSPACE_BINDING_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        assert_eq!(
            crate::headless_server::run_headless_server(None)
                .await
                .expect("native hosted child"),
            0
        );
        return;
    }

    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let wrapper = fixtures.path().join("native-child.sh");
    let child_input_log = fixtures.path().join("native-child-input.log");
    let child_output_log = fixtures.path().join("native-child-output.log");
    let current = std::env::current_exe().expect("current test binary");
    // Record each request before the child can answer it. GNU tee writes stdout
    // first, so a child receipt alone does not make its input log observable.
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n: > \"$MAESTRO_HOSTED_WORKSPACE_BINDING_INPUT_LOG\" || exit\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$MAESTRO_HOSTED_WORKSPACE_BINDING_INPUT_LOG\" || exit; printf '%s\\n' \"$line\" || exit; done | \"{}\" hosted_runner::tests::hosted_runner_forwards_resident_binding_to_native_workspace_activation --exact --nocapture --format terse | tee \"$MAESTRO_HOSTED_WORKSPACE_BINDING_OUTPUT_LOG\" | while IFS= read -r line; do case \"$line\" in '{{'*) printf '%s\\n' \"$line\" ;; esac; done\n",
            current.display()
        ),
    )
    .expect("native child wrapper");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("wrapper permissions");

    let mut supervisor_config = crate::headless::SupervisorConfig {
        auto_reconnect: false,
        ..Default::default()
    };
    supervisor_config.transport.cli_path = wrapper.display().to_string();
    supervisor_config.transport.cwd = Some(workspace.path().display().to_string());
    supervisor_config.transport.env = vec![
        (CHILD_ENV.to_string(), "1".to_string()),
        (
            "MAESTRO_HOSTED_WORKSPACE_BINDING_INPUT_LOG".to_string(),
            child_input_log.display().to_string(),
        ),
        (
            "MAESTRO_HOSTED_WORKSPACE_BINDING_OUTPUT_LOG".to_string(),
            child_output_log.display().to_string(),
        ),
        ("OPENAI_API_KEY".to_string(), "fixture-key".to_string()),
        (
            crate::credential_mode::ACCESS_TOKEN_ENV.to_string(),
            "fixture-evalops-access-token".to_string(),
        ),
        (
            "MAESTRO_IDENTITY_URL".to_string(),
            crate::credential_mode::test_identity_base_url().to_string(),
        ),
        (
            "MAESTRO_RUNNER_SESSION_ID".to_string(),
            "mrs_test".to_string(),
        ),
        ("MAESTRO_SESSION_ID".to_string(), "sess_test".to_string()),
        ("MAESTRO_EVALOPS_ORG_ID".to_string(), "org-1".to_string()),
        (
            "MAESTRO_EVALOPS_WORKSPACE_ID".to_string(),
            "ws_test".to_string(),
        ),
        (
            "MAESTRO_EVALOPS_THREAD_ID".to_string(),
            "sess_test".to_string(),
        ),
    ];
    let mut supervisor = AgentSupervisor::new(supervisor_config);
    supervisor.connect().await.expect("connect native child");
    tokio::time::timeout(Duration::from_secs(20), async {
        while supervisor.state().model.is_none() {
            supervisor.recv().await.expect("native child startup event");
        }
    })
    .await
    .expect("native child ready");
    let supervisor = Arc::new(Mutex::new(supervisor));
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let mut config = test_config(workspace.path().to_path_buf());
    config.runtime_generation = 7;
    let handle = start_hosted_runner_with_message_executor(config, executor.clone())
        .await
        .expect("hosted runner");
    handle
        .shared
        .stop_event_pump()
        .await
        .expect("stop background event pump for deterministic fixture");
    let client = reqwest::Client::new();
    let connection_id = "conn_workspace_binding";
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), connection_id).await;
    let message_url = format!(
        "{}/api/headless/sessions/sess_test/messages",
        handle.base_url()
    );
    let send = |body: serde_json::Value| {
        client
            .post(&message_url)
            .header("x-maestro-headless-connection-id", connection_id)
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .json(&body)
            .send()
    };
    let hello = json!({
        "type": "hello",
        "protocol_version": HEADLESS_PROTOCOL_VERSION,
        "controller_binding_version": crate::headless::controller_binding::CONTROLLER_BINDING_VERSION,
        "controller_context": {
            "schema_version": crate::headless::controller_binding::CONTROLLER_CONTEXT_SCHEMA_VERSION,
            "controller_id": "evalops.platform",
            "organization_id": "org-1",
            "workspace_id": "ws_test",
            "thread_id": "sess_test",
            "lifetime_profile": "resident",
            "runtime_generation": 7
        },
        "capability_manifest": {
            "schema_version": "evalops.maestro.capability-manifest.v1",
            "engine_kind": "maestro",
            "protocol_version": HEADLESS_PROTOCOL_VERSION,
            "tool_protocol_version": "evalops.maestro.tool-bridge.v1",
            "supported_tools": [],
            "native_tool_calls": true,
            "approvals": true,
            "continuation": false,
            "cancellation": true,
            "idempotent_replay": true,
            "streaming": true
        }
    });
    let hello_ack: serde_json::Value = send(hello)
        .await
        .expect("hosted hello response")
        .error_for_status()
        .expect("hosted hello status")
        .json()
        .await
        .expect("hosted hello acknowledgement");

    let body = "Always keep the hosted receipt causal.";
    let mut request = crate::headless::workspace_capabilities::ApplyWorkspaceCapabilitySet {
        organization_id: "org-1".to_string(),
        workspace_id: "ws_test".to_string(),
        runner_session_id: "mrs_test".to_string(),
        runtime_generation: 7,
        activation_generation: 1,
        workspace_snapshot_digest:
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        workspace_skill_set_digest:
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        capability_set_digest: String::new(),
        workspace_instructions: vec!["Use hosted workspace guidance.".to_string()],
        admitted_catalog: vec![
            crate::headless::workspace_capabilities::WorkspacePromptCapability {
                qualified_id: "skill.hosted-review".to_string(),
                name: "hosted-review".to_string(),
                scope: "workspace".to_string(),
                revision_digest:
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                        .to_string(),
                body_digest: format!("sha256:{:x}", Sha256::digest(body.as_bytes())),
                trigger_patterns: vec!["review".to_string()],
                user_invocable: true,
                pinned_prompt_only: true,
                title: "Hosted review".to_string(),
                description: "Review hosted workspace changes.".to_string(),
                instructions: vec!["Apply the hosted checklist.".to_string()],
                body: body.to_string(),
                entry_digest: String::new(),
            },
        ],
        admission_receipt_id: "hosted-admission-1".to_string(),
    };
    crate::headless::workspace_capabilities::recompute_request_digests(&mut request)
        .expect("hosted request digests");
    let activation_ack: serde_json::Value =
        send(json!({"type":"apply_workspace_capability_set","request":request}))
            .await
            .expect("hosted activation response")
            .error_for_status()
            .expect("hosted activation status")
            .json()
            .await
            .expect("hosted activation acknowledgement");
    assert_eq!(hello_ack["execution"], "runtime_handled");
    assert_eq!(activation_ack["execution"], "runtime_handled");
    tokio::time::timeout(Duration::from_secs(20), async {
        while !std::fs::read_to_string(&child_output_log)
            .unwrap_or_default()
            .contains(r#""type":"workspace_capability_set_applied""#)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native child activation receipt barrier");
    let child_input = std::fs::read_to_string(&child_input_log).expect("native child input");
    let binding_offset = child_input
        .find(r#""controller_binding_version""#)
        .expect("native child received resident binding");
    let activation_offset = child_input
        .find(r#""type":"apply_workspace_capability_set""#)
        .expect("native child received activation");
    assert!(binding_offset < activation_offset);
    let receipt = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            assert_eq!(pump_tick(&handle.shared), PumpTick::Continue);
            let receipt = handle
                .shared
                .state
                .lock()
                .expect("runner state")
                .envelopes
                .iter()
                .find_map(|envelope| match envelope {
                    StreamEnvelope::Message { message, .. } => match message.as_ref() {
                        FromAgentMessage::WorkspaceCapabilitySetApplied { receipt } => {
                            Some(receipt.clone())
                        }
                        _ => None,
                    },
                    _ => None,
                });
            if let Some(receipt) = receipt {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native activation receipt through hosted runner");
    assert_eq!(receipt.organization_id, "org-1");
    assert_eq!(receipt.workspace_id, "ws_test");
    assert_eq!(receipt.runner_session_id, "mrs_test");
    assert_eq!(receipt.runtime_generation, 7);
    assert_eq!(receipt.activation_generation, 1);
    assert!(!receipt.controller_binding_sha256.is_empty());

    handle.shutdown().await;
    drop(executor);
    let mutex = Arc::try_unwrap(supervisor).unwrap_or_else(|supervisor| {
        panic!(
            "release supervisor owners: {} remain",
            Arc::strong_count(&supervisor)
        )
    });
    mutex
        .into_inner()
        .unwrap_or_else(|error| error.into_inner())
        .shutdown_and_wait()
        .await;
}

#[tokio::test]
async fn failed_init_persistence_restores_the_previous_runtime_snapshot() {
    let workspace = tempfile::tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(ResponseRecordingExecutor::default()),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let connection_id = "conn_init_persist_failure";
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), connection_id).await;
    let previous_status = handle
        .shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last_status
        .clone();
    handle.shared.fail_next_thread_persistences(1);

    let response = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", connection_id)
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .json(&json!({
            "type": "init",
            "system_prompt": "This init must not survive a failed journal write."
        }))
        .send()
        .await
        .expect("init response");
    assert!(response.status().is_server_error());

    let snapshot: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .error_for_status()
        .expect("state status")
        .json()
        .await
        .expect("state json");
    assert!(snapshot["last_init"].is_null());
    {
        let state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.last_status, previous_status);
    }
    handle.shutdown().await;
}

fn response_headers(
    connection_id: &str,
    subscription_id: &str,
    capability: &str,
    idempotency_key: &str,
) -> HashMap<String, String> {
    [
        (
            "x-maestro-headless-connection-id".to_string(),
            connection_id.to_string(),
        ),
        (
            "x-maestro-headless-subscriber-id".to_string(),
            subscription_id.to_string(),
        ),
        (
            "x-maestro-headless-connection-capability".to_string(),
            capability.to_string(),
        ),
        (
            "x-maestro-idempotency-key".to_string(),
            idempotency_key.to_string(),
        ),
    ]
    .into_iter()
    .collect()
}

#[cfg(unix)]
fn create_response_consumer_script(
    directory: &Path,
    name: &str,
    log_path: &Path,
    acknowledge: bool,
    acknowledgement_delay: Option<&str>,
) -> PathBuf {
    let action = if acknowledge {
        if let Some(delay) = acknowledgement_delay {
            format!(
                "sleep {delay}; printf '{{\"type\":\"response_accepted\",\"request_id\":\"restart-call\"}}\\n'"
            )
        } else {
            "printf '{\"type\":\"response_accepted\",\"request_id\":\"restart-call\"}\\n'"
                .to_string()
        }
    } else {
        "sleep 1.1; exit 0".to_string()
    };
    create_response_consumer_script_with_action(directory, name, log_path, &action)
}

#[cfg(unix)]
fn create_response_consumer_script_with_action(
    directory: &Path,
    name: &str,
    log_path: &Path,
    action: &str,
) -> PathBuf {
    let script_path = directory.join(name);
    std::fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
printf '{{"type":"ready","model":"test","provider":"test"}}\n'
while IFS= read -r line; do
  case "$line" in
    *'"type":"tool_response"'*)
      printf '%s\n' "$line" >> "{}"
      {action}
      ;;
  esac
done
"#,
            log_path.display()
        ),
    )
    .expect("response consumer script");
    let mut permissions = std::fs::metadata(&script_path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).expect("script permissions");
    script_path
}

#[cfg(unix)]
fn create_reject_then_accept_script(
    directory: &Path,
    log_path: &Path,
    rejection_delay: Option<&str>,
) -> PathBuf {
    let script_path = directory.join("reject-then-accept.sh");
    let count_path = directory.join("reject-count");
    std::fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
printf '{{"type":"ready","model":"test","provider":"test"}}\n'
while IFS= read -r line; do
  case "$line" in
    *'"type":"tool_response"'*)
      printf '%s\n' "$line" >> "{}"
      count=0
      if [ -f "{}" ]; then count=$(cat "{}"); fi
      count=$((count + 1))
      printf '%s' "$count" > "{}"
      if [ "$count" -eq 1 ]; then
        {}
        printf '{{"type":"error","request_id":"retry-call","message":"tool call retry-call is not awaiting a decision","fatal":false,"error_type":"protocol"}}\n'
      else
        printf '{{"type":"response_accepted","request_id":"retry-call"}}\n'
      fi
      ;;
  esac
done
"#,
            log_path.display(),
            count_path.display(),
            count_path.display(),
            count_path.display(),
            rejection_delay
                .map(|delay| format!("sleep {delay}"))
                .unwrap_or_default(),
        ),
    )
    .expect("reject-then-accept script");
    let mut permissions = std::fs::metadata(&script_path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).expect("script permissions");
    script_path
}

#[cfg(unix)]
fn create_delayed_identity_ack_script(
    directory: &Path,
    name: &str,
    log_path: &Path,
    message_type: &str,
    request_id: &str,
) -> PathBuf {
    let script_path = directory.join(name);
    std::fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
printf '{{"type":"ready","model":"test","provider":"test"}}\n'
while IFS= read -r line; do
  case "$line" in
    *'"type":"{message_type}"'*)
      printf '%s\n' "$line" >> "{}"
      sleep 0.65
      printf '{{"type":"response_accepted","request_id":"{request_id}"}}\n'
      ;;
  esac
done
"#,
            log_path.display(),
        ),
    )
    .expect("delayed identity acknowledgement script");
    let mut permissions = std::fs::metadata(&script_path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).expect("script permissions");
    script_path
}

#[cfg(unix)]
async fn connected_supervisor_for_script(script_path: &Path) -> Arc<Mutex<AgentSupervisor>> {
    let mut config = crate::headless::SupervisorConfig::default();
    config.transport.cli_path = script_path.display().to_string();
    config.auto_reconnect = false;
    let mut supervisor = AgentSupervisor::new(config);
    supervisor.connect().await.expect("connect scripted child");
    let _ = supervisor.recv().await.expect("connected");
    let _ = supervisor.recv().await.expect("healthy");
    let _ = supervisor.recv().await.expect("ready");
    Arc::new(Mutex::new(supervisor))
}

#[tokio::test]
async fn route_rejects_connection_prefix_without_separator() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let request = HttpRequest {
        method: "POST".to_string(),
        path: "/api/headless/connections-extra".to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
        body: b"{}".to_vec(),
    };

    let error = match route_request(request, shared, "127.0.0.1:4567".parse().unwrap()).await {
        Ok(_) => panic!("unexpected route match"),
        Err(error) => error,
    };

    assert_eq!(error.code, HostedRunnerErrorCode::NotFound);
}

#[test]
fn supervisor_executor_reports_runtime_not_ready_until_connected() {
    let workspace = tempdir().expect("workspace");
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(
        crate::headless::SupervisorConfig::default(),
    )));
    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(supervisor);
    let context = HostedRunnerHeadlessMessageContext {
        session_id: "sess_test".to_string(),
        connection_id: "conn_exec".to_string(),
        subscription_id: Some("sub_exec".to_string()),
        role: ConnectionRole::Controller,
        controller_connection_id: Some("conn_exec".to_string()),
        client_protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: None,
        capabilities: None,
        opt_out_notifications: None,
        lease_expires_at: Utc::now().to_rfc3339(),
        workspace_root: workspace.path().to_path_buf(),
        response_idempotency_key: None,
    };

    let error = executor
        .execute(
            &context,
            ToAgentMessage::Prompt {
                content: "hello".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            },
        )
        .expect_err("supervisor should not be connected");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeNotReady);
}

#[test]
fn supervisor_executor_negotiates_hello_without_mutating_shared_runtime() {
    let workspace = tempdir().expect("workspace");
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(
        crate::headless::SupervisorConfig::default(),
    )));
    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(supervisor);
    let context = HostedRunnerHeadlessMessageContext {
        session_id: "sess_test".to_string(),
        connection_id: "conn_block".to_string(),
        subscription_id: Some("sub_block".to_string()),
        role: ConnectionRole::Viewer,
        controller_connection_id: Some("conn_controller".to_string()),
        client_protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: None,
        capabilities: Some(ClientCapabilities {
            transcript_grade: Some(crate::transcript::TranscriptGrade::Block),
            ..ClientCapabilities::default()
        }),
        opt_out_notifications: None,
        lease_expires_at: Utc::now().to_rfc3339(),
        workspace_root: workspace.path().to_path_buf(),
        response_idempotency_key: None,
    };

    let result = executor
        .execute(
            &context,
            ToAgentMessage::Hello {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                client_info: None,
                capabilities: context.capabilities.clone(),
                role: Some(ConnectionRole::Viewer),
                opt_out_notifications: None,
                controller_binding: None,
            },
        )
        .expect("hosted hello");

    assert!(matches!(
        result.messages.as_slice(),
        [FromAgentMessage::HelloOk {
            connection_id: Some(connection_id),
            capabilities: Some(ClientCapabilities {
                transcript_grade: Some(crate::transcript::TranscriptGrade::Block),
                ..
            }),
            server_capabilities: Some(server_capabilities),
            ..
        }] if connection_id == "conn_block"
            && server_capabilities
                .native_tools
                .iter()
                .any(|tool| tool.name == "bash" && tool.version.as_deref() == Some("current"))
    ));
}

#[test]
fn hosted_server_capabilities_follow_attached_agent_state() {
    let attached = ServerCapabilities {
        native_tools: vec![NativeToolCapability {
            name: "attached-only-tool".to_string(),
            requires_approval: false,
            version: Some("agent-build".to_string()),
        }],
        ..ServerCapabilities::default()
    };
    let executor = StatefulRuntimeExecutor::new(AgentState {
        server_capabilities: Some(attached.clone()),
        ..AgentState::default()
    });

    assert_eq!(server_capabilities_for_executor(&executor), Some(attached));
}

#[test]
fn hosted_server_capabilities_fail_closed_without_executor_state() {
    let executor = TransportOnlyHostedRunnerMessageExecutor;

    let capabilities = server_capabilities_for_executor(&executor)
        .expect("transport-only executor preserves baseline server capabilities");
    assert!(!capabilities.server_requests.is_empty());
    assert!(!capabilities.native_tools.is_empty());
    assert!(capabilities.governed_tool_grant_algorithms.is_empty());
    assert!(governed_grant_algorithms_for_executor(&executor).is_empty());

    let workspace = tempdir().expect("transport-only workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshot_capabilities = shared
        .public_snapshot(&state)
        .state
        .server_capabilities
        .expect("snapshot preserves baseline server capabilities");
    drop(state);
    assert!(!snapshot_capabilities.server_requests.is_empty());
    assert!(!snapshot_capabilities.native_tools.is_empty());
    assert!(
        snapshot_capabilities
            .governed_tool_grant_algorithms
            .is_empty()
    );

    let mut formerly_capable = native_server_capabilities();
    formerly_capable.governed_tool_grant_algorithms = vec!["ed25519".to_string()];
    let restored_workspace = tempdir().expect("restored child workspace");
    let restored_shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(restored_workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(AgentState {
            server_capabilities: Some(formerly_capable),
            ..AgentState::default()
        })),
        None,
    );
    let restored_snapshot = {
        let state = restored_shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        restored_shared.public_snapshot(&state)
    };
    let restored_snapshot_capabilities = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.restored_snapshot = Some(restored_snapshot);
        shared
            .public_snapshot(&state)
            .state
            .server_capabilities
            .expect("restored snapshot preserves baseline server capabilities")
    };
    assert!(!restored_snapshot_capabilities.server_requests.is_empty());
    assert!(!restored_snapshot_capabilities.native_tools.is_empty());
    assert!(
        restored_snapshot_capabilities
            .governed_tool_grant_algorithms
            .is_empty()
    );
}

#[test]
fn supervisor_hello_capabilities_prefer_attached_agent_state() {
    let attached = ServerCapabilities {
        native_tools: vec![NativeToolCapability {
            name: "child-build-tool".to_string(),
            requires_approval: true,
            version: Some("child-build".to_string()),
        }],
        ..ServerCapabilities::default()
    };
    let mut supervisor = AgentSupervisor::new(crate::headless::SupervisorConfig::default());
    supervisor.restore_session_replay(crate::headless::SessionReplay {
        state: AgentState {
            server_capabilities: Some(attached.clone()),
            ..AgentState::default()
        },
        last_init: None,
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    });
    let executor =
        AgentSupervisorHostedRunnerMessageExecutor::new(Arc::new(Mutex::new(supervisor)));

    assert_eq!(executor.negotiated_server_capabilities(), Some(attached));
}

#[test]
fn supervisor_hello_fallback_withholds_governed_grant_algorithms() {
    const ENV: &str = "MAESTRO_PLATFORM_TOOL_GRANT_ED25519_PUBLIC_KEYS";
    let _guard = crate::headless_server::GOVERNED_GRANT_ENV_LOCK
        .lock()
        .expect("governed grant environment lock");
    let previous = std::env::var_os(ENV);
    // SAFETY: this test holds the shared lock for the dedicated environment
    // key and restores its prior value before releasing it.
    unsafe {
        std::env::set_var(
            ENV,
            r#"{"active":{"algorithm":"ed25519","public_key":"11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=","state":"active"}}"#,
        );
    }

    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(
        crate::headless::SupervisorConfig::default(),
    )));
    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(supervisor);
    let capabilities = executor
        .negotiated_server_capabilities()
        .expect("pre-transport fallback capabilities");
    assert!(capabilities.governed_tool_grant_algorithms.is_empty());

    // SAFETY: restore the value while the same dedicated environment lock is
    // still held.
    unsafe {
        match previous {
            Some(value) => std::env::set_var(ENV, value),
            None => std::env::remove_var(ENV),
        }
    }
}

fn stream_message(cursor: u64, message: FromAgentMessage) -> StreamEnvelope {
    StreamEnvelope::Message {
        cursor,
        message: Box::new(message),
    }
}

#[test]
fn block_stream_filter_coalesces_text_and_omits_delta_events() {
    let mut filter = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Block, 0);
    let mut output = Vec::new();
    for envelope in [
        stream_message(
            0,
            FromAgentMessage::ResponseStart {
                response_id: "response".to_string(),
            },
        ),
        stream_message(
            1,
            FromAgentMessage::ResponseChunk {
                response_id: "response".to_string(),
                content: "hel".to_string(),
                is_thinking: false,
            },
        ),
        stream_message(
            2,
            FromAgentMessage::ResponseChunk {
                response_id: "response".to_string(),
                content: "hidden thought".to_string(),
                is_thinking: true,
            },
        ),
        stream_message(
            3,
            FromAgentMessage::ResponseChunk {
                response_id: "response".to_string(),
                content: "lo".to_string(),
                is_thinking: false,
            },
        ),
        stream_message(
            4,
            FromAgentMessage::ToolOutput {
                call_id: "call".to_string(),
                content: "delta".to_string(),
            },
        ),
        stream_message(
            5,
            FromAgentMessage::ToolStart {
                call_id: "call".to_string(),
            },
        ),
        stream_message(
            7,
            FromAgentMessage::ResponseEnd {
                response_id: "response".to_string(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
        ),
    ] {
        output.extend(filter.apply(envelope));
    }

    let output = serde_json::to_value(output).expect("serialize filtered events");
    assert_eq!(output.as_array().expect("events").len(), 4);
    assert_eq!(output[0]["message"]["type"], "response_start");
    assert_eq!(output[1]["cursor"], 5);
    assert_eq!(output[1]["message"]["type"], "tool_start");
    assert_eq!(output[2]["cursor"], 6);
    assert_eq!(output[2]["message"]["type"], "response_chunk");
    assert_eq!(output[2]["message"]["content"], "hello");
    assert_eq!(output[3]["message"]["type"], "response_end");
    assert_eq!(output[3]["cursor"], 7);
    assert!(!output.to_string().contains("hidden thought"));
    assert!(!output.to_string().contains("delta"));
}

#[test]
fn coarse_stream_resume_reconstructs_full_response_with_monotonic_cursor() {
    let mut filter = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Turn, 3);
    let mut output = Vec::new();
    for envelope in [
        stream_message(
            1,
            FromAgentMessage::ResponseStart {
                response_id: "response".to_string(),
            },
        ),
        stream_message(
            2,
            FromAgentMessage::ResponseChunk {
                response_id: "response".to_string(),
                content: "before".to_string(),
                is_thinking: false,
            },
        ),
        stream_message(
            4,
            FromAgentMessage::ResponseChunk {
                response_id: "response".to_string(),
                content: " after".to_string(),
                is_thinking: false,
            },
        ),
        stream_message(
            5,
            FromAgentMessage::ResponseEnd {
                response_id: "response".to_string(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
        ),
    ] {
        output.extend(filter.apply(envelope));
    }

    let output = serde_json::to_value(output).expect("serialize filtered events");
    assert_eq!(output.as_array().expect("events").len(), 2);
    assert_eq!(output[0]["cursor"], 4);
    assert_eq!(output[0]["message"]["content"], "before after");
    assert_eq!(output[1]["cursor"], 5);
    assert_eq!(output[1]["message"]["type"], "response_end");
}

#[test]
fn response_completion_reserves_a_cursor_for_the_coarse_aggregate() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shared.publish_message(
        &mut state,
        FromAgentMessage::ResponseStart {
            response_id: "response".into(),
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ResponseChunk {
            response_id: "response".into(),
            content: "complete".into(),
            is_thinking: false,
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ResponseEnd {
            response_id: "response".into(),
            usage: None,
            tools_summary: None,
            duration_ms: None,
            ttft_ms: None,
        },
    );

    assert_eq!(state.cursor, 4);
    assert!(matches!(
        state.envelopes.back(),
        Some(StreamEnvelope::Message { cursor: 4, message })
            if matches!(message.as_ref(), FromAgentMessage::ResponseEnd { .. })
    ));
}

#[test]
fn coarse_stream_resume_resets_when_retained_response_start_was_evicted() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cursor = 11;
        state.envelopes.push_back(stream_message(
            10,
            FromAgentMessage::ResponseChunk {
                response_id: "response".into(),
                content: "retained suffix".into(),
                is_thinking: false,
            },
        ));
        state.envelopes.push_back(stream_message(
            11,
            FromAgentMessage::ResponseEnd {
                response_id: "response".into(),
                usage: None,
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            },
        ));
    }

    let (replay, _rx) = shared.subscribe_coarse_from(9);
    assert!(matches!(
        replay.as_slice(),
        [StreamEnvelope::Reset { reason, .. }] if reason == "coarse_replay_incomplete"
    ));
}

#[test]
fn coarse_stream_resume_resets_when_active_response_is_entirely_evicted() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cursor = 10;
        state.active_response_ids.insert("response".into());
        state.envelopes.push_back(stream_message(
            10,
            FromAgentMessage::Status {
                message: "still working".into(),
            },
        ));
    }

    let (replay, _rx) = shared.subscribe_coarse_from(9);
    assert!(matches!(
        replay.as_slice(),
        [StreamEnvelope::Reset { reason, .. }] if reason == "coarse_replay_incomplete"
    ));
}

#[test]
fn coarse_stream_resume_suppresses_stale_snapshots_but_preserves_resets() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let (stale, fresh) = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cursor = 2;
        let stale = shared.snapshot(&state);
        state.cursor = 4;
        let fresh = shared.snapshot(&state);
        (stale, fresh)
    };
    let mut filter = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Turn, 3);

    assert!(
        filter
            .apply(StreamEnvelope::Snapshot {
                snapshot: stale.clone(),
            })
            .is_empty()
    );
    assert_eq!(
        filter
            .apply(StreamEnvelope::Reset {
                reason: "replay_gap".to_string(),
                snapshot: stale,
            })
            .len(),
        1
    );
    assert_eq!(
        filter
            .apply(StreamEnvelope::Snapshot { snapshot: fresh })
            .len(),
        1
    );
}

#[test]
fn coarse_stream_reset_discards_partial_aggregation() {
    let mut filter = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Block, 0);
    assert_eq!(
        filter
            .apply(stream_message(
                1,
                FromAgentMessage::ResponseStart {
                    response_id: "response".into(),
                },
            ))
            .len(),
        1
    );
    assert!(
        filter
            .apply(stream_message(
                2,
                FromAgentMessage::ResponseChunk {
                    response_id: "response".into(),
                    content: "partial".into(),
                    is_thinking: false,
                },
            ))
            .is_empty()
    );
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let snapshot = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.snapshot(&state)
    };
    assert_eq!(
        filter
            .apply(StreamEnvelope::Reset {
                reason: "broadcast_lag".into(),
                snapshot,
            })
            .len(),
        1
    );
    let completion = filter.apply(stream_message(
        3,
        FromAgentMessage::ResponseEnd {
            response_id: "response".into(),
            usage: None,
            tools_summary: None,
            duration_ms: None,
            ttft_ms: None,
        },
    ));
    assert_eq!(completion.len(), 1);
    assert!(matches!(
        completion[0],
        StreamEnvelope::Message {
            message: ref value,
            ..
        } if matches!(**value, FromAgentMessage::ResponseEnd { .. })
    ));
}

#[test]
fn coarse_stream_new_response_retires_orphaned_response() {
    let mut filter = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Block, 0);
    let _ = filter.apply(stream_message(
        1,
        FromAgentMessage::ResponseStart {
            response_id: "orphan".into(),
        },
    ));
    let _ = filter.apply(stream_message(
        2,
        FromAgentMessage::ResponseChunk {
            response_id: "orphan".into(),
            content: "partial".into(),
            is_thinking: false,
        },
    ));
    let restarted = filter.apply(stream_message(
        3,
        FromAgentMessage::ResponseStart {
            response_id: "retry".into(),
        },
    ));
    assert_eq!(restarted.len(), 1);
    let completed = filter.apply(stream_message(
        4,
        FromAgentMessage::ResponseEnd {
            response_id: "retry".into(),
            usage: None,
            tools_summary: None,
            duration_ms: None,
            ttft_ms: None,
        },
    ));
    assert_eq!(completed.len(), 1);
    assert!(
        !serde_json::to_string(&completed)
            .unwrap()
            .contains("partial")
    );
}

#[test]
fn hosted_replay_journal_stores_redacted_tool_events() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let (_, mut live_events) = shared.reset_and_subscribe("credential-redaction-test");
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shared.publish_message(
        &mut state,
        FromAgentMessage::ToolCall {
            call_id: "call".into(),
            tool_execution_id: None,
            tool: "http".into(),
            args: serde_json::json!({
                "headers":{"authorization":"Bearer secret"},
                "command":"curl -H 'Authorization: Bearer abc/inline+secret~==' example.test",
                "basic":"curl -H 'Authorization: Basic dG9vbDp0b29sLXNlY3JldA==' example.test",
                "negotiate":"curl -H 'Authorization: Negotiate TlRMTVNTUAAB' example.test",
                "digest":"curl -H 'Authorization: Digest username=\"alice\", nonce=\"nonce-secret\", response=\"response-secret\"' example.test",
                "sigv4":"curl -H 'Authorization: AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260729/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=sigv4-signature-secret' example.test",
                "quoted_password":"curl --data 'password=abc;remaining-secret' example.test",
                "embedded_quoted_password":"curl --data 'user=x&password=abc;embedded-tail-secret' example.test",
                "escaped_quote_password":"curl --data password=\"abc\\\"escaped-remaining-secret\" example.test",
                "quoted_to_unquoted_password":"curl --data password='abc'raw-password-secret example.test",
                "unquoted_to_quoted_password":"curl --data password=abc'remaining-secret' example.test",
                "repeated_password_segments":"curl --data password=a'b'c\"d\"e example.test",
                "source":"password: String\ntoken: Option<String>",
                "vaulted":"curl -H 'Authorization: Bearer {{CRED:token:abcdef012345}}' example.test",
                "vaulted_adjacent":"Authorization: Bearer {{CRED:token:abcdef012345}}raw-adjacent-secret",
                "malformed_closed":"{{CRED:sk-ant-abcdefghijklmnopqrstuvwxyz123456}}",
                "malformed_delimited":"{{CRED:password:abc,delimiter-tail-secret}}",
                "malformed_whitespace":
                    "{{CRED:password:abc whitespace-tail-secret}} preserved-tail"
            }),
            requires_approval: false,
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ToolCall {
            call_id: "shell-call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({
                "command":"curl --data password=abc,comma-tail-secret; password=(array-secret other-secret); password=$(printf dynamic-secret); password=`printf legacy-secret`; printf '%s' 'foo\\'; password=abc; echo ok"
            }),
            requires_approval: false,
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ClientToolRequest {
            call_id: "client-call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({
                "command":"curl -H 'Authorization: Bearer client/inline+secret~==' example.test",
                "basic":"curl -H 'Authorization: Basic Y2xpZW50OmNsaWVudC1zZWNyZXQ=' example.test"
            }),
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ServerRequest {
            request_id: "approval".into(),
            request_type: ServerRequestType::Approval,
            call_id: "server-call".into(),
            tool_execution_id: None,
            tool: "bash".into(),
            args: serde_json::json!({
                "command":"curl -H 'Authorization: Bearer server/inline+secret~==' example.test",
                "basic":"curl -H 'Authorization: Basic c2VydmVyOnNlcnZlci1zZWNyZXQ=' example.test"
            }),
            reason: "approval required".into(),
            started_at_ms: None,
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: "client_secret=secret".into(),
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content: "INFO\n{\"api_key\":\"prefixed-secret\",\"result\":\"ok\"}".into(),
        },
    );
    shared.publish_message(
        &mut state,
        FromAgentMessage::ToolOutput {
            call_id: "call".into(),
            content:
                "-----BEGIN OPENSSH PRIVATE KEY-----\nprivate-key-material\n-----END OPENSSH PRIVATE KEY-----"
                    .into(),
        },
    );

    let stored = serde_json::to_string(&state.envelopes).expect("serialize replay journal");
    assert!(!stored.contains("Bearer secret"));
    assert!(!stored.contains("inline+secret"));
    assert!(!stored.contains("client/inline+secret"));
    assert!(!stored.contains("server/inline+secret"));
    assert!(!stored.contains("dG9vbDp0b29sLXNlY3JldA"));
    assert!(!stored.contains("TlRMTVNTUAAB"));
    assert!(!stored.contains("nonce-secret"));
    assert!(!stored.contains("response-secret"));
    assert!(!stored.contains("20260729/us-east-1/s3/aws4_request"));
    assert!(!stored.contains("sigv4-signature-secret"));
    assert!(!stored.contains("Y2xpZW50OmNsaWVudC1zZWNyZXQ"));
    assert!(!stored.contains("c2VydmVyOnNlcnZlci1zZWNyZXQ"));
    assert!(!stored.contains("remaining-secret"));
    assert!(!stored.contains("escaped-remaining-secret"));
    assert!(!stored.contains("comma-tail-secret"));
    assert!(!stored.contains("array-secret"));
    assert!(!stored.contains("other-secret"));
    assert!(!stored.contains("dynamic-secret"));
    assert!(!stored.contains("legacy-secret"));
    assert!(stored.contains("echo ok"));
    assert!(!stored.contains("embedded-tail-secret"));
    assert!(!stored.contains("delimiter-tail-secret"));
    assert!(!stored.contains("whitespace-tail-secret"));
    assert!(!stored.contains("{{CRED:password:abc "));
    assert!(stored.contains("preserved-tail"));
    assert!(!stored.contains("raw-password-secret"));
    assert!(!stored.contains("a'b'c"));
    assert!(!stored.contains("\"d\"e"));
    let live = (0..4)
        .map(|_| live_events.try_recv().expect("redacted live event"))
        .collect::<Vec<_>>();
    let live = serde_json::to_string(&live).expect("serialize live events");
    assert!(!live.contains("inline+secret"));
    assert!(!live.contains("client/inline+secret"));
    assert!(!live.contains("server/inline+secret"));
    assert!(!live.contains("dG9vbDp0b29sLXNlY3JldA"));
    assert!(!live.contains("TlRMTVNTUAAB"));
    assert!(!live.contains("nonce-secret"));
    assert!(!live.contains("response-secret"));
    assert!(!live.contains("20260729/us-east-1/s3/aws4_request"));
    assert!(!live.contains("sigv4-signature-secret"));
    assert!(!live.contains("Y2xpZW50OmNsaWVudC1zZWNyZXQ"));
    assert!(!live.contains("c2VydmVyOnNlcnZlci1zZWNyZXQ"));
    assert!(!live.contains("remaining-secret"));
    assert!(!live.contains("escaped-remaining-secret"));
    assert!(!live.contains("comma-tail-secret"));
    assert!(!live.contains("array-secret"));
    assert!(!live.contains("other-secret"));
    assert!(!live.contains("dynamic-secret"));
    assert!(!live.contains("legacy-secret"));
    assert!(live.contains("echo ok"));
    assert!(!live.contains("embedded-tail-secret"));
    assert!(!live.contains("delimiter-tail-secret"));
    assert!(!live.contains("whitespace-tail-secret"));
    assert!(!live.contains("{{CRED:password:abc "));
    assert!(live.contains("preserved-tail"));
    assert!(!live.contains("raw-password-secret"));
    assert!(!live.contains("a'b'c"));
    assert!(!live.contains("\"d\"e"));
    assert!(live.contains("[REDACTED:token:portable-export]"));
    assert!(live.contains("[REDACTED:password:portable-export]"));
    assert!(!stored.contains("client_secret"));
    assert!(!stored.contains("prefixed-secret"));
    assert!(!stored.contains("private-key-material"));
    assert!(stored.contains("[REDACTED]"));
    assert!(stored.contains("curl -H"));
    assert!(!stored.contains("password: String"));
    assert!(!stored.contains("token: Option<String>"));
    assert!(stored.contains("[REDACTED:password:portable-export]"));
    assert!(stored.contains("[REDACTED:token:portable-export]"));
    assert!(stored.contains("Bearer {{CRED:token:abcdef012345}}"));
    assert!(!stored.contains("raw-adjacent-secret"));
    assert!(stored.contains("{{CRED:token:abcdef012345}}[REDACTED:token:portable-export]"));
    assert!(!stored.contains("sk-ant-abcdefghijklmnopqrstuvwxyz123456"));
    assert!(!stored.contains("{{CRED:sk-ant-"));
    assert!(stored.contains("[REDACTED:credential_reference:portable-export]"));
}

#[test]
fn controller_stream_preserves_only_executable_client_tool_arguments() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let (_, mut observer_live) = shared.reset_and_subscribe("observer");
    let (_, mut controller_live) = shared.reset_and_subscribe_controller("controller");
    let client_args = serde_json::json!({
        "command": "curl -H 'Authorization: Bearer client-execution-secret' example.test",
        "nested": {"value": "client-byte-faithful"}
    });
    let server_args = serde_json::json!({
        "command": "curl -H 'Authorization: Bearer server-execution-secret' example.test",
        "nested": ["server-byte-faithful", 7]
    });
    let approval_args = serde_json::json!({
        "command": "curl -H 'Authorization: Bearer approval-observer-secret' example.test"
    });
    let (observer_replay, controller_replay) = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.publish_message(
            &mut state,
            FromAgentMessage::ClientToolRequest {
                call_id: "client-call".into(),
                tool_execution_id: None,
                tool: "bash".into(),
                args: client_args.clone(),
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::ServerRequest {
                request_id: "server-client-tool".into(),
                request_type: ServerRequestType::ClientTool,
                call_id: "server-call".into(),
                tool_execution_id: None,
                tool: "bash".into(),
                args: server_args.clone(),
                reason: "run on the authenticated controller".into(),
                started_at_ms: None,
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::ServerRequest {
                request_id: "approval".into(),
                request_type: ServerRequestType::Approval,
                call_id: "approval-call".into(),
                tool_execution_id: None,
                tool: "bash".into(),
                args: approval_args,
                reason: "approval required".into(),
                started_at_ms: None,
            },
        );
        (
            state.envelopes.iter().cloned().collect::<Vec<_>>(),
            state
                .controller_envelopes
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
        )
    };

    let observer_live = (0..3)
        .map(|_| observer_live.try_recv().expect("observer event"))
        .collect::<Vec<_>>();
    let controller_live = (0..3)
        .map(|_| controller_live.try_recv().expect("controller event"))
        .collect::<Vec<_>>();

    for observer_copy in [&observer_replay, &observer_live] {
        let serialized = serde_json::to_string(observer_copy).expect("serialize observer stream");
        assert!(!serialized.contains("client-execution-secret"));
        assert!(!serialized.contains("server-execution-secret"));
        assert!(!serialized.contains("approval-observer-secret"));
        assert!(serialized.contains("[REDACTED:token:portable-export]"));
    }
    for controller_copy in [&controller_replay, &controller_live] {
        let messages = controller_copy
            .iter()
            .filter_map(|envelope| match envelope {
                StreamEnvelope::Message { message, .. } => Some(message.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            messages.as_slice(),
            [
                FromAgentMessage::ClientToolRequest { args, .. },
                FromAgentMessage::ServerRequest {
                    request_type: ServerRequestType::ClientTool,
                    args: server_request_args,
                    ..
                },
                FromAgentMessage::ServerRequest {
                    request_type: ServerRequestType::Approval,
                    args: approval_request_args,
                    ..
                }
            ] if args == &client_args
                && server_request_args == &server_args
                && serde_json::to_string(approval_request_args)
                    .expect("serialize approval args")
                    .contains("[REDACTED:token:portable-export]")
        ));
    }
}

#[test]
fn controller_subscribe_returns_only_current_raw_pending_executable_events() {
    let workspace = tempdir().expect("workspace");
    let pending_args = serde_json::json!({
        "command": "curl -H 'Authorization: Bearer pending-controller-secret' example.test",
        "nested": {"value": "pending-byte-faithful"}
    });
    let completed_args = serde_json::json!({
        "command": "curl -H 'Authorization: Bearer completed-controller-secret' example.test"
    });
    let mut agent_state = AgentState::default();
    let approval_args = serde_json::json!({
        "query": "computer browser create private computer terminal",
        "marker": "pending-approval-reconnect"
    });
    agent_state.pending_approvals.push(PendingApproval {
        call_id: "call_approval".to_string(),
        tool_execution_id: None,
        request_id: None,
        tool: "tool_search".to_string(),
        args: approval_args.clone(),
        started_at_ms: None,
    });
    agent_state.pending_client_tools.push(PendingApproval {
        call_id: "call_pending".to_string(),
        tool_execution_id: Some("exec_pending".to_string()),
        request_id: None,
        tool: "bash".to_string(),
        args: pending_args.clone(),
        started_at_ms: None,
    });
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(agent_state)),
        None,
    );
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.publish_message(
            &mut state,
            FromAgentMessage::ToolCall {
                call_id: "call_approval".to_string(),
                tool_execution_id: None,
                tool: "tool_search".to_string(),
                args: approval_args.clone(),
                requires_approval: true,
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::ClientToolRequest {
                call_id: "call_pending".to_string(),
                tool_execution_id: Some("exec_pending".to_string()),
                tool: "bash".to_string(),
                args: pending_args.clone(),
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::ClientToolRequest {
                call_id: "call_completed".to_string(),
                tool_execution_id: Some("exec_completed".to_string()),
                tool: "bash".to_string(),
                args: completed_args,
            },
        );
        let public_snapshot = shared.public_snapshot(&state);
        assert_eq!(
            public_snapshot.state.pending_client_tools[0]["args"],
            serde_json::json!({})
        );
        assert!(
            !serde_json::to_string(&public_snapshot)
                .expect("serialize public snapshot")
                .contains("pending-controller-secret")
        );

        for index in 0..=MAX_EVENTS {
            shared.publish_message(
                &mut state,
                FromAgentMessage::Status {
                    message: format!("later event {index}"),
                },
            );
        }
        assert!(
            !state.controller_envelopes.iter().any(|envelope| {
                matches!(
                    envelope,
                    StreamEnvelope::Message { message, .. }
                        if matches!(
                            message.as_ref(),
                            FromAgentMessage::ClientToolRequest { call_id, .. }
                                if call_id == "call_pending"
                        )
                )
            }),
            "the bounded replay queue should have evicted the original raw request"
        );
        assert_eq!(
            state.pending_controller_events.len(),
            2,
            "later non-executable traffic must not consume the pending-event bound"
        );
    }

    let subscribe = |connection_id: &str, role: ConnectionRole| SubscribeRequest {
        connection_id: Some(connection_id.to_string()),
        subscription_id: None,
        connection_capability: None,
        connection_capability_required: false,
        protocol_version: None,
        client_info: None,
        capabilities: None,
        opt_out_notifications: Vec::new(),
        role: Some(role),
        take_control: false,
    };
    let response_json = |response| match response {
        ResponseBody::Json { status, body } => {
            assert_eq!(status, 200);
            body
        }
        ResponseBody::Sse { .. } => panic!("expected JSON subscription response"),
    };

    let viewer = response_json(
        handle_subscribe(
            shared.clone(),
            "sess_test",
            subscribe("conn_viewer", ConnectionRole::Viewer),
        )
        .expect("viewer subscription"),
    );
    assert_eq!(viewer["controller_pending_events"], serde_json::json!([]));
    assert!(!viewer.to_string().contains("pending-controller-secret"));

    let controller = response_json(
        handle_subscribe(
            shared,
            "sess_test",
            subscribe("conn_controller", ConnectionRole::Controller),
        )
        .expect("controller subscription"),
    );
    let pending_events = controller["controller_pending_events"]
        .as_array()
        .expect("controller pending events");
    assert_eq!(pending_events.len(), 2);
    assert_eq!(pending_events[0]["type"], "server_request");
    assert_eq!(pending_events[0]["request_type"], "approval");
    assert_eq!(pending_events[0]["request_id"], "call_approval");
    assert_eq!(pending_events[0]["call_id"], "call_approval");
    assert_eq!(pending_events[0]["args"], approval_args);
    assert_eq!(pending_events[1]["type"], "client_tool_request");
    assert_eq!(pending_events[1]["call_id"], "call_pending");
    assert_eq!(pending_events[1]["tool_execution_id"], "exec_pending");
    assert_eq!(pending_events[1]["args"], pending_args);
    assert!(controller.to_string().contains("pending-controller-secret"));
    assert!(
        !controller
            .to_string()
            .contains("completed-controller-secret")
    );

    let controller_capability = controller["connection_capability"]
        .as_str()
        .expect("controller connection capability");
    assert!(
        !controller["snapshot"]
            .to_string()
            .contains(controller_capability)
    );
    assert!(
        !controller["snapshot"]
            .to_string()
            .contains("pending-controller-secret")
    );
}

#[test]
fn controller_recovers_every_live_pending_event_beyond_replay_capacity_once() {
    const PENDING_COUNT: usize = 1025;

    let workspace = tempdir().expect("workspace");
    let mut agent_state = AgentState::default();
    let mut pending_messages = Vec::with_capacity(PENDING_COUNT);
    for index in 0..PENDING_COUNT {
        let call_id = format!("call_{index}");
        let tool_execution_id = format!("exec_{index}");
        let args = serde_json::json!({
            "command": format!("echo pending-controller-secret-{index}")
        });
        agent_state.pending_client_tools.push(PendingApproval {
            call_id: call_id.clone(),
            tool_execution_id: Some(tool_execution_id.clone()),
            request_id: None,
            tool: "bash".to_string(),
            args: args.clone(),
            started_at_ms: None,
        });
        pending_messages.push(FromAgentMessage::ClientToolRequest {
            call_id,
            tool_execution_id: Some(tool_execution_id),
            tool: "bash".to_string(),
            args,
        });
    }

    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(agent_state)),
        None,
    );
    let recovered = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for message in pending_messages {
            shared.publish_message(&mut state, message);
        }
        shared.publish_message(
            &mut state,
            FromAgentMessage::ServerRequest {
                request_id: "approval_only".to_string(),
                request_type: ServerRequestType::Approval,
                call_id: "approval_only".to_string(),
                tool_execution_id: None,
                tool: "bash".to_string(),
                args: serde_json::json!({"secret": "non-client-secret"}),
                reason: "approval only".to_string(),
                started_at_ms: None,
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::ClientToolRequest {
                call_id: "call_completed".to_string(),
                tool_execution_id: Some("exec_completed".to_string()),
                tool: "bash".to_string(),
                args: serde_json::json!({"secret": "completed-controller-secret"}),
            },
        );

        assert_eq!(
            state.pending_controller_events.len(),
            PENDING_COUNT,
            "the dedicated queue must retain one event per authoritative live request"
        );
        assert!(
            !state.controller_envelopes.iter().any(|envelope| {
                matches!(
                    envelope,
                    StreamEnvelope::Message { message, .. }
                        if matches!(
                            message.as_ref(),
                            FromAgentMessage::ClientToolRequest { call_id, .. }
                                if call_id == "call_0"
                        )
                )
            }),
            "the generic replay queue should demonstrate that attach depends on dedicated retention"
        );
        let public_snapshot = shared.public_snapshot(&state);
        assert_eq!(
            public_snapshot.state.pending_client_tools.len(),
            PENDING_COUNT
        );
        assert!(
            public_snapshot
                .state
                .pending_client_tools
                .iter()
                .all(|pending| pending["args"] == serde_json::json!({}))
        );
        assert!(
            !serde_json::to_string(&public_snapshot)
                .expect("serialize public snapshot")
                .contains("pending-controller-secret")
        );

        shared.controller_pending_events(&mut state)
    };

    assert_eq!(recovered.len(), PENDING_COUNT);
    let recovered_call_ids = recovered
        .iter()
        .filter_map(|message| match message {
            FromAgentMessage::ClientToolRequest { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    assert_eq!(recovered_call_ids.len(), PENDING_COUNT);
    assert!(recovered_call_ids.contains("call_0"));
    assert!(recovered_call_ids.contains("call_1024"));
    let serialized = serde_json::to_string(&recovered).expect("serialize recovered events");
    assert!(serialized.contains("pending-controller-secret-0"));
    assert!(serialized.contains("pending-controller-secret-1024"));
    assert!(!serialized.contains("non-client-secret"));
    assert!(!serialized.contains("completed-controller-secret"));
}

#[tokio::test]
async fn zero_message_completion_prunes_retained_controller_event_immediately() {
    let workspace = tempdir().expect("workspace");
    let pending = PendingApproval {
        call_id: "call_completed_silently".to_string(),
        tool_execution_id: Some("exec_completed_silently".to_string()),
        request_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "echo retained-secret"}),
        started_at_ms: None,
    };
    let mut agent_state = AgentState::default();
    agent_state.pending_client_tools.push(pending.clone());
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(CompletingRuntimeExecutor::new(agent_state)),
        None,
    );

    let (connection_capability, headers) = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        upsert_connection(
            &mut state,
            ConnectionUpsert {
                connection_id: "conn_zero_message".to_string(),
                connection_capability: None,
                connection_capability_required: true,
                role: ConnectionRole::Controller,
                client_protocol_version: None,
                client_info: None,
                capabilities: None,
                opt_out_notifications: vec![],
                take_control: false,
            },
        )
        .expect("controller connection");
        let connection_capability = state
            .connections
            .get("conn_zero_message")
            .expect("connection")
            .connection_capability
            .clone();
        state.subscriptions.insert(
            "sub_zero_message".to_string(),
            SubscriptionRecord {
                connection_id: "conn_zero_message".to_string(),
                connection_capability: connection_capability.clone(),
                authority_mode: ConnectionAuthorityMode::Capability,
                role: ConnectionRole::Controller,
                attached: true,
            },
        );
        shared.publish_message(
            &mut state,
            FromAgentMessage::ClientToolRequest {
                call_id: pending.call_id.clone(),
                tool_execution_id: pending.tool_execution_id.clone(),
                tool: pending.tool.clone(),
                args: pending.args.clone(),
            },
        );
        assert_eq!(state.pending_controller_events.len(), 1);
        let headers = HashMap::from([
            (
                "x-maestro-headless-connection-id".to_string(),
                "conn_zero_message".to_string(),
            ),
            (
                "x-maestro-headless-subscriber-id".to_string(),
                "sub_zero_message".to_string(),
            ),
            (
                "x-maestro-headless-connection-capability".to_string(),
                connection_capability.clone().expect("private capability"),
            ),
        ]);
        (connection_capability, headers)
    };

    let response = handle_message(
        shared.clone(),
        "sess_test",
        headers,
        ToAgentMessage::ClientToolResult {
            call_id: pending.call_id,
            content: Vec::new(),
            is_error: false,
        },
    )
    .await
    .expect("zero-message completion");
    assert!(matches!(response, ResponseBody::Json { .. }));

    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(state.pending_controller_events.is_empty());
    assert!(shared.controller_pending_events(&mut state).is_empty());
    assert_eq!(
        state
            .connections
            .get("conn_zero_message")
            .expect("connection")
            .connection_capability,
        connection_capability
    );
}

#[test]
fn pending_client_tool_server_request_uses_inherited_execution_identity() {
    let workspace = tempdir().expect("workspace");
    let tracked = FromAgentMessage::ToolCall {
        call_id: "call_inherited".to_string(),
        tool_execution_id: Some("exec_inherited".to_string()),
        tool: "bash".to_string(),
        args: serde_json::json!({"command": "prepare"}),
        requires_approval: false,
    };
    let pending = FromAgentMessage::ServerRequest {
        request_id: "request_pending".to_string(),
        request_type: ServerRequestType::ClientTool,
        call_id: "call_inherited".to_string(),
        tool_execution_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({
            "command": "curl -H 'Authorization: Bearer inherited-pending-secret' example.test"
        }),
        reason: "execute on controller".to_string(),
        started_at_ms: None,
    };
    let non_client = FromAgentMessage::ServerRequest {
        request_id: "request_pending".to_string(),
        request_type: ServerRequestType::Approval,
        call_id: "call_inherited".to_string(),
        tool_execution_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({
            "command": "curl -H 'Authorization: Bearer non-client-secret' example.test"
        }),
        reason: "approval only".to_string(),
        started_at_ms: None,
    };
    let completed = FromAgentMessage::ServerRequest {
        request_id: "request_completed".to_string(),
        request_type: ServerRequestType::ClientTool,
        call_id: "call_inherited".to_string(),
        tool_execution_id: None,
        tool: "bash".to_string(),
        args: serde_json::json!({
            "command": "curl -H 'Authorization: Bearer completed-request-secret' example.test"
        }),
        reason: "already completed".to_string(),
        started_at_ms: None,
    };

    let mut agent_state = AgentState::default();
    let _ = agent_state.handle_message(tracked.clone());
    let _ = agent_state.handle_message(pending.clone());
    assert_eq!(agent_state.pending_client_tools.len(), 1);
    assert_eq!(
        agent_state.pending_client_tools[0]
            .tool_execution_id
            .as_deref(),
        Some("exec_inherited")
    );
    assert_eq!(
        agent_state.pending_client_tools[0].request_id.as_deref(),
        Some("request_pending")
    );

    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(agent_state)),
        None,
    );
    let pending_events = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.publish_message(&mut state, tracked);
        shared.publish_message(&mut state, pending);
        shared.publish_message(&mut state, non_client);
        shared.publish_message(&mut state, completed);
        shared.controller_pending_events(&mut state)
    };

    assert_eq!(pending_events.len(), 1);
    assert!(matches!(
        pending_events.as_slice(),
        [FromAgentMessage::ServerRequest {
            request_id,
            request_type: ServerRequestType::ClientTool,
            call_id,
            tool_execution_id: None,
            args,
            ..
        }] if request_id == "request_pending"
            && call_id == "call_inherited"
            && args
                .get("command")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|command| command.contains("inherited-pending-secret"))
    ));
    let serialized = serde_json::to_string(&pending_events).expect("serialize pending events");
    assert!(!serialized.contains("non-client-secret"));
    assert!(!serialized.contains("completed-request-secret"));
}

#[test]
fn transcript_filters_are_connection_local() {
    let response_chunk = || {
        stream_message(
            7,
            FromAgentMessage::ResponseChunk {
                response_id: "response".to_string(),
                content: "hello".to_string(),
                is_thinking: false,
            },
        )
    };
    let mut delta = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Delta, 0);
    let mut off = TranscriptStreamFilter::new(crate::transcript::TranscriptGrade::Off, 0);

    assert_eq!(delta.apply(response_chunk()).len(), 1);
    assert!(off.apply(response_chunk()).is_empty());
}

#[test]
fn resolves_env_config_with_hosted_runner_contract_names() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert(
        "MAESTRO_REMOTE_RUNNER_OWNER_INSTANCE_ID".to_string(),
        "pod_1".to_string(),
    );
    env.insert(
        "MAESTRO_SANDBOXWICH_PLACEMENT_GENERATION".to_string(),
        "42".to_string(),
    );
    env.insert(
        "MAESTRO_HOSTED_RUNNER_LISTEN".to_string(),
        "127.0.0.1:9090".to_string(),
    );
    env.insert(
        "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID".to_string(),
        "workspace_1".to_string(),
    );
    env.insert(
        "MAESTRO_AGENT_RUN_ID".to_string(),
        "agent_run_1".to_string(),
    );
    env.insert(
        "MAESTRO_CAUSAL_RECEIPT_ID".to_string(),
        "causal.receipt:platform-1".to_string(),
    );
    env.insert(
        "MAESTRO_REMOTE_RUNNER_SNAPSHOT_ROOT".to_string(),
        ".snapshots".to_string(),
    );
    env.insert(
        "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST".to_string(),
        ".snapshots/restore.json".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");
    assert_eq!(config.runner_session_id, "mrs_123");
    assert_eq!(config.owner_instance_id.as_deref(), Some("pod_1"));
    assert_eq!(config.runtime_generation, 42);
    assert_eq!(
        config.workspace_root,
        dunce::canonicalize(workspace.path()).unwrap()
    );
    assert_eq!(config.bind_addr, "127.0.0.1:9090".parse().unwrap());
    assert_eq!(
        config.snapshot_root.as_deref(),
        Some(
            dunce::canonicalize(workspace.path())
                .unwrap()
                .join(".snapshots")
                .as_path()
        )
    );
    assert_eq!(config.workspace_id.as_deref(), Some("workspace_1"));
    assert_eq!(config.agent_run_id.as_deref(), Some("agent_run_1"));
    assert_eq!(
        config.causal_receipt_id.as_deref(),
        Some("causal.receipt:platform-1")
    );
    assert_eq!(
        config.restore_manifest_path.as_deref(),
        Some(
            dunce::canonicalize(workspace.path())
                .unwrap()
                .join(".snapshots/restore.json")
                .as_path()
        )
    );
}

#[test]
fn causal_receipt_id_validation_is_bounded_and_fail_closed() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());

    for value in ["", "contains/slash", "contains whitespace"] {
        env.insert("MAESTRO_CAUSAL_RECEIPT_ID".to_string(), value.to_string());
        let error = HostedRunnerConfig::from_env_map(&env).expect_err("invalid receipt id");
        assert!(error.to_string().contains("MAESTRO_CAUSAL_RECEIPT_ID"));
    }

    env.insert("MAESTRO_CAUSAL_RECEIPT_ID".to_string(), "a".repeat(129));
    let error = HostedRunnerConfig::from_env_map(&env).expect_err("oversized receipt id");
    assert!(error.to_string().contains("MAESTRO_CAUSAL_RECEIPT_ID"));
}

#[test]
fn canonical_placement_generation_wins_for_runtime_fencing() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    let token_file = workspace.path().join("projected-token");
    add_workload_identity_env(&mut env, &token_file);
    env.insert(
        "MAESTRO_SANDBOXWICH_PLACEMENT_GENERATION".to_string(),
        "11".to_string(),
    );
    env.insert(
        "MAESTRO_REMOTE_RUNNER_GENERATION".to_string(),
        "12".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");

    assert_eq!(config.runtime_generation, 7);
    assert_eq!(
        config
            .workload_identity
            .expect("workload identity")
            .placement_generation,
        7
    );
}

#[test]
fn defaults_env_only_hosted_runner_bind_to_wildcard_with_legacy_auth() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert(
        "MAESTRO_WEB_API_KEY".to_string(),
        "legacy-secret".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");

    assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
    assert_eq!(config.auth_token.as_deref(), Some("legacy-secret"));
}

#[test]
fn hosted_runner_auth_token_takes_precedence_over_legacy_web_api_key() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert(
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN".to_string(),
        "runner-secret".to_string(),
    );
    env.insert(
        "MAESTRO_WEB_API_KEY".to_string(),
        "legacy-secret".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");

    assert_eq!(config.auth_token.as_deref(), Some("runner-secret"));
}

#[test]
fn rejects_default_env_only_wildcard_bind_without_auth_token() {
    let workspace = tempdir().expect("workspace");
    let env = base_hosted_runner_env(workspace.path());

    let error = HostedRunnerConfig::from_env_map(&env).expect_err("expected config error");

    assert!(
        error
            .to_string()
            .contains("MAESTRO_HOSTED_RUNNER_AUTH_TOKEN")
    );
}

#[test]
fn projected_workload_identity_replaces_static_auth_on_non_loopback_bind() {
    let workspace = tempdir().expect("workspace");
    let token_file = workspace.path().join("projected-token");
    let mut env = base_hosted_runner_env(workspace.path());
    add_workload_identity_env(&mut env, &token_file);

    let config = HostedRunnerConfig::from_env_map(&env)
        .expect("complete projected identity should secure the wildcard listener");

    assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
    assert!(config.auth_token.is_none());
}

#[test]
fn startup_join_uses_identity_bound_workspace() {
    let workspace = tempdir().expect("workspace");
    let token_file = workspace.path().join("projected-token");
    let mut env = base_hosted_runner_env(workspace.path());
    add_workload_identity_env(&mut env, &token_file);
    env.insert(
        "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID".to_string(),
        "unrelated-remote-workspace".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("workload identity config");

    assert_eq!(startup_workspace_id(&config), "workspace-123");
}

#[test]
fn projected_workload_identity_forbids_static_bearer_fallback() {
    let workspace = tempdir().expect("workspace");
    let token_file = workspace.path().join("projected-token");
    let mut env = base_hosted_runner_env(workspace.path());
    add_workload_identity_env(&mut env, &token_file);
    env.insert(
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN".to_string(),
        "long-lived-secret".to_string(),
    );

    let error = HostedRunnerConfig::from_env_map(&env)
        .expect_err("secure mode must not retain a static bearer alternate");

    assert!(error.to_string().contains("forbids static bearer"));
}

#[test]
fn projected_workload_identity_rejects_external_runner_client_ca() {
    let workspace = tempdir().expect("workspace");
    let token_file = workspace.path().join("projected-token");
    let mut env = base_hosted_runner_env(workspace.path());
    add_workload_identity_env(&mut env, &token_file);
    env.insert(
        "MAESTRO_RUNNER_CLIENT_CA_FILE".to_string(),
        workspace
            .path()
            .join("ambient-client-ca.crt")
            .display()
            .to_string(),
    );

    let error = HostedRunnerConfig::from_env_map(&env)
        .expect_err("client trust must come only from the signed exchange response");

    assert!(error.to_string().contains("not supported"));
}

#[test]
fn projected_workload_identity_rejects_partial_binding() {
    let workspace = tempdir().expect("workspace");
    let token_file = workspace.path().join("projected-token");
    let mut env = base_hosted_runner_env(workspace.path());
    add_workload_identity_env(&mut env, &token_file);
    env.remove("MAESTRO_SANDBOX_ID");
    env.insert(
        "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN".to_string(),
        "legacy-secret".to_string(),
    );

    let error = HostedRunnerConfig::from_env_map(&env)
        .expect_err("partial workload identity must fail closed");

    assert!(error.to_string().contains("MAESTRO_SANDBOX_ID"));
}

#[test]
fn preserves_wildcard_bind_for_port_only_hosted_runner_env() {
    let workspace = tempdir().expect("workspace");
    for (key, value) in [
        ("MAESTRO_HOSTED_RUNNER_LISTEN", "9090"),
        ("MAESTRO_HOSTED_RUNNER_PORT", "9091"),
        ("PORT", "9092"),
    ] {
        let mut env = base_hosted_runner_env(workspace.path());
        env.insert(key.to_string(), value.to_string());
        env.insert(
            "MAESTRO_HOSTED_RUNNER_AUTH_TOKEN".to_string(),
            "secret-token".to_string(),
        );

        let config = HostedRunnerConfig::from_env_map(&env).expect("config");

        assert_eq!(
            config.bind_addr,
            format!("0.0.0.0:{value}").parse().unwrap(),
            "{key} should preserve hosted ingress wildcard binding"
        );
    }
}

#[test]
fn rejects_non_loopback_bind_without_auth_token() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert(
        "MAESTRO_HOSTED_RUNNER_LISTEN".to_string(),
        "0.0.0.0:9090".to_string(),
    );

    let error = HostedRunnerConfig::from_env_map(&env).expect_err("expected config error");
    assert!(
        error
            .to_string()
            .contains("MAESTRO_HOSTED_RUNNER_AUTH_TOKEN")
    );
}

#[tokio::test]
async fn rejects_public_api_non_loopback_bind_without_auth_token() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf())
        .with_bind_addr("0.0.0.0:0".parse().expect("bind addr"));

    let error = match start_hosted_runner(config).await {
        Ok(_) => panic!("expected auth token error"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("auth_token"));
}

#[tokio::test]
async fn rejects_public_api_non_loopback_bind_with_blank_auth_token() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf())
        .with_bind_addr("0.0.0.0:0".parse().expect("bind addr"))
        .with_auth_token("   ");

    let error = match start_hosted_runner(config).await {
        Ok(_) => panic!("expected auth token error"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("auth_token"));
}

#[test]
fn explicit_host_env_overrides_port_only_wildcard_bind() {
    let workspace = tempdir().expect("workspace");
    let mut env = base_hosted_runner_env(workspace.path());
    env.insert("MAESTRO_HOSTED_RUNNER_PORT".to_string(), "9090".to_string());
    env.insert(
        "MAESTRO_HOSTED_RUNNER_HOST".to_string(),
        "127.0.0.1".to_string(),
    );

    let config = HostedRunnerConfig::from_env_map(&env).expect("config");

    assert_eq!(config.bind_addr, "127.0.0.1:9090".parse().unwrap());
}

#[tokio::test]
async fn headless_routes_require_auth_token_when_configured() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf()).with_auth_token("secret-token");
    let handle = start_hosted_runner(config)
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let unauthorized = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("unauthorized response");
    assert_eq!(unauthorized.status(), StatusCode::FORBIDDEN);

    let authorized = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer secret-token")
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("authorized response");
    assert_eq!(authorized.status(), StatusCode::OK);

    let custom_token_authorized = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer wrong-forwarded-token")
        .header("x-maestro-hosted-runner-token", "secret-token")
        .json(&json!({"sessionId": "sess_test", "role": "viewer"}))
        .send()
        .await
        .expect("custom token authorized response");
    assert_eq!(custom_token_authorized.status(), StatusCode::OK);

    for legacy_header in ["x-maestro-api-key", "x-composer-api-key"] {
        let legacy_authorized = client
            .post(format!("{}/api/headless/connections", handle.base_url()))
            .header(legacy_header, "secret-token")
            .json(&json!({"sessionId": "sess_test", "role": "viewer"}))
            .send()
            .await
            .expect("legacy API key authorized response");
        assert_eq!(
            legacy_authorized.status(),
            StatusCode::OK,
            "{legacy_header}"
        );
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn loopback_drain_allows_prestop_without_auth_token() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf()).with_auth_token("secret-token");
    let handle = start_hosted_runner(config)
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let drain = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "preStop", "requested_by": "kubernetes", "export_paths": ["."]}))
        .send()
        .await
        .expect("drain response");

    assert_eq!(drain.status(), StatusCode::OK);
    handle.shutdown().await;
}

#[tokio::test]
async fn remote_drain_requires_auth_token_when_configured() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(
        test_config(workspace.path().to_path_buf()).with_auth_token("secret-token"),
    );
    let request = HttpRequest {
        method: "POST".to_string(),
        path: HOSTED_RUNNER_DRAIN_PATH.to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
        body: serde_json::to_vec(
            &json!({"reason": "remote", "requested_by": "platform", "export_paths": ["."]}),
        )
        .expect("drain request"),
    };

    let error = match route_request(request, shared, "203.0.113.10:4567".parse().unwrap()).await {
        Ok(_) => panic!("remote drain without token should be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.status, StatusCode::FORBIDDEN.as_u16());
    assert_eq!(error.code, HostedRunnerErrorCode::AccessDenied);
}

#[test]
fn sse_lag_reset_envelope_includes_current_snapshot() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    {
        let mut state = shared.state.lock().expect("state");
        shared.publish_message(
            &mut state,
            FromAgentMessage::ProviderError {
                kind: maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure,
                message: "provider rejected the response".to_string(),
            },
        );
    }

    let envelope = shared.reset_envelope("broadcast_lag:3");
    let StreamEnvelope::Reset { reason, snapshot } = envelope else {
        panic!("expected reset envelope");
    };
    assert_eq!(reason, "broadcast_lag:3");
    assert_eq!(snapshot.cursor, 1);
    assert_eq!(
        snapshot.state.provider_error_kind,
        Some(maestro_ai::ProviderStreamErrorKind::ProviderDeclaredFailure)
    );
}

#[tokio::test]
async fn events_negative_cursor_returns_replay_gap_reset() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let mut events_response = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=-999",
            handle.base_url()
        ))
        .send()
        .await
        .expect("events response");
    assert_eq!(events_response.status(), StatusCode::OK);
    let event_text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut event_text = String::new();
        while !event_text.contains(r#""reason":"replay_gap""#) {
            let chunk = events_response
                .chunk()
                .await
                .expect("event chunk read")
                .expect("event chunk");
            event_text.push_str(&String::from_utf8_lossy(&chunk));
        }
        event_text
    })
    .await
    .expect("event chunk timeout");
    assert!(event_text.contains(r#""type":"reset""#));
    assert!(event_text.contains(r#""reason":"replay_gap""#));

    handle.shutdown().await;
}

#[test]
fn connection_headers_accept_evalops_subscription_aliases() {
    let headers = HashMap::from([
        (
            "x-evalops-headless-connection-id".to_string(),
            "conn_evalops".to_string(),
        ),
        (
            "x-evalops-headless-subscription-id".to_string(),
            "sub_evalops".to_string(),
        ),
    ]);

    let (connection_id, subscription_id, connection_capability) = connection_from_headers(&headers);

    assert_eq!(connection_id.as_deref(), Some("conn_evalops"));
    assert_eq!(subscription_id.as_deref(), Some("sub_evalops"));
    assert!(connection_capability.is_none());
}

#[tokio::test]
async fn identity_and_drain_follow_runner_contract() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let handle = start_hosted_runner(config.clone())
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let identity: HostedRunnerIdentity = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            handle.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert_eq!(identity.runner_session_id, "mrs_test");
    assert_eq!(identity.owner_instance_id.as_deref(), Some("owner_test"));
    assert!(identity.ready);
    assert!(!identity.draining);

    let identity_json: serde_json::Value = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            handle.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert!(identity_json.get("workspace_id").is_none());
    assert!(identity_json.get("agent_run_id").is_none());
    assert!(identity_json.get("maestro_session_id").is_none());

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "test", "requested_by": "platform", "export_paths": ["."]}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    assert_eq!(drain["status"], "drained");
    let manifest_path = drain["manifest_path"].as_str().expect("manifest path");
    assert!(Path::new(manifest_path).exists());
    let manifest_bytes = std::fs::read(manifest_path).expect("manifest contents");
    let typed_manifest =
        parse_snapshot_manifest_bytes(&manifest_bytes, workspace.path()).expect("typed manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("manifest json");
    assert_eq!(drain["manifest"], manifest);
    assert_eq!(
        typed_manifest.protocol_version,
        HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION
    );
    assert_eq!(
        manifest["protocol_version"],
        HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION
    );
    assert_eq!(manifest["workspace_export"]["mode"], "local_path_contract");
    assert_eq!(
        manifest["workspace_export"]["paths"][0]["relative_path"],
        "."
    );
    assert_eq!(
        manifest["workspace_export"]["paths"][0]["type"],
        "directory"
    );
    assert_eq!(
        manifest["work_continuity"]["protocol_version"],
        HOSTED_RUNNER_WORK_CONTINUITY_VERSION
    );
    assert_eq!(
        manifest["runtime_continuity"]["protocol_version"],
        HOSTED_RUNNER_RUNTIME_CONTINUITY_VERSION
    );
    assert_eq!(manifest["runtime_continuity"]["handoff"], "drain_restore");
    assert_eq!(
        manifest["runtime_continuity"]["source_runner_session_id"],
        "mrs_test"
    );
    assert_eq!(
        manifest["runtime_continuity"]["source_owner_instance_id"],
        "owner_test"
    );
    assert!(
        manifest["runtime_continuity"]["source_process_id"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(
        manifest["runtime_continuity"]["restore_environment_key"],
        "MAESTRO_REMOTE_RUNNER_RESTORE_MANIFEST"
    );
    assert_eq!(manifest["work_continuity"]["active_tool_count"], 0);
    assert_eq!(manifest["work_continuity"]["tracked_tool_count"], 0);
    assert_eq!(
        manifest["work_continuity"]["codex_subagent_tool_call_ids"],
        json!([])
    );
    assert_eq!(
        manifest["retention_policy"]["policy_version"],
        HOSTED_RUNNER_RETENTION_POLICY_VERSION
    );
    assert_eq!(
        manifest["retention_policy"]["visibility"]["runtime_snapshot"],
        "internal"
    );
    assert_eq!(
        manifest["platform_evidence"]["protocol_version"],
        HOSTED_RUNNER_PLATFORM_EVIDENCE_VERSION
    );
    assert_eq!(
        manifest["platform_evidence"]["runtime_flush_status"],
        "skipped"
    );
    assert_eq!(
        manifest["platform_evidence"]["runtime_continuity"]["protocol_version"],
        HOSTED_RUNNER_RUNTIME_CONTINUITY_VERSION
    );
    assert_eq!(
        manifest["platform_evidence"]["manifest_path"],
        manifest_path
    );

    let post_drain_identity: HostedRunnerIdentity = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            handle.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert!(!post_drain_identity.ready);
    assert!(post_drain_identity.draining);

    let post_drain_state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(post_drain_state["state"]["is_ready"], false);
    assert_eq!(post_drain_state["state"]["last_status"], "Drained");
    let source_lineage = drain["runtime_receipt"]["snapshotLineage"]
        .as_str()
        .expect("drain receipt lineage");
    assert!(source_lineage.starts_with("snapshot:"));

    let attach = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("attach response");
    assert_eq!(attach.status(), StatusCode::SERVICE_UNAVAILABLE);
    handle.shutdown().await;

    let restarted = start_hosted_runner(config)
        .await
        .expect("restart drained hosted runner");
    let restarted_receipt: serde_json::Value = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/receipt",
            restarted.base_url()
        ))
        .send()
        .await
        .expect("restarted receipt response")
        .error_for_status()
        .expect("restarted receipt status")
        .json()
        .await
        .expect("restarted receipt json");
    assert_eq!(
        restarted_receipt["snapshotLineage"].as_str(),
        Some(source_lineage)
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn drain_manifest_records_runtime_cursor_after_activity() {
    let workspace = tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.md"), "notes").expect("workspace file");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_drain",
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    assert_eq!(connection["connection_id"], "conn_drain");
    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_drain",
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    assert!(
        subscription["controller_subscription_id"]
            .as_str()
            .is_some()
    );
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id");
    let connection_capability = subscription["connection_capability"]
        .as_str()
        .expect("connection capability");

    let message: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_drain")
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            connection_capability,
        )
        .json(&json!({"type": "prompt", "content": "before drain"}))
        .send()
        .await
        .expect("message response")
        .json()
        .await
        .expect("message json");
    assert!(message["cursor"].as_u64().unwrap_or_default() > 0);

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "cursor-check", "export_paths": ["notes.md"]}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    let manifest_path = drain["manifest_path"].as_str().expect("manifest path");
    let manifest_bytes = std::fs::read(manifest_path).expect("manifest contents");
    let typed_manifest =
        parse_snapshot_manifest_bytes(&manifest_bytes, workspace.path()).expect("typed manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("manifest json");
    assert!(drain["manifest"]["snapshot"]["state"]["controller_subscription_id"].is_null());
    assert!(manifest["snapshot"]["state"]["controller_subscription_id"].is_null());
    assert_eq!(drain["runtime_receipt"]["kind"], "drained");
    assert_eq!(drain["runtime_receipt"]["lifecycleState"], "drained");
    assert_eq!(
        drain["runtime_receipt"]["replayCursor"],
        manifest["runtime"]["cursor"]
    );
    assert!(
        drain["runtime_receipt"]["flushWatermark"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(manifest["runtime"]["flush_status"], "completed");
    assert_eq!(
        typed_manifest.runtime.flush_status,
        RuntimeFlushStatus::Completed
    );
    assert_eq!(
        manifest["runtime"]["protocol_version"],
        HEADLESS_PROTOCOL_VERSION
    );
    assert!(manifest["runtime"]["cursor"].as_u64().unwrap_or_default() >= 2);
    assert_eq!(drain["manifest"], manifest);
    assert_eq!(
        manifest["workspace_export"]["paths"][0]["input"],
        "notes.md"
    );
    assert_eq!(
        manifest["workspace_export"]["paths"][0]["relative_path"],
        "notes.md"
    );
    assert_eq!(manifest["workspace_export"]["paths"][0]["type"], "file");
    assert_eq!(
        manifest["retention_policy"]["redaction"]["required_before_external_persistence"],
        json!(["runtime_snapshot", "runtime_logs"])
    );
    let mut escaped_manifest = manifest.clone();
    escaped_manifest["workspace_export"]["paths"][0]["relative_path"] = json!("../secret.txt");
    let escaped_bytes = serde_json::to_vec(&escaped_manifest).expect("escaped manifest json");
    let error =
        parse_snapshot_manifest_bytes(&escaped_bytes, workspace.path()).expect_err("escape");
    assert_eq!(error.code, HostedRunnerErrorCode::WorkspaceViolation);

    handle.shutdown().await;
}

#[tokio::test]
async fn drain_manifest_exports_typed_replay_sidecar_with_narrowed_paths() {
    let workspace = tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.md"), "notes").expect("workspace file");
    let sessions = workspace.path().join(".maestro/sessions");
    std::fs::create_dir_all(&sessions).expect("sessions directory");
    let session_file = sessions.join("sess_test.jsonl");
    std::fs::write(&session_file, "{}\n").expect("session journal");
    let replay_file = session_file.with_extension("replay.json");
    std::fs::write(&replay_file, "{\"semantic_conversation\":[]}").expect("replay sidecar");
    assert!(
        replay_file.starts_with(workspace.path()),
        "{} is outside {}",
        replay_file.display(),
        workspace.path().display()
    );

    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(SessionArtifactExecutor {
            session_file: session_file.clone(),
        }),
    )
    .await
    .expect("start hosted runner");
    let response = reqwest::Client::new()
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({ "export_paths": ["notes.md"] }))
        .send()
        .await
        .expect("drain request");
    let status = response.status();
    let response_body = response.text().await.expect("drain response body");
    assert_eq!(status, StatusCode::OK, "{response_body}");
    let drain: serde_json::Value = serde_json::from_str(&response_body).expect("drain json");

    assert_eq!(
        drain["manifest"]["runtime"]["replay_file"],
        serde_json::Value::String(replay_file.display().to_string())
    );
    let export_paths = drain["manifest"]["workspace_export"]["paths"]
        .as_array()
        .expect("workspace export paths");
    assert!(export_paths.iter().any(|path| {
        path["relative_path"] == ".maestro/sessions/sess_test.replay.json" && path["type"] == "file"
    }));
    handle.shutdown().await;
}

#[tokio::test]
async fn restore_manifest_seeds_runtime_state_and_replay_marker() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_restore",
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    assert_eq!(connection["connection_id"], "conn_restore");

    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_restore",
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let message: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_restore")
        .header(
            "x-maestro-headless-subscriber-id",
            subscription["subscription_id"]
                .as_str()
                .expect("subscription id"),
        )
        .header(
            "x-maestro-headless-connection-capability",
            subscription["connection_capability"]
                .as_str()
                .expect("connection capability"),
        )
        .json(&json!({"type": "prompt", "content": "before restore"}))
        .send()
        .await
        .expect("message response")
        .json()
        .await
        .expect("message json");
    assert!(message["cursor"].as_u64().unwrap_or_default() > 0);

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "restore-check"}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    let manifest_path = PathBuf::from(drain["manifest_path"].as_str().expect("manifest path"));
    let restored_cursor = drain["manifest"]["runtime"]["cursor"]
        .as_u64()
        .expect("manifest cursor");
    let source_snapshot_lineage = format!(
        "snapshot:{}",
        drain["manifest"]["created_at"]
            .as_str()
            .expect("manifest creation time")
    );
    let mut restore_manifest = drain["manifest"].clone();
    restore_manifest["snapshot"]["state"]["pending_approvals"] = json!([{
        "call_id": "completed-restore-approval-call",
        "request_id": "completed-restore-approval",
        "tool": "bash",
        "args": {"cmd": "cat secret.txt"},
        "started_at_ms": 1_771_000_002_000u64
    }]);
    restore_manifest["snapshot"]["state"]["pending_user_inputs"] = json!([{
        "call_id": "completed-restore-input-call",
        "request_id": "completed-restore-input",
        "tool": "user_input",
        "args": {"prompt": "enter secret"},
        "started_at_ms": 1_771_000_002_100u64
    }]);
    restore_manifest["snapshot"]["state"]["pending_tool_retries"] = json!([{
        "call_id": "completed-restore-retry-call",
        "request_id": "completed-restore-retry",
        "tool": "bash",
        "args": {"stderr": "token in error"},
        "started_at_ms": 1_771_000_002_200u64
    }]);
    restore_manifest["snapshot"]["state"]["active_tools"] = json!([{
        "call_id": "completed-restore-active-call",
        "tool": "bash",
        "output": "secret active output"
    }]);
    tokio::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&restore_manifest).expect("manifest json"),
    )
    .await
    .expect("write restore manifest");
    handle.shutdown().await;

    let mut restore_config = test_config(workspace.path().to_path_buf());
    restore_config.runner_session_id = "mrs_restored".to_string();
    restore_config.maestro_session_id = None;
    restore_config.restore_manifest_path = Some(manifest_path.clone());
    let live_empty_state = AgentState {
        is_ready: true,
        ..AgentState::default()
    };
    let restored = start_hosted_runner_with_message_executor(
        restore_config,
        Arc::new(StatefulRuntimeExecutor::new(live_empty_state)),
    )
    .await
    .expect("start restored hosted runner");

    let identity: HostedRunnerIdentity = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            restored.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert_eq!(identity.runner_session_id, "mrs_restored");
    assert!(identity.ready);
    assert!(!identity.draining);

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            restored.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["session_id"], "sess_test");
    assert_eq!(state["cursor"], restored_cursor);
    assert_eq!(state["state"]["last_status"], "Restored from snapshot");
    assert_eq!(state["state"]["is_ready"], true);
    assert_eq!(state["runtime_receipt"]["kind"], "restored");
    assert_eq!(
        state["runtime_receipt"]["lifecycleState"],
        "execution_ready"
    );
    assert_eq!(
        state["runtime_receipt"]["snapshotLineage"],
        source_snapshot_lineage
    );
    assert_eq!(
        state["state"]["pending_approvals"][0]["call_id"],
        "completed-restore-approval-call"
    );
    assert_eq!(state["state"]["pending_approvals"][0]["args"], json!({}));
    assert_eq!(
        state["state"]["pending_user_inputs"][0]["call_id"],
        "completed-restore-input-call"
    );
    assert_eq!(state["state"]["pending_user_inputs"][0]["args"], json!({}));
    assert_eq!(
        state["state"]["pending_tool_retries"][0]["call_id"],
        "completed-restore-retry-call"
    );
    assert_eq!(state["state"]["pending_tool_retries"][0]["args"], json!({}));
    assert_eq!(
        state["state"]["active_tools"][0]["call_id"],
        "completed-restore-active-call"
    );
    assert_eq!(state["state"]["active_tools"][0]["output"], "");

    {
        let mut restored_runner_state =
            restored.shared.state.lock().expect("restored runner state");
        restored_runner_state.cursor = restored_cursor + 1;
    }
    let cleared_state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            restored.base_url()
        ))
        .send()
        .await
        .expect("cleared state response")
        .json()
        .await
        .expect("cleared state json");
    assert_eq!(cleared_state["cursor"], restored_cursor + 1);
    assert_eq!(cleared_state["state"]["pending_approvals"], json!([]));
    assert_eq!(cleared_state["state"]["pending_user_inputs"], json!([]));
    assert_eq!(cleared_state["state"]["pending_tool_retries"], json!([]));
    assert_eq!(cleared_state["state"]["active_tools"], json!([]));

    let mut events_response = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0",
            restored.base_url()
        ))
        .send()
        .await
        .expect("events response");
    assert_eq!(events_response.status(), StatusCode::OK);
    let event_text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut event_text = String::new();
        while !event_text.contains(r#""reason":"restored_from_snapshot""#) {
            let chunk = events_response
                .chunk()
                .await
                .expect("event chunk read")
                .expect("event chunk");
            event_text.push_str(&String::from_utf8_lossy(&chunk));
        }
        event_text
    })
    .await
    .expect("event chunk timeout");
    assert!(event_text.contains(r#""type":"reset""#));
    assert!(event_text.contains(r#""reason":"restored_from_snapshot""#));
    assert!(event_text.contains("Restored from snapshot"));

    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            restored.base_url()
        ))
        .json(&json!({"role": "controller"}))
        .send()
        .await
        .expect("subscribe response")
        .json()
        .await
        .expect("subscribe json");
    assert_eq!(subscription["controller_lease_granted"], true);
    assert_eq!(subscription["snapshot"]["session_id"], "sess_test");

    restored.shutdown().await;
}

#[test]
fn snapshot_manifest_parser_accepts_typescript_hosted_shape() {
    let workspace = tempdir().expect("workspace");
    let readme_path = workspace.path().join("README.md");
    std::fs::write(&readme_path, "# workspace\n").expect("workspace file");
    let runtime = json!({
            "flush_status": "completed",
            "session_id": "session_ts",
            "session_file": workspace.path().join(".maestro/sessions/session.jsonl"),
            "protocol_version": HEADLESS_PROTOCOL_VERSION,
            "cursor": 7
    });
    let workspace_export = json!({
            "mode": "local_path_contract",
            "paths": [{
                "input": "README.md",
                "path": readme_path,
                "relative_path": "README.md",
                "type": "file"
            }]
    });
    let work_continuity = json!({
            "protocol_version": HOSTED_RUNNER_WORK_CONTINUITY_VERSION,
            "active_tool_count": 1,
            "tracked_tool_count": 1,
            "pending_request_count": 0,
            "codex_subagent_tool_call_ids": ["collab-spawn-ts"],
            "codex_subagent_child_run_ids": ["agent-run-child-ts"],
            "codex_subagent_thread_ids": ["child-thread-ts"]
    });
    let platform_work_continuity = json!({
            "protocol_version": HOSTED_RUNNER_WORK_CONTINUITY_VERSION,
            "active_tool_count": 1,
            "tracked_tool_count": 1,
            "pending_request_count": 0,
            "codex_subagent_tool_call_count": 1,
            "codex_subagent_child_run_count": 1,
            "codex_subagent_thread_count": 1,
            "codex_subagent_edge_count": 0,
            "codex_subagent_tool_call_ids": ["collab-spawn-ts"],
            "codex_subagent_child_run_ids": ["agent-run-child-ts"],
            "codex_subagent_thread_ids": ["child-thread-ts"]
    });
    let retention = json!({
            "policy_version": HOSTED_RUNNER_RETENTION_POLICY_VERSION,
            "control_plane_metadata_visibility": "operator",
            "runtime_snapshot_visibility": "internal",
            "redaction_required_before_external_persistence": [
                "runtime_snapshot",
                "runtime_logs"
            ]
    });
    let platform_evidence = json!({
            "protocol_version": HOSTED_RUNNER_PLATFORM_EVIDENCE_VERSION,
            "event_type": "hosted_runner_drain_manifest_recorded",
            "runner_session_id": "mrs_ts",
            "workspace_id": "ws_ts",
            "agent_run_id": "run_ts",
            "maestro_session_id": "session_ts",
            "status": "drained",
            "runtime_flush_status": "completed",
            "manifest_path": workspace.path().join(".maestro/runner-snapshots/mrs_ts.json"),
            "manifest_protocol_version": HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION,
            "created_at": "2026-04-23T00:00:00.000Z",
            "reason": "ttl_expired",
            "requested_by": "platform",
            "work_continuity": platform_work_continuity,
            "retention": retention,
            "evidence_refs": [
                "remote-runner://sessions/mrs_ts/drain#manifest",
                "maestro://headless/sessions/session_ts#drain",
                "platform-agent-run:run_ts"
            ]
    });
    let retention_policy = json!({
            "policy_version": HOSTED_RUNNER_RETENTION_POLICY_VERSION,
            "managed_by": "platform",
            "visibility": {
                "control_plane_metadata": "operator",
                "workspace_export": "tenant",
                "runtime_snapshot": "internal",
                "runtime_logs": "operator"
            },
            "redaction": {
                "required_before_external_persistence": [
                    "runtime_snapshot",
                    "runtime_logs"
                ],
                "forbidden_plaintext": [
                    "provider_credentials",
                    "tool_secrets",
                    "attach_tokens",
                    "artifact_access_tokens",
                    "raw_environment"
                ]
            }
    });
    let snapshot = json!({
            "protocolVersion": HEADLESS_PROTOCOL_VERSION,
            "session_id": "session_ts",
            "cursor": 7,
            "last_init": null,
            "state": {
                "protocol_version": HEADLESS_PROTOCOL_VERSION,
                "connection_count": 0,
                "subscriber_count": 0,
                "connections": [],
                "model": "gpt-5.6",
                "provider": "openai",
                "session_id": "session_ts",
                "cwd": workspace.path(),
                "pending_approvals": [],
                "pending_client_tools": [],
                "pending_mcp_elicitations": [],
                "pending_user_inputs": [],
                "pending_tool_retries": [],
                "tracked_tools": [],
                "active_tools": [],
                "active_utility_commands": [],
                "active_file_watches": [],
                "last_status": "Ready",
                "is_ready": true,
                "is_responding": false
            }
    });
    let manifest = json!({
        "protocol_version": HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION,
        "runner_session_id": "mrs_ts",
        "workspace_id": "ws_ts",
        "agent_run_id": "run_ts",
        "maestro_session_id": "session_ts",
        "reason": "ttl_expired",
        "requested_by": "platform",
        "created_at": "2026-04-23T00:00:00.000Z",
        "workspace_root": workspace.path(),
        "runtime": runtime,
        "workspace_export": workspace_export,
        "work_continuity": work_continuity,
        "platform_evidence": platform_evidence,
        "retention_policy": retention_policy,
        "snapshot": snapshot
    });
    let bytes = serde_json::to_vec(&manifest).expect("manifest json");
    let parsed = parse_snapshot_manifest_bytes(&bytes, workspace.path()).expect("typed manifest");

    assert_eq!(
        parsed.protocol_version,
        HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION
    );
    assert_eq!(parsed.runtime.flush_status, RuntimeFlushStatus::Completed);
    assert_eq!(
        parsed
            .retention_policy
            .as_ref()
            .expect("retention policy")
            .policy_version,
        HOSTED_RUNNER_RETENTION_POLICY_VERSION
    );
    let work_continuity = parsed.work_continuity.as_ref().expect("work continuity");
    assert_eq!(
        work_continuity.protocol_version,
        HOSTED_RUNNER_WORK_CONTINUITY_VERSION
    );
    assert_eq!(
        work_continuity.codex_subagent_child_run_ids,
        vec!["agent-run-child-ts".to_string()]
    );
    assert_eq!(
        parsed
            .platform_evidence
            .as_ref()
            .expect("platform evidence")
            .protocol_version,
        HOSTED_RUNNER_PLATFORM_EVIDENCE_VERSION
    );
    assert_eq!(parsed.snapshot.session_id, "session_ts");
    assert_eq!(parsed.snapshot.cursor, 7);
    assert_eq!(parsed.workspace_export.paths[0].relative_path, "README.md");

    let mut matching_restore = test_config(workspace.path().to_path_buf());
    matching_restore.workspace_id = Some("ws_ts".to_string());
    matching_restore.maestro_session_id = Some("session_ts".to_string());
    matching_restore.runner_session_id = "mrs_replacement".to_string();
    parsed
        .validate_for_restore(&matching_restore)
        .expect("a replacement Runner Host session may restore the same Maestro session");

    let mut wrong_workspace = matching_restore.clone();
    wrong_workspace.workspace_id = Some("ws_other".to_string());
    let error = parsed
        .validate_for_restore(&wrong_workspace)
        .expect_err("workspace mismatch must reject restore");
    assert_eq!(error.code, HostedRunnerErrorCode::InvalidSnapshotManifest);
    assert!(error.message.contains("workspace"));

    let mut wrong_session = matching_restore;
    wrong_session.maestro_session_id = Some("session_other".to_string());
    let error = parsed
        .validate_for_restore(&wrong_session)
        .expect_err("Maestro session mismatch must reject restore");
    assert_eq!(error.code, HostedRunnerErrorCode::InvalidSnapshotManifest);
    assert!(error.message.contains("Maestro session"));
}

#[test]
fn runtime_state_snapshot_serializes_empty_codex_subagent_edges() {
    let snapshot = RuntimeSnapshot {
        protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
        session_id: "session_empty_edges".to_string(),
        cursor: 0,
        last_init: None,
        state: RuntimeStateSnapshot {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_protocol_version: None,
            client_info: None,
            capabilities: None,
            server_capabilities: None,
            opt_out_notifications: None,
            connection_role: None,
            connection_count: 0,
            subscriber_count: 0,
            controller_subscription_id: None,
            controller_connection_id: None,
            connections: Vec::new(),
            model: Some("gpt-5.6".to_string()),
            provider: Some("rust".to_string()),
            session_id: Some("session_empty_edges".to_string()),
            cwd: None,
            git_branch: None,
            current_response: None,
            pending_approvals: Vec::new(),
            pending_client_tools: Vec::new(),
            governed_client_tool_bindings: HashMap::new(),
            pending_mcp_elicitations: Vec::new(),
            pending_user_inputs: Vec::new(),
            pending_tool_retries: Vec::new(),
            tracked_tools: Vec::new(),
            active_tools: Vec::new(),
            codex_subagent_edges: Vec::new(),
            active_utility_commands: Vec::new(),
            active_file_watches: Vec::new(),
            last_error: None,
            provider_error_kind: None,
            last_error_type: None,
            last_status: Some("Ready".to_string()),
            last_response_duration_ms: None,
            last_ttft_ms: None,
            is_ready: true,
            is_responding: false,
        },
    };

    let value = serde_json::to_value(&snapshot).expect("snapshot json");
    let empty_edges = json!([]);

    assert_eq!(
        value.pointer("/state/codex_subagent_edges"),
        Some(&empty_edges)
    );
}

#[test]
fn work_continuity_manifest_extracts_codex_subagent_ids() {
    let snapshot: RuntimeSnapshot = serde_json::from_value(json!({
        "protocolVersion": HEADLESS_PROTOCOL_VERSION,
        "session_id": "session_rust",
        "cursor": 9,
        "last_init": null,
        "state": {
            "protocol_version": HEADLESS_PROTOCOL_VERSION,
            "connection_count": 0,
            "subscriber_count": 0,
            "connections": [],
            "model": "gpt-5.6",
            "provider": "rust",
            "session_id": "session_rust",
            "pending_approvals": [{
                "id": "approval-rust",
                "call_id": "approval-call-rust",
                "tool": "shell"
            }],
            "pending_client_tools": [],
            "pending_mcp_elicitations": [],
            "pending_user_inputs": [{
                "id": "input-rust",
                "prompt": "continue?"
            }],
            "pending_tool_retries": [],
                "tracked_tools": [{
                    "call_id": "collab-spawn-rust",
                    "tool_execution_id": "texec-collab-spawn-rust",
                    "tool": "codex.subagent.spawnAgent",
                "args": {
                    "prompt": "Sensitive Rust subagent prompt",
                    "codex_work_graph": {
                        "schema_version": "evalops.maestro.codex.subagent-workgraph.v1",
                        "child_runs": [{
                            "edge_id": "collab-spawn-rust:0:spawnAgent:agent-run-child-rust",
                            "target_index": 0,
                            "thread_id": "child-thread-rust",
                            "child_run_id": "agent-run-child-rust",
                            "status": "running"
                        }]
                    }
                }
            }],
            "active_tools": [{
                "call_id": "collab-spawn-rust",
                "tool": "codex.subagent.spawnAgent",
                "output": "starting child"
            }],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    }))
    .expect("runtime snapshot");

    let continuity = default_work_continuity_manifest(&snapshot);

    assert_eq!(
        continuity.protocol_version,
        HOSTED_RUNNER_WORK_CONTINUITY_VERSION
    );
    assert_eq!(continuity.active_tool_count, 1);
    assert_eq!(continuity.tracked_tool_count, 1);
    assert_eq!(continuity.pending_request_count, 2);
    assert_eq!(
        continuity.codex_subagent_tool_call_ids,
        vec!["collab-spawn-rust".to_string()]
    );
    assert_eq!(
        continuity.codex_subagent_child_run_ids,
        vec!["agent-run-child-rust".to_string()]
    );
    assert_eq!(
        continuity.codex_subagent_thread_ids,
        vec!["child-thread-rust".to_string()]
    );
    assert_eq!(
        continuity.codex_subagent_edges,
        vec![CodexSubagentContinuityEdge {
            spawn_tool_call_id: Some("collab-spawn-rust".to_string()),
            spawn_tool_execution_id: Some("texec-collab-spawn-rust".to_string()),
            wait_tool_call_id: None,
            wait_tool_execution_id: None,
            child_run_id: Some("agent-run-child-rust".to_string()),
            thread_id: Some("child-thread-rust".to_string()),
            operation: "spawn_agent".to_string(),
            status: "running".to_string(),
        }]
    );
    let continuity_json = serde_json::to_string(&continuity).expect("continuity json");
    assert!(!continuity_json.contains("Sensitive Rust subagent prompt"));
}

#[test]
fn work_continuity_manifest_preserves_restored_codex_subagent_edges() {
    let snapshot: RuntimeSnapshot = serde_json::from_value(json!({
        "protocolVersion": HEADLESS_PROTOCOL_VERSION,
        "session_id": "session_rust_restored",
        "cursor": 10,
        "last_init": null,
        "state": {
            "protocol_version": HEADLESS_PROTOCOL_VERSION,
            "connection_count": 0,
            "subscriber_count": 0,
            "connections": [],
            "model": "gpt-5.6",
            "provider": "rust",
            "session_id": "session_rust_restored",
            "pending_approvals": [],
            "pending_client_tools": [],
            "pending_mcp_elicitations": [],
            "pending_user_inputs": [],
            "pending_tool_retries": [],
            "tracked_tools": [],
            "active_tools": [],
            "codex_subagent_edges": [
                {
                    "spawn_tool_call_id": "collab-spawn-rust-restored",
                    "child_run_id": "agent-run-child-rust-restored",
                    "thread_id": "child-thread-rust-restored",
                    "operation": "spawn_agent",
                    "status": "completed"
                },
                {
                    "wait_tool_call_id": "collab-close-rust-restored",
                    "child_run_id": "agent-run-child-rust-restored",
                    "thread_id": "child-thread-rust-restored",
                    "operation": "close_agent",
                    "status": "closed"
                }
            ],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    }))
    .expect("runtime snapshot");

    let continuity = default_work_continuity_manifest(&snapshot);

    assert_eq!(continuity.active_tool_count, 0);
    assert_eq!(continuity.tracked_tool_count, 2);
    assert_eq!(continuity.pending_request_count, 0);
    assert_eq!(
        continuity.codex_subagent_tool_call_ids,
        vec![
            "collab-close-rust-restored".to_string(),
            "collab-spawn-rust-restored".to_string(),
        ]
    );
    assert_eq!(
        continuity.codex_subagent_child_run_ids,
        vec!["agent-run-child-rust-restored".to_string()]
    );
    assert_eq!(
        continuity.codex_subagent_thread_ids,
        vec!["child-thread-rust-restored".to_string()]
    );
    assert_eq!(
        continuity.codex_subagent_edges,
        vec![
            CodexSubagentContinuityEdge {
                spawn_tool_call_id: None,
                spawn_tool_execution_id: None,
                wait_tool_call_id: Some("collab-close-rust-restored".to_string()),
                wait_tool_execution_id: None,
                child_run_id: Some("agent-run-child-rust-restored".to_string()),
                thread_id: Some("child-thread-rust-restored".to_string()),
                operation: "close_agent".to_string(),
                status: "closed".to_string(),
            },
            CodexSubagentContinuityEdge {
                spawn_tool_call_id: Some("collab-spawn-rust-restored".to_string()),
                spawn_tool_execution_id: None,
                wait_tool_call_id: None,
                wait_tool_execution_id: None,
                child_run_id: Some("agent-run-child-rust-restored".to_string()),
                thread_id: Some("child-thread-rust-restored".to_string()),
                operation: "spawn_agent".to_string(),
                status: "completed".to_string(),
            },
        ]
    );
}

#[test]
fn work_continuity_manifest_counts_mixed_codex_and_regular_tools() {
    let snapshot: RuntimeSnapshot = serde_json::from_value(json!({
        "protocolVersion": HEADLESS_PROTOCOL_VERSION,
        "session_id": "session_rust_mixed",
        "cursor": 11,
        "last_init": null,
        "state": {
            "protocol_version": HEADLESS_PROTOCOL_VERSION,
            "connection_count": 0,
            "subscriber_count": 0,
            "connections": [],
            "model": "gpt-5.6",
            "provider": "rust",
            "session_id": "session_rust_mixed",
            "pending_approvals": [],
            "pending_client_tools": [],
            "pending_mcp_elicitations": [],
            "pending_user_inputs": [],
            "pending_tool_retries": [],
            "tracked_tools": [{
                "call_id": "regular-tracked-rust",
                "tool": "shell.exec",
                "args": {
                    "command": "pwd"
                }
            }],
            "active_tools": [{
                "call_id": "regular-active-rust",
                "tool": "shell.exec",
                "output": "running"
            }],
            "codex_subagent_edges": [
                {
                    "spawn_tool_call_id": "collab-spawn-mixed",
                    "child_run_id": "agent-run-child-mixed",
                    "thread_id": "child-thread-mixed",
                    "operation": "spawn_agent",
                    "status": "completed"
                },
                {
                    "wait_tool_call_id": "collab-wait-mixed",
                    "child_run_id": "agent-run-child-mixed",
                    "thread_id": "child-thread-mixed",
                    "operation": "wait_agent",
                    "status": "wait_pending"
                }
            ],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    }))
    .expect("runtime snapshot");

    let continuity = default_work_continuity_manifest(&snapshot);

    assert_eq!(continuity.active_tool_count, 2);
    assert_eq!(continuity.tracked_tool_count, 3);
    assert_eq!(
        continuity.codex_subagent_tool_call_ids,
        vec![
            "collab-spawn-mixed".to_string(),
            "collab-wait-mixed".to_string()
        ]
    );
}

#[test]
fn work_continuity_manifest_keeps_spawned_and_resumed_codex_subagents_active() {
    let snapshot: RuntimeSnapshot = serde_json::from_value(json!({
        "protocolVersion": HEADLESS_PROTOCOL_VERSION,
        "session_id": "session_rust_active_subagents",
        "cursor": 12,
        "last_init": null,
        "state": {
            "protocol_version": HEADLESS_PROTOCOL_VERSION,
            "connection_count": 0,
            "subscriber_count": 0,
            "connections": [],
            "model": "gpt-5.6",
            "provider": "rust",
            "session_id": "session_rust_active_subagents",
            "pending_approvals": [],
            "pending_client_tools": [],
            "pending_mcp_elicitations": [],
            "pending_user_inputs": [],
            "pending_tool_retries": [],
            "tracked_tools": [],
            "active_tools": [],
            "codex_subagent_edges": [
                {
                    "spawn_tool_call_id": "collab-spawn-active",
                    "child_run_id": "agent-run-child-spawned",
                    "thread_id": "child-thread-spawned",
                    "operation": "spawn_agent",
                    "status": "spawned"
                },
                {
                    "wait_tool_call_id": "collab-send-ack",
                    "child_run_id": "agent-run-child-ack",
                    "thread_id": "child-thread-ack",
                    "operation": "send_input",
                    "status": "acknowledged"
                },
                {
                    "wait_tool_call_id": "collab-resume-active",
                    "child_run_id": "agent-run-child-resumed",
                    "thread_id": "child-thread-resumed",
                    "operation": "resume_agent",
                    "status": "resumed"
                }
            ],
            "active_utility_commands": [],
            "active_file_watches": [],
            "is_ready": true,
            "is_responding": false
        }
    }))
    .expect("runtime snapshot");

    let continuity = default_work_continuity_manifest(&snapshot);

    assert_eq!(continuity.active_tool_count, 2);
    assert_eq!(continuity.tracked_tool_count, 3);
    assert_eq!(
        continuity.codex_subagent_tool_call_ids,
        vec![
            "collab-resume-active".to_string(),
            "collab-send-ack".to_string(),
            "collab-spawn-active".to_string()
        ]
    );
}

#[tokio::test]
async fn failed_restore_manifest_stays_not_ready_and_rejects_attach() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_partial_restore",
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    assert_eq!(connection["connection_id"], "conn_partial_restore");

    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_partial_restore",
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let message: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_partial_restore")
        .header(
            "x-maestro-headless-subscriber-id",
            subscription["subscription_id"]
                .as_str()
                .expect("subscription id"),
        )
        .header(
            "x-maestro-headless-connection-capability",
            subscription["connection_capability"]
                .as_str()
                .expect("connection capability"),
        )
        .json(&json!({"type": "prompt", "content": "before interrupted restore"}))
        .send()
        .await
        .expect("message response")
        .json()
        .await
        .expect("message json");
    assert!(message["cursor"].as_u64().unwrap_or_default() > 0);

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "preempted"}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    let manifest_path = PathBuf::from(drain["manifest_path"].as_str().expect("manifest path"));
    let restored_cursor = drain["manifest"]["runtime"]["cursor"]
        .as_u64()
        .expect("manifest cursor");
    let mut partial_manifest = drain["manifest"].clone();
    partial_manifest["runtime"]["flush_status"] = json!("failed");
    partial_manifest["runtime"]["error"] = json!("flush timed out");
    partial_manifest["snapshot"]["state"]["current_response"] = json!({
        "response_id": "restore-response",
        "text": "sensitive restored response"
    });
    partial_manifest["snapshot"]["state"]["pending_approvals"] = json!([{
        "call_id": "restore-approval-call",
        "request_id": "restore-approval",
        "tool": "bash",
        "args": {"cmd": "cat secret.txt"},
        "started_at_ms": 1_771_000_001_000u64
    }]);
    partial_manifest["snapshot"]["state"]["pending_user_inputs"] = json!([{
        "call_id": "restore-input-call",
        "request_id": "restore-input",
        "tool": "user_input",
        "args": {"prompt": "enter secret"},
        "started_at_ms": 1_771_000_001_100u64
    }]);
    partial_manifest["snapshot"]["state"]["pending_tool_retries"] = json!([{
        "call_id": "restore-retry-call",
        "request_id": "restore-retry",
        "tool": "bash",
        "args": {"stderr": "token in error"},
        "started_at_ms": 1_771_000_001_200u64
    }]);
    partial_manifest["snapshot"]["state"]["active_tools"] = json!([{
        "call_id": "restore-active-call",
        "tool": "bash",
        "output": "secret active output"
    }]);
    tokio::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&partial_manifest).expect("manifest json"),
    )
    .await
    .expect("write partial manifest");
    handle.shutdown().await;

    let mut restore_config = test_config(workspace.path().to_path_buf());
    restore_config.runner_session_id = "mrs_partial_restored".to_string();
    restore_config.maestro_session_id = None;
    restore_config.restore_manifest_path = Some(manifest_path.clone());
    let restored = start_hosted_runner_with_message_executor(
        restore_config,
        Arc::new(StatefulRuntimeExecutor::new(AgentState::default())),
    )
    .await
    .expect("start restored hosted runner");

    let identity: HostedRunnerIdentity = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            restored.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert_eq!(identity.runner_session_id, "mrs_partial_restored");
    assert!(!identity.ready);
    assert!(!identity.draining);

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            restored.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["session_id"], "sess_test");
    assert_eq!(state["cursor"], restored_cursor);
    assert_eq!(
        state["state"]["last_status"],
        "Restore interrupted before runtime flush completed"
    );
    assert_eq!(state["state"]["last_error"], "flush timed out");
    assert_eq!(state["state"]["last_error_type"], "protocol");
    assert_eq!(state["state"]["is_ready"], false);
    assert_eq!(state["runtime_receipt"]["kind"], "failed");
    assert_eq!(state["runtime_receipt"]["lifecycleState"], "failed");
    assert_eq!(state["runtime_receipt"]["errorType"], "restore_incomplete");

    // A child may still emit Ready after an incomplete restore. That event is
    // not evidence that the failed restore flush became durable; the
    // generation must retain its not-ready receipt and fence.
    {
        let mut state = restored.shared.state.lock().expect("restored state");
        restored.shared.publish_message(
            &mut state,
            FromAgentMessage::Ready {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                model: "late-ready-model".to_string(),
                provider: "late-ready-provider".to_string(),
                session_id: None,
            },
        );
        assert!(state.restore_incomplete);
        assert!(!state.ready);
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Failed)
        );
    }
    assert!(state["state"]["current_response"].is_null());
    assert_eq!(
        state["state"]["pending_approvals"][0]["call_id"],
        "restore-approval-call"
    );
    assert_eq!(
        state["state"]["pending_approvals"][0]["request_id"],
        "restore-approval"
    );
    assert_eq!(state["state"]["pending_approvals"][0]["args"], json!({}));
    assert_eq!(
        state["state"]["pending_user_inputs"][0]["call_id"],
        "restore-input-call"
    );
    assert_eq!(
        state["state"]["pending_user_inputs"][0]["request_id"],
        "restore-input"
    );
    assert_eq!(state["state"]["pending_user_inputs"][0]["args"], json!({}));
    assert_eq!(
        state["state"]["pending_tool_retries"][0]["call_id"],
        "restore-retry-call"
    );
    assert_eq!(
        state["state"]["pending_tool_retries"][0]["request_id"],
        "restore-retry"
    );
    assert_eq!(state["state"]["pending_tool_retries"][0]["args"], json!({}));
    assert_eq!(
        state["state"]["active_tools"][0]["call_id"],
        "restore-active-call"
    );
    assert_eq!(state["state"]["active_tools"][0]["output"], "");

    let ready = client
        .get(format!("{}/readyz", restored.base_url()))
        .send()
        .await
        .expect("ready response");
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);

    let attach = client
        .post(format!("{}/api/headless/connections", restored.base_url()))
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("attach response");
    assert_eq!(attach.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut events_response = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0",
            restored.base_url()
        ))
        .send()
        .await
        .expect("events response");
    assert_eq!(events_response.status(), StatusCode::OK);
    let event_text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut event_text = String::new();
        while !event_text.contains(r#""reason":"restored_from_snapshot""#) {
            let chunk = events_response
                .chunk()
                .await
                .expect("event chunk read")
                .expect("event chunk");
            event_text.push_str(&String::from_utf8_lossy(&chunk));
        }
        event_text
    })
    .await
    .expect("event chunk timeout");
    assert!(event_text.contains(r#""type":"reset""#));
    assert!(event_text.contains("Restore interrupted before runtime flush completed"));

    drop(events_response);
    restored.shutdown().await;
}

#[tokio::test]
async fn failed_restore_boundary_survives_restart_without_manifest() {
    let workspace = tempdir().expect("workspace");
    let source = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let response = handle_drain(
        source.clone(),
        DrainRequest {
            reason: Some("failed restore restart fixture".to_string()),
            requested_by: Some("hosted runner test".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("drain source runner");
    let ResponseBody::Json { body, .. } = response else {
        panic!("drain should return a manifest JSON response");
    };
    let manifest_path = PathBuf::from(body["manifest_path"].as_str().expect("manifest path"));
    let mut manifest = body["manifest"].clone();
    manifest["runtime"]["flush_status"] = json!("failed");
    manifest["runtime"]["error"] = json!("flush timed out");
    tokio::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("failed restore manifest JSON"),
    )
    .await
    .expect("write failed restore manifest");
    drop(source);

    let restore_manifest = serde_json::from_value::<SnapshotManifest>(manifest)
        .expect("failed restore manifest fixture");
    let mut restore_config = test_config(workspace.path().to_path_buf());
    restore_config.runner_session_id = "mrs_failed_restore_restart".to_string();
    restore_config.maestro_session_id = None;
    let first = SharedRunner::new_with_message_executor_and_restore(
        restore_config,
        Arc::new(TransportOnlyHostedRunnerMessageExecutor),
        Some(restore_manifest),
    );
    assert_eq!(
        first.runtime_receipt().as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Failed)
    );
    drop(first);

    let mut restart_config = test_config(workspace.path().to_path_buf());
    restart_config.runner_session_id = "mrs_failed_restore_restart".to_string();
    restart_config.maestro_session_id = Some("sess_test".to_string());
    let restarted = SharedRunner::new(restart_config);
    let state = restarted.state.lock().expect("restarted state");
    assert!(!state.ready);
    assert!(state.runtime_failed);
    assert_eq!(state.last_status.as_deref(), Some("Runtime failed"));
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Failed)
    );
    assert_eq!(
        state
            .runtime_receipt
            .as_ref()
            .and_then(|receipt| receipt.error_type.as_deref()),
        Some("restore_incomplete")
    );
}

#[tokio::test]
async fn completed_restore_failure_preserves_snapshot_error_category() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let drain = handle
        .drain_for_shutdown("completed failed restore fixture", "hosted runner test")
        .await
        .expect("drain hosted runner");
    let manifest_path = PathBuf::from(drain["manifest_path"].as_str().expect("manifest path"));
    handle.shutdown().await;

    let mut manifest = drain["manifest"].clone();
    manifest["runtime"]["flush_status"] = json!("completed");
    manifest["runtime"]["error"] = serde_json::Value::Null;
    manifest["snapshot"]["state"]["last_status"] = json!("Runtime failed");
    manifest["snapshot"]["state"]["last_error"] = json!("provider rejected the restored turn");
    manifest["snapshot"]["state"]["last_error_type"] = json!("provider");
    tokio::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )
    .await
    .expect("write completed failed manifest");

    let mut config = test_config(workspace.path().to_path_buf());
    config.runner_session_id = "mrs_completed_failed_restore".to_string();
    config.maestro_session_id = None;
    config.restore_manifest_path = Some(manifest_path);
    let restored = start_hosted_runner(config)
        .await
        .expect("start completed-failed restored hosted runner");
    let client = reqwest::Client::new();
    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            restored.base_url()
        ))
        .send()
        .await
        .expect("completed-failed state response")
        .json()
        .await
        .expect("completed-failed state json");
    assert_eq!(state["state"]["last_error_type"], "provider");
    assert_eq!(state["runtime_receipt"]["errorType"], "provider");
    assert_ne!(state["runtime_receipt"]["errorType"], "restore_incomplete");
    assert_eq!(state["runtime_receipt"]["kind"], "failed");
    restored.shutdown().await;
}

#[tokio::test]
async fn oversized_restored_receipt_metadata_fails_before_listener_bind() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let drain = handle
        .drain_for_shutdown("oversized metadata fixture", "hosted runner test")
        .await
        .expect("drain hosted runner");
    let manifest_path = PathBuf::from(drain["manifest_path"].as_str().expect("manifest path"));
    handle.shutdown().await;

    for (field, value) in [
        (
            "model",
            "m".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1),
        ),
        (
            "provider",
            "p".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1),
        ),
        (
            "last_error_type",
            "e".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES + 1),
        ),
    ] {
        let mut manifest = drain["manifest"].clone();
        manifest["runtime"]["flush_status"] = json!("completed");
        manifest["snapshot"]["state"]["last_status"] = json!("Runtime failed");
        manifest["snapshot"]["state"]["last_error"] = json!("restore failed");
        manifest["snapshot"]["state"][field] = json!(value);
        tokio::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("manifest json"),
        )
        .await
        .expect("write oversized metadata manifest");

        let mut config = test_config(workspace.path().to_path_buf());
        config.runner_session_id = format!("mrs_oversized_{field}");
        config.restore_manifest_path = Some(manifest_path.clone());
        let error = match prepare_hosted_runner(config).await {
            Ok(_) => panic!("oversized restored {field} must fail before listener bind"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(field));
        assert!(error.to_string().contains("runtime receipt limit"));
    }

    let mut manifest = drain["manifest"].clone();
    manifest["runtime"]["flush_status"] = json!("completed");
    manifest["snapshot"]["state"]["last_status"] = json!("Runtime failed");
    manifest["snapshot"]["state"]["last_error"] = json!("restore failed");
    manifest["snapshot"]["state"]["last_error_type"] = json!(" \t ");
    tokio::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("whitespace error type manifest json"),
    )
    .await
    .expect("write whitespace error type manifest");
    let mut config = test_config(workspace.path().to_path_buf());
    config.runner_session_id = "mrs_whitespace_error_type".to_string();
    config.restore_manifest_path = Some(manifest_path.clone());
    let error = match prepare_hosted_runner(config).await {
        Ok(_) => panic!("whitespace restored last_error_type must fail before listener bind"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("last_error_type"));
    assert!(error.to_string().contains("must not be empty"));

    let mut manifest = drain["manifest"].clone();
    manifest["created_at"] =
        json!("created-at-".repeat(maestro_runtime::MAX_RUNTIME_RECEIPT_STRING_BYTES));
    tokio::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("oversized lineage json"),
    )
    .await
    .expect("write oversized lineage manifest");
    let mut config = test_config(workspace.path().to_path_buf());
    config.runner_session_id = "mrs_oversized_lineage".to_string();
    config.restore_manifest_path = Some(manifest_path);
    let error = match prepare_hosted_runner(config).await {
        Ok(_) => panic!("oversized restored snapshot lineage must fail before listener bind"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("snapshot_lineage"));
    assert!(error.to_string().contains("runtime receipt limit"));
}

#[tokio::test]
async fn skipped_restore_manifest_stays_not_ready() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "empty-runtime"}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    assert_eq!(drain["manifest"]["runtime"]["flush_status"], "skipped");
    let manifest_path = PathBuf::from(drain["manifest_path"].as_str().expect("manifest path"));
    handle.shutdown().await;

    let mut restore_config = test_config(workspace.path().to_path_buf());
    restore_config.runner_session_id = "mrs_skipped_restored".to_string();
    restore_config.maestro_session_id = None;
    restore_config.restore_manifest_path = Some(manifest_path);
    let restored = start_hosted_runner(restore_config)
        .await
        .expect("start restored hosted runner");

    let identity: HostedRunnerIdentity = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            restored.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert!(!identity.ready);

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            restored.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(
        state["state"]["last_status"],
        "Restore incomplete: runtime flush skipped"
    );
    assert_eq!(
        state["state"]["last_error"],
        "runtime flush was skipped; no runtime activity was persisted"
    );
    assert_eq!(state["state"]["last_error_type"], "protocol");
    assert_eq!(state["state"]["is_ready"], false);
    assert_eq!(state["runtime_receipt"]["kind"], "failed");
    assert_eq!(state["runtime_receipt"]["lifecycleState"], "failed");
    assert_eq!(state["runtime_receipt"]["errorType"], "restore_incomplete");

    restored.shutdown().await;
}

#[tokio::test]
async fn message_executor_publishes_runtime_handled_events() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_exec",
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    assert_eq!(connection["connection_id"], "conn_exec");

    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_exec",
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "subscriptionId": "sub_exec",
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id")
        .to_string();
    let connection_capability = subscription["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();
    let viewer: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_off",
            "role": "viewer",
            "capabilities": {"transcriptGrade": "off"}
        }))
        .send()
        .await
        .expect("viewer connection response")
        .json()
        .await
        .expect("viewer connection json");
    assert_eq!(viewer["connection_id"], "conn_off");
    let viewer_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_off",
            "connectionCapability": viewer["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "viewer",
            "capabilities": {"transcriptGrade": "off"}
        }))
        .send()
        .await
        .expect("viewer subscription response")
        .json()
        .await
        .expect("viewer subscription json");
    let viewer_subscription_id = viewer_subscription["subscription_id"]
        .as_str()
        .expect("viewer subscription id")
        .to_string();

    let message: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_exec")
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            &connection_capability,
        )
        .json(&json!({"type": "prompt", "content": "hello"}))
        .send()
        .await
        .expect("message response")
        .json()
        .await
        .expect("message json");
    assert_eq!(message["success"], true);
    assert_eq!(message["execution"], "runtime_handled");
    assert_eq!(message["published_messages"], 3);

    let mut events_response = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={subscription_id}",
            handle.base_url(),
        ))
        .send()
        .await
        .expect("events response");
    assert_eq!(events_response.status(), StatusCode::OK);
    let mut event_text = String::new();
    for _ in 0..8 {
        let chunk =
            tokio::time::timeout(std::time::Duration::from_secs(1), events_response.chunk())
                .await
                .expect("event chunk timeout")
                .expect("event chunk read");
        let Some(chunk) = chunk else {
            break;
        };
        event_text.push_str(&String::from_utf8_lossy(&chunk));
        if event_text.contains("\"type\":\"response_end\"") {
            break;
        }
    }
    assert!(event_text.contains("\"type\":\"response_start\""));
    assert!(event_text.contains("\"type\":\"response_chunk\""));
    assert!(event_text.contains("\"content\":\"runtime: hello\""));
    assert!(event_text.contains("\"type\":\"response_end\""));

    let mut viewer_events_response = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={viewer_subscription_id}",
            handle.base_url(),
        ))
        .send()
        .await
        .expect("viewer events response");
    assert_eq!(viewer_events_response.status(), StatusCode::OK);
    let mut viewer_event_text = String::new();
    for _ in 0..8 {
        let chunk = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            viewer_events_response.chunk(),
        )
        .await
        .expect("viewer event chunk timeout")
        .expect("viewer event chunk read");
        let Some(chunk) = chunk else {
            break;
        };
        viewer_event_text.push_str(&String::from_utf8_lossy(&chunk));
        if viewer_event_text.contains("\"type\":\"response_end\"") {
            break;
        }
    }
    assert!(viewer_event_text.contains("\"type\":\"response_start\""));
    assert!(viewer_event_text.contains("\"type\":\"response_end\""));
    assert!(!viewer_event_text.contains("\"type\":\"response_chunk\""));
    assert!(!viewer_event_text.contains("\"content\":\"runtime: hello\""));

    handle.shutdown().await;
}

async fn next_sse_message_type(response: &mut reqwest::Response, buffered: &mut String) -> String {
    next_sse_message_type_with_timeout(response, buffered, Duration::from_millis(250)).await
}

async fn next_sse_message_type_with_timeout(
    response: &mut reqwest::Response,
    buffered: &mut String,
    timeout: Duration,
) -> String {
    loop {
        if let Some(event_end) = buffered.find("\n\n") {
            let event = buffered[..event_end].to_string();
            buffered.drain(..event_end + 2);
            let payload = event.strip_prefix("data: ").expect("SSE data event");
            let envelope: serde_json::Value = serde_json::from_str(payload).expect("SSE envelope");
            return envelope["message"]["type"]
                .as_str()
                .expect("agent event type")
                .to_string();
        }

        let chunk = tokio::time::timeout(timeout, response.chunk())
            .await
            .expect("response event should arrive without another HTTP request")
            .expect("event chunk read")
            .expect("event stream remained open");
        buffered.push_str(&String::from_utf8_lossy(&chunk));
    }
}

async fn wait_for_condition(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !condition() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition should become true");
}

#[tokio::test]
async fn transport_notification_publishes_ready_before_maintenance_tick() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(NotifyingPumpRuntimeExecutor::default());
    let handle = start_hosted_runner_with_message_executor_and_maintenance_interval(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
        Duration::from_secs(5),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    tokio::time::sleep(Duration::from_millis(10)).await;

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_event_notification",
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    let connection_capability = connection["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();
    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_event_notification",
            "connectionCapability": connection_capability,
            "connectionCapabilityRequired": true,
            "subscriptionId": "sub_event_notification",
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id");

    let mut events = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={subscription_id}",
            handle.base_url()
        ))
        .send()
        .await
        .expect("events response");
    assert_eq!(events.status(), StatusCode::OK);
    let mut buffered = String::new();
    assert_eq!(
        next_sse_message_type(&mut events, &mut buffered).await,
        "connection_info"
    );

    executor.queue_ready_event();
    let ready = tokio::time::timeout(
        Duration::from_secs(1),
        next_sse_message_type_with_timeout(&mut events, &mut buffered, Duration::from_secs(1)),
    )
    .await
    .expect("ready event should not wait for the 5s maintenance tick");
    assert_eq!(ready, "ready");

    handle.shutdown().await;
}

#[tokio::test]
async fn event_pump_publishes_agent_events_without_a_follow_up_http_request() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(PumpOnlyRuntimeExecutor::default()),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_event_pump",
            "role": "controller"
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    let connection_capability = connection["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();
    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_event_pump",
            "connectionCapability": connection_capability,
            "connectionCapabilityRequired": true,
            "subscriptionId": "sub_event_pump",
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id");

    let mut events = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={subscription_id}",
            handle.base_url()
        ))
        .send()
        .await
        .expect("events response");
    assert_eq!(events.status(), StatusCode::OK);
    let mut buffered = String::new();
    assert_eq!(
        next_sse_message_type(&mut events, &mut buffered).await,
        "connection_info"
    );

    let prompt: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_event_pump")
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            connection["connection_capability"]
                .as_str()
                .expect("connection capability"),
        )
        .json(&json!({"type": "prompt", "content": "remember this"}))
        .send()
        .await
        .expect("prompt response")
        .json()
        .await
        .expect("prompt json");
    assert_eq!(prompt["published_messages"], 0);

    let (response_start, response_chunk, response_end) =
        tokio::time::timeout(Duration::from_secs(1), async {
            (
                next_sse_message_type_with_timeout(
                    &mut events,
                    &mut buffered,
                    Duration::from_secs(1),
                )
                .await,
                next_sse_message_type_with_timeout(
                    &mut events,
                    &mut buffered,
                    Duration::from_secs(1),
                )
                .await,
                next_sse_message_type_with_timeout(
                    &mut events,
                    &mut buffered,
                    Duration::from_secs(1),
                )
                .await,
            )
        })
        .await
        .expect("response event pump should publish all events without another HTTP request");
    assert_eq!(response_start, "response_start");
    assert_eq!(response_chunk, "response_chunk");
    assert_eq!(response_end, "response_end");

    handle.shutdown().await;
}

async fn start_pump_race_runner() -> (HostedRunnerHandle, tempfile::TempDir) {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(PumpOnlyRuntimeExecutor::default());
    executor
        .queued
        .lock()
        .expect("queued messages")
        .push(FromAgentMessage::ResponseStart {
            response_id: "response-race".to_string(),
        });
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor,
    )
    .await
    .expect("start hosted runner");
    (handle, workspace)
}

#[tokio::test]
async fn drain_stops_event_pump_before_snapshotting() {
    let (handle, _workspace) = start_pump_race_runner().await;
    let shared = handle.shared.clone();

    let manifest = handle
        .drain_for_shutdown("test drain", "hosted runner test")
        .await
        .expect("drain hosted runner");

    assert_eq!(
        manifest["manifest"]["snapshot"]["cursor"],
        shared.last_published_cursor()
    );
    assert!(shared.event_pump_is_finished());
    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_awaits_event_pump() {
    let (handle, _workspace) = start_pump_race_runner().await;
    let shared = handle.shared.clone();

    handle.shutdown().await;

    assert!(shared.event_pump_is_finished());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_fences_in_flight_prompts_and_serializes_repeated_drains() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(BlockedPromptRuntimeExecutor::new());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    {
        let mut state = handle.shared.state.lock().expect("runner state");
        state.connections.insert(
            "conn_drain_fence".to_string(),
            ConnectionRecord {
                id: "conn_drain_fence".to_string(),
                connection_capability: None,
                authority_mode: ConnectionAuthorityMode::LegacySubscription,
                role: ConnectionRole::Controller,
                client_protocol_version: None,
                client_info: None,
                capabilities: None,
                opt_out_notifications: Vec::new(),
                subscription_ids: HashSet::from(["sub_drain_fence".to_string()]),
                last_seen_at: Utc::now(),
            },
        );
        state.subscriptions.insert(
            "sub_drain_fence".to_string(),
            SubscriptionRecord {
                connection_id: "conn_drain_fence".to_string(),
                connection_capability: None,
                authority_mode: ConnectionAuthorityMode::LegacySubscription,
                role: ConnectionRole::Controller,
                attached: true,
            },
        );
        state.controller_connection_id = Some("conn_drain_fence".to_string());
    }

    let prompt_shared = handle.shared.clone();
    let prompt = tokio::spawn(async move {
        handle_message(
            prompt_shared,
            "sess_test",
            HashMap::from([
                (
                    "x-maestro-headless-connection-id".to_string(),
                    "conn_drain_fence".to_string(),
                ),
                (
                    "x-maestro-headless-subscriber-id".to_string(),
                    "sub_drain_fence".to_string(),
                ),
            ]),
            ToAgentMessage::Prompt {
                content: "finish before drain".to_string(),
                attachments: None,
                managed_inference_authorization: None,
            },
        )
        .await
    });
    wait_for_condition(|| executor.started.load(Ordering::SeqCst)).await;

    let first_shared = handle.shared.clone();
    let first_drain = tokio::spawn(async move {
        handle_drain(
            first_shared,
            DrainRequest {
                reason: Some("test drain".to_string()),
                requested_by: Some("hosted runner test".to_string()),
                export_paths: Some(vec![".".to_string()]),
            },
        )
        .await
    });
    let second_shared = handle.shared.clone();
    let second_drain = tokio::spawn(async move {
        handle_drain(
            second_shared,
            DrainRequest {
                reason: Some("second test drain".to_string()),
                requested_by: Some("hosted runner test".to_string()),
                export_paths: Some(vec![".".to_string()]),
            },
        )
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    executor.release();

    let prompt = prompt.await.expect("prompt task").expect("prompt result");
    assert!(matches!(prompt, ResponseBody::Json { status: 200, .. }));
    let first = first_drain.await.expect("first drain task");
    let second = second_drain.await.expect("second drain task");
    let mut manifest = None;
    for response in [first, second].into_iter().flatten() {
        assert!(manifest.is_none(), "only one drain may compose a manifest");
        manifest = Some(response);
    }
    let first = manifest.expect("one drain manifest");

    let ResponseBody::Json { body: manifest, .. } = first else {
        panic!("drain must return a JSON manifest");
    };
    assert_eq!(
        manifest["manifest"]["snapshot"]["cursor"],
        handle.shared.last_published_cursor()
    );
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_idempotent_messages_wait_for_the_original_attempt() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(BlockedPromptRuntimeExecutor::new());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    {
        let mut state = handle.shared.state.lock().expect("runner state");
        state.connections.insert(
            "conn_idempotency_race".to_string(),
            ConnectionRecord {
                id: "conn_idempotency_race".to_string(),
                connection_capability: None,
                authority_mode: ConnectionAuthorityMode::LegacySubscription,
                role: ConnectionRole::Controller,
                client_protocol_version: None,
                client_info: None,
                capabilities: None,
                opt_out_notifications: Vec::new(),
                subscription_ids: HashSet::from(["sub_idempotency_race".to_string()]),
                last_seen_at: Utc::now(),
            },
        );
        state.subscriptions.insert(
            "sub_idempotency_race".to_string(),
            SubscriptionRecord {
                connection_id: "conn_idempotency_race".to_string(),
                connection_capability: None,
                authority_mode: ConnectionAuthorityMode::LegacySubscription,
                role: ConnectionRole::Controller,
                attached: true,
            },
        );
        state.controller_connection_id = Some("conn_idempotency_race".to_string());
    }

    let headers = || {
        HashMap::from([
            (
                "x-maestro-headless-connection-id".to_string(),
                "conn_idempotency_race".to_string(),
            ),
            (
                "x-maestro-headless-subscriber-id".to_string(),
                "sub_idempotency_race".to_string(),
            ),
            (
                "x-maestro-idempotency-key".to_string(),
                "response-race".to_string(),
            ),
        ])
    };
    let message = || ToAgentMessage::ClientToolResult {
        call_id: "response-race".to_string(),
        content: Vec::new(),
        is_error: false,
    };

    let first = tokio::spawn(handle_message(
        handle.shared.clone(),
        "sess_test",
        headers(),
        message(),
    ));
    wait_for_condition(|| executor.started.load(Ordering::SeqCst)).await;

    let second = tokio::spawn(handle_message(
        handle.shared.clone(),
        "sess_test",
        headers(),
        message(),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(
        !second.is_finished(),
        "retry entered the executor concurrently"
    );

    executor.release();
    assert!(matches!(
        first.await.expect("first task").expect("first response"),
        ResponseBody::Json { status: 200, .. }
    ));
    let ResponseBody::Json { status, body } =
        second.await.expect("second task").expect("second response")
    else {
        panic!("retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["replayed"], true);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn utility_command_completion_after_drain_does_not_mutate_snapshot_state() {
    let workspace = tempdir().expect("workspace");
    let release_path = workspace.path().join("release-utility-command");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let command_id = "utility-drain-fence".to_string();
    let utility_command_task = run_utility_command(
        handle.shared.clone(),
        UtilityCommandInvocation {
            connection_id: None,
            command_id: command_id.clone(),
            command: format!(
                "while [ ! -f {} ]; do sleep 0.01; done; echo settled",
                release_path.display(),
            ),
            cwd: None,
            env: HashMap::new(),
            shell_mode: UtilityCommandShellMode::Shell,
            terminal_mode: UtilityCommandTerminalMode::Pipe,
            columns: None,
            rows: None,
        },
    )
    .await
    .expect("start blocked utility command");
    assert!(
        handle
            .shared
            .state
            .lock()
            .expect("runner state")
            .active_utility_commands
            .contains_key(&command_id)
    );

    let manifest = handle
        .drain_for_shutdown("test drain", "hosted runner test")
        .await
        .expect("drain hosted runner");
    let cursor = handle.shared.last_published_cursor();
    let replay_count = handle
        .shared
        .state
        .lock()
        .expect("runner state")
        .envelopes
        .len();

    tokio::fs::write(&release_path, b"release")
        .await
        .expect("release utility command");
    tokio::time::timeout(std::time::Duration::from_secs(1), utility_command_task)
        .await
        .expect("utility command task should settle")
        .expect("utility command task should not panic");

    {
        let state = handle.shared.state.lock().expect("runner state");
        assert_eq!(manifest["manifest"]["snapshot"]["cursor"], cursor);
        assert_eq!(state.cursor, cursor);
        assert_eq!(state.envelopes.len(), replay_count);
        assert!(state.active_utility_commands.contains_key(&command_id));
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn event_pump_failure_marks_runtime_failed_and_rejects_admission() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(FailingPumpRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    wait_for_condition(|| !handle.shared.identity().ready).await;
    {
        let state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.runtime_failed);
        assert_eq!(state.last_status.as_deref(), Some("Runtime failed"));
        assert_eq!(state.last_error_type.as_deref(), Some("event_pump_failed"));
    }
    let client = reqwest::Client::new();

    let ready = client
        .get(format!("{}/readyz", handle.base_url()))
        .send()
        .await
        .expect("ready response");
    assert_eq!(ready.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let ready_body: serde_json::Value = ready.json().await.expect("ready body");
    assert_eq!(ready_body["code"], "runtime_failed");
    let admission = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("connection response");
    assert_eq!(admission.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let admission_body: serde_json::Value = admission.json().await.expect("admission body");
    assert_eq!(admission_body["code"], "runtime_failed");

    let (events, _) = handle.shared.subscribe_from(0);
    assert!(matches!(
        events.last(),
        Some(StreamEnvelope::Message {
            message,
            ..
        }) if matches!(message.as_ref(), FromAgentMessage::Error {
            fatal: true,
            error_type: Some(crate::headless::messages::HeadlessErrorType::Fatal),
            ..
        })
    ));
    handle.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn child_exit_after_ready_revokes_identity_and_fails_closed() {
    let workspace = tempdir().expect("workspace");
    let workspace_root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
    let child_log = workspace.path().join("child.log");
    let script = create_response_consumer_script(
        workspace.path(),
        "ready-until-disconnected.sh",
        &child_log,
        true,
        None,
    );
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace_root),
        executor,
        None,
    );

    assert!(shared.identity().ready);
    shared
        .ensure_mutation_allowed()
        .expect("live ready child admits mutations");

    supervisor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .disconnect();
    assert_eq!(pump_tick(&shared), PumpTick::Stop);
    assert!(!shared.identity().ready);

    {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.runtime_failed);
        assert_eq!(state.last_status.as_deref(), Some("Runtime failed"));
        assert_eq!(state.last_error_type.as_deref(), Some("event_pump_failed"));
        assert_eq!(
            state.last_error.as_deref(),
            Some("native headless agent disconnected after becoming ready")
        );
    }

    let admission = shared
        .ensure_mutation_allowed()
        .expect_err("dead child must reject the next mutation");
    assert_eq!(admission.code, HostedRunnerErrorCode::RuntimeFailed);

    let response = handle_drain(
        shared.clone(),
        DrainRequest {
            reason: Some("runtime-failed".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("terminal runtime must remain drainable for durable export");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    let manifest_path = body["manifest_path"].as_str().expect("drain manifest path");
    assert!(Path::new(manifest_path).is_file());
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(state.runtime_failed);
    assert!(!state.ready);
}

#[tokio::test]
async fn authenticated_current_controller_gets_byte_faithful_client_tool_arguments() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()).with_auth_token("controller-test-token"),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let authorized = || ("authorization", "Bearer controller-test-token");

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header(authorized().0, authorized().1)
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_exec",
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("controller connection response")
        .json()
        .await
        .expect("controller connection json");
    assert_eq!(connection["controller_lease_granted"], true);
    let controller_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .header(authorized().0, authorized().1)
        .json(&json!({
            "connectionId": "conn_exec",
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "subscriptionId": "sub_exec",
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("controller subscription response")
        .json()
        .await
        .expect("controller subscription json");
    let controller_subscription_id = controller_subscription["subscription_id"]
        .as_str()
        .expect("controller subscription id");
    let controller_connection_capability = controller_subscription["connection_capability"]
        .as_str()
        .expect("controller connection capability");
    assert_eq!(
        controller_subscription["controller_subscription_id"],
        controller_subscription_id
    );
    assert!(controller_subscription["snapshot"]["state"]["controller_subscription_id"].is_null());

    let viewer_connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header(authorized().0, authorized().1)
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_viewer",
            "role": "viewer",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("viewer connection response")
        .json()
        .await
        .expect("viewer connection json");
    assert_eq!(viewer_connection["controller_lease_granted"], false);
    let viewer_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .header(authorized().0, authorized().1)
        .json(&json!({
            "connectionId": "conn_viewer",
            "connectionCapability": viewer_connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "subscriptionId": "sub_viewer",
            "role": "viewer",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("viewer subscription response")
        .json()
        .await
        .expect("viewer subscription json");
    let viewer_subscription_id = viewer_subscription["subscription_id"]
        .as_str()
        .expect("viewer subscription id");
    assert_eq!(viewer_subscription["role"], "viewer");
    assert!(viewer_subscription["controller_subscription_id"].is_null());
    assert!(viewer_subscription["snapshot"]["state"]["controller_subscription_id"].is_null());
    assert_ne!(controller_subscription_id, viewer_subscription_id);

    let publish: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header(authorized().0, authorized().1)
        .header("x-maestro-headless-connection-id", "conn_exec")
        .header(
            "x-maestro-headless-subscriber-id",
            controller_subscription_id,
        )
        .header(
            "x-maestro-headless-connection-capability",
            controller_connection_capability,
        )
        .json(&json!({"type": "prompt", "content": "client-tool-boundary"}))
        .send()
        .await
        .expect("message response")
        .json()
        .await
        .expect("message json");
    assert_eq!(publish["published_messages"], 2);

    let read_replay = |subscription_id: &str| {
        let client = client.clone();
        let url = format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={subscription_id}",
            handle.base_url()
        );
        async move {
            let mut response = client
                .get(url)
                .header("authorization", "Bearer controller-test-token")
                .send()
                .await
                .expect("events response");
            assert_eq!(response.status(), StatusCode::OK);
            let mut text = String::new();
            for _ in 0..8 {
                let chunk =
                    tokio::time::timeout(std::time::Duration::from_secs(1), response.chunk())
                        .await
                        .expect("event chunk timeout")
                        .expect("event chunk read");
                let Some(chunk) = chunk else {
                    break;
                };
                text.push_str(&String::from_utf8_lossy(&chunk));
                if text.contains("\"request_id\":\"server-client-tool\"") {
                    break;
                }
            }
            text
        }
    };
    let controller_events = read_replay(controller_subscription_id).await;
    let viewer_events = read_replay(viewer_subscription_id).await;

    assert!(controller_events.contains("client-execution-secret"));
    assert!(controller_events.contains("client-byte-faithful"));
    assert!(controller_events.contains("server-execution-secret"));
    assert!(controller_events.contains("server-byte-faithful"));
    assert!(!viewer_events.contains("client-execution-secret"));
    assert!(!viewer_events.contains("server-execution-secret"));
    assert!(viewer_events.contains("[REDACTED:token:portable-export]"));

    handle.shutdown().await;
}

#[tokio::test]
async fn upgraded_controller_keeps_existing_viewer_stream_redacted() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()).with_auth_token("controller-test-token"),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let viewer_connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_second",
            "role": "viewer",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("viewer connection response")
        .json()
        .await
        .expect("viewer connection json");
    let connection_capability = viewer_connection["connection_capability"]
        .as_str()
        .expect("viewer connection capability");
    let viewer_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "connectionId": "conn_second",
            "connectionCapability": connection_capability,
            "connectionCapabilityRequired": true,
            "role": "viewer",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("viewer subscription response")
        .json()
        .await
        .expect("viewer subscription json");
    let viewer_subscription_id = viewer_subscription["subscription_id"]
        .as_str()
        .expect("viewer subscription id");

    let upgraded: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_second",
            "connectionCapability": connection_capability,
            "connectionCapabilityRequired": true,
            "role": "controller",
            "takeControl": true
        }))
        .send()
        .await
        .expect("controller upgrade response")
        .json()
        .await
        .expect("controller upgrade json");
    assert_eq!(upgraded["controller_lease_granted"], true);

    let published: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .header("x-maestro-headless-connection-id", "conn_second")
        .header("x-maestro-headless-subscriber-id", viewer_subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            connection_capability,
        )
        .json(&json!({"type": "prompt", "content": "client-tool-boundary"}))
        .send()
        .await
        .expect("message response")
        .json()
        .await
        .expect("message json");
    assert_eq!(published["published_messages"], 2);

    let mut events = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={viewer_subscription_id}",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .send()
        .await
        .expect("viewer events response");
    assert_eq!(events.status(), StatusCode::OK);
    let mut text = String::new();
    for _ in 0..8 {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), events.chunk())
            .await
            .expect("viewer event chunk timeout")
            .expect("viewer event chunk read");
        let Some(chunk) = chunk else {
            break;
        };
        text.push_str(&String::from_utf8_lossy(&chunk));
        if text.contains("\"request_id\":\"server-client-tool\"") {
            break;
        }
    }
    assert!(!text.contains("client-execution-secret"));
    assert!(!text.contains("server-execution-secret"));
    assert!(text.contains("[REDACTED:token:portable-export]"));

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_event_stream_is_revoked_on_takeover() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()).with_auth_token("controller-test-token"),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let subscribe =
        |connection_id: &'static str, connection_capability: String, take_control: bool| {
            let client = client.clone();
            let url = format!(
                "{}/api/headless/sessions/sess_test/subscribe",
                handle.base_url()
            );
            async move {
                client
                    .post(url)
                    .header("authorization", "Bearer controller-test-token")
                    .json(&json!({
                        "connectionId": connection_id,
                        "connectionCapability": connection_capability,
                        "connectionCapabilityRequired": true,
                        "role": "controller",
                        "takeControl": take_control,
                        "capabilities": {"transcriptGrade": "delta"}
                    }))
                    .send()
                    .await
                    .expect("subscription response")
                    .json::<serde_json::Value>()
                    .await
                    .expect("subscription json")
            }
        };

    let first_connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_first",
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("first controller connection")
        .json()
        .await
        .expect("first controller json");
    let first_subscription = subscribe(
        "conn_first",
        first_connection["connection_capability"]
            .as_str()
            .expect("first connection capability")
            .to_string(),
        false,
    )
    .await;
    let first_subscription_id = first_subscription["subscription_id"]
        .as_str()
        .expect("first subscription id");
    let mut stale_stream = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={first_subscription_id}",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .send()
        .await
        .expect("first controller events response");
    assert_eq!(stale_stream.status(), StatusCode::OK);

    let second_connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_second",
            "role": "controller",
            "takeControl": true,
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("second controller connection")
        .json()
        .await
        .expect("second controller json");
    let second_subscription = subscribe(
        "conn_second",
        second_connection["connection_capability"]
            .as_str()
            .expect("second connection capability")
            .to_string(),
        true,
    )
    .await;
    let second_subscription_id = second_subscription["subscription_id"]
        .as_str()
        .expect("second subscription id");
    let second_connection_capability = second_subscription["connection_capability"]
        .as_str()
        .expect("second connection capability");

    let publish = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .header("x-maestro-headless-connection-id", "conn_second")
        .header("x-maestro-headless-subscriber-id", second_subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            second_connection_capability,
        )
        .json(&json!({"type": "prompt", "content": "client-tool-boundary"}))
        .send()
        .await
        .expect("message response");
    assert_eq!(publish.status(), StatusCode::OK);

    let mut stale_text = String::new();
    for _ in 0..8 {
        let chunk =
            tokio::time::timeout(std::time::Duration::from_millis(250), stale_stream.chunk()).await;
        let Ok(Ok(Some(chunk))) = chunk else {
            break;
        };
        stale_text.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!stale_text.contains("client-execution-secret"));
    assert!(!stale_text.contains("server-execution-secret"));

    let mut current_stream = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={second_subscription_id}",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .send()
        .await
        .expect("second controller events response");
    assert_eq!(current_stream.status(), StatusCode::OK);
    let mut current_text = String::new();
    for _ in 0..8 {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), current_stream.chunk())
            .await
            .expect("current event chunk timeout")
            .expect("current event chunk read");
        let Some(chunk) = chunk else {
            break;
        };
        current_text.push_str(&String::from_utf8_lossy(&chunk));
        if current_text.contains("server-execution-secret") {
            break;
        }
    }
    assert!(current_text.contains("client-execution-secret"));
    assert!(current_text.contains("server-execution-secret"));

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_event_stream_is_revoked_on_hello_demotion() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()).with_auth_token("controller-test-token"),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let first_connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_exec",
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("first controller connection")
        .json()
        .await
        .expect("first controller json");
    let first_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "connectionId": "conn_exec",
            "connectionCapability": first_connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("first subscription response")
        .json()
        .await
        .expect("first subscription json");
    let first_subscription_id = first_subscription["subscription_id"]
        .as_str()
        .expect("first subscription id");
    let first_connection_capability = first_subscription["connection_capability"]
        .as_str()
        .expect("first connection capability");
    let mut stale_stream = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/events?cursor=0&subscriptionId={first_subscription_id}",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .send()
        .await
        .expect("first controller events response");
    assert_eq!(stale_stream.status(), StatusCode::OK);

    let demotion: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .header("x-maestro-headless-connection-id", "conn_exec")
        .header("x-maestro-headless-subscriber-id", first_subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            first_connection_capability,
        )
        .json(&json!({"type": "hello", "role": "viewer"}))
        .send()
        .await
        .expect("demotion response")
        .json()
        .await
        .expect("demotion json");
    assert_eq!(demotion["ok"], true);
    assert!(demotion["snapshot"]["state"]["controller_connection_id"].is_null());

    let second_connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_second",
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("second controller connection")
        .json()
        .await
        .expect("second controller json");
    let second_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "connectionId": "conn_second",
            "connectionCapability": second_connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller",
            "capabilities": {"transcriptGrade": "delta"}
        }))
        .send()
        .await
        .expect("second subscription response")
        .json()
        .await
        .expect("second subscription json");
    let second_subscription_id = second_subscription["subscription_id"]
        .as_str()
        .expect("second subscription id");
    let second_connection_capability = second_subscription["connection_capability"]
        .as_str()
        .expect("second connection capability");

    let publish = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .header("x-maestro-headless-connection-id", "conn_second")
        .header("x-maestro-headless-subscriber-id", second_subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            second_connection_capability,
        )
        .json(&json!({"type": "prompt", "content": "client-tool-boundary"}))
        .send()
        .await
        .expect("message response");
    assert_eq!(publish.status(), StatusCode::OK);

    let mut stale_text = String::new();
    for _ in 0..8 {
        let chunk =
            tokio::time::timeout(std::time::Duration::from_millis(250), stale_stream.chunk()).await;
        let Ok(Ok(Some(chunk))) = chunk else {
            break;
        };
        stale_text.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!stale_text.contains("client-execution-secret"));
    assert!(!stale_text.contains("server-execution-secret"));

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_replay_write_is_cancelled_during_takeover() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let authorization = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        upsert_connection(
            &mut state,
            ConnectionUpsert {
                connection_id: "conn_replay".to_string(),
                connection_capability: None,
                connection_capability_required: true,
                role: ConnectionRole::Controller,
                client_protocol_version: None,
                client_info: None,
                capabilities: None,
                opt_out_notifications: vec![],
                take_control: false,
            },
        )
        .expect("controller connection");
        let connection_capability = state
            .connections
            .get("conn_replay")
            .expect("connection")
            .connection_capability
            .clone();
        state.subscriptions.insert(
            "sub_replay".to_string(),
            SubscriptionRecord {
                connection_id: "conn_replay".to_string(),
                connection_capability,
                authority_mode: ConnectionAuthorityMode::Capability,
                role: ConnectionRole::Controller,
                attached: true,
            },
        );
        ControllerStreamAuthorization {
            connection_id: "conn_replay".to_string(),
            subscription_id: "sub_replay".to_string(),
            cancellation: state.controller_stream_cancellation.clone(),
        }
    };

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("listener address");
    let client = tokio::spawn(async move {
        TcpStream::connect(address)
            .await
            .expect("test client connection")
    });
    let (server, _) = listener.accept().await.expect("server connection");
    let mut client = client.await.expect("client task");

    let final_marker = "controller-replay-final-marker";
    let envelope = StreamEnvelope::Message {
        cursor: 1,
        message: Box::new(FromAgentMessage::ClientToolRequest {
            call_id: "large-replay".to_string(),
            tool_execution_id: None,
            tool: "bash".to_string(),
            args: json!({
                "command": "client execution",
                "padding": format!("{}{}", "x".repeat(8 * 1024 * 1024), final_marker),
            }),
        }),
    };
    let writer_shared = shared.clone();
    let writer = tokio::spawn(async move {
        let mut server = server;
        write_sse_event_if_authorized(&mut server, &writer_shared, Some(&authorization), &envelope)
            .await
            .expect("authorized replay write")
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revoke_controller_streams(&mut state);
        state.controller_connection_id = Some("conn_takeover".to_string());
    }
    let completed = tokio::time::timeout(std::time::Duration::from_secs(1), writer)
        .await
        .expect("revoked replay writer should stop")
        .expect("writer task");
    assert!(!completed);

    let mut delivered = Vec::new();
    client
        .read_to_end(&mut delivered)
        .await
        .expect("read partial replay");
    assert!(!String::from_utf8_lossy(&delivered).contains(final_marker));
}

#[tokio::test]
async fn state_snapshot_redacts_sensitive_supervisor_state() {
    let workspace = tempdir().expect("workspace");
    let mut current_response = crate::headless::StreamingResponse::new("resp-state-1".to_string());
    current_response.append("working on hosted state", false);
    let mut active_tools = HashMap::new();
    active_tools.insert(
        "active-call-1".to_string(),
        crate::headless::ActiveTool {
            call_id: "active-call-1".to_string(),
            tool: "bash".to_string(),
            output: "secret command output".to_string(),
            started: std::time::Instant::now(),
        },
    );
    let supervisor_state = AgentState {
        model: Some("gpt-5.6".to_string()),
        provider: Some("openai".to_string()),
        session_id: Some("supervisor-session-1".to_string()),
        cwd: Some("/runtime/workspace".to_string()),
        git_branch: Some("feature/runtime-state".to_string()),
        current_response: Some(current_response),
        pending_approvals: vec![crate::headless::PendingApproval {
            call_id: "call-1".to_string(),
            tool_execution_id: None,
            request_id: Some("approval-1".to_string()),
            tool: "bash".to_string(),
            args: json!({"cmd": "cargo test"}),
            started_at_ms: None,
        }],
        pending_user_inputs: vec![crate::headless::PendingApproval {
            call_id: "input-call-1".to_string(),
            tool_execution_id: Some("input-exec-1".to_string()),
            request_id: None,
            tool: "user_input".to_string(),
            args: json!({"prompt": "enter secret"}),
            started_at_ms: Some(1_771_000_000_100),
        }],
        pending_tool_retries: vec![crate::headless::PendingApproval {
            call_id: "retry-call-1".to_string(),
            tool_execution_id: Some("retry-exec-1".to_string()),
            request_id: Some("retry-1".to_string()),
            tool: "bash".to_string(),
            args: json!({"error": "token leaked in stderr"}),
            started_at_ms: Some(1_771_000_000_200),
        }],
        active_tools,
        last_status: Some("thinking".to_string()),
        is_ready: true,
        is_responding: true,
        ..AgentState::default()
    };
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(supervisor_state)),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let controller: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_state",
            "role": "controller"
        }))
        .send()
        .await
        .expect("controller response")
        .json()
        .await
        .expect("controller json");
    assert_eq!(controller["snapshot"]["state"]["model"], "gpt-5.6");
    assert_eq!(controller["snapshot"]["state"]["connection_count"], 1);

    let subscribe: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_state",
            "connectionCapability": controller["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscribe response")
        .json()
        .await
        .expect("subscribe json");
    assert_eq!(
        subscribe["snapshot"]["state"]["controller_connection_id"],
        "conn_state"
    );
    assert!(subscribe["snapshot"]["state"]["controller_subscription_id"].is_null());
    assert!(subscribe["controller_subscription_id"].as_str().is_some());

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["state"]["model"], "gpt-5.6");
    assert_eq!(state["state"]["provider"], "openai");
    assert_eq!(state["state"]["session_id"], "supervisor-session-1");
    assert_eq!(state["state"]["cwd"], "/runtime/workspace");
    assert_eq!(state["state"]["git_branch"], "feature/runtime-state");
    assert!(state["state"]["current_response"].is_null());
    assert_eq!(state["state"]["pending_approvals"][0]["call_id"], "call-1");
    assert_eq!(
        state["state"]["pending_approvals"][0]["request_id"],
        "approval-1"
    );
    assert_eq!(state["state"]["pending_approvals"][0]["tool"], "bash");
    assert_eq!(state["state"]["pending_approvals"][0]["args"], json!({}));
    assert!(
        state["state"]["pending_approvals"][0]
            .get("tool_execution_id")
            .is_none()
    );
    assert!(
        state["state"]["pending_approvals"][0]
            .get("started_at_ms")
            .is_none()
    );
    assert_eq!(state["state"]["pending_client_tools"], json!([]));
    assert_eq!(state["state"]["pending_mcp_elicitations"], json!([]));
    assert_eq!(
        state["state"]["pending_user_inputs"][0]["call_id"],
        "input-call-1"
    );
    assert_eq!(
        state["state"]["pending_user_inputs"][0]["tool_execution_id"],
        "input-exec-1"
    );
    assert_eq!(state["state"]["pending_user_inputs"][0]["args"], json!({}));
    assert!(
        state["state"]["pending_user_inputs"][0]
            .get("request_id")
            .is_none()
    );
    assert_eq!(
        state["state"]["pending_tool_retries"][0]["call_id"],
        "retry-call-1"
    );
    assert_eq!(
        state["state"]["pending_tool_retries"][0]["request_id"],
        "retry-1"
    );
    assert_eq!(state["state"]["pending_tool_retries"][0]["args"], json!({}));
    assert_eq!(state["state"]["tracked_tools"], json!([]));
    assert_eq!(
        state["state"]["active_tools"][0]["call_id"],
        "active-call-1"
    );
    assert_eq!(state["state"]["active_tools"][0]["tool"], "bash");
    assert_eq!(state["state"]["active_tools"][0]["output"], "");
    assert_eq!(state["state"]["last_status"], "thinking");
    assert_eq!(state["state"]["is_ready"], true);
    assert_eq!(state["state"]["is_responding"], true);
    assert_eq!(state["state"]["connection_count"], 1);
    assert_eq!(state["state"]["subscriber_count"], 1);
    assert_eq!(state["state"]["controller_connection_id"], "conn_state");
    assert_eq!(
        state["state"]["connections"][0]["controller_lease_granted"],
        true
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn remote_transport_attaches_and_receives_workspace_events() {
    let workspace = tempdir().expect("workspace");
    tokio::fs::write(workspace.path().join("notes.md"), "alpha\nbeta\n")
        .await
        .expect("write fixture");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let mut transport = crate::headless::RemoteAgentTransport::connect(RemoteTransportConfig {
        base_url: handle.base_url(),
        session_id: Some("sess_test".to_string()),
        role: Some("controller".to_string()),
        client_name: "rust-hosted-runner-test".to_string(),
        opt_out_notifications: vec!["heartbeat".to_string()],
        ..RemoteTransportConfig::default()
    })
    .await
    .expect("connect");

    assert_eq!(transport.session_id(), "sess_test");
    transport
        .read_file(
            "read_notes".to_string(),
            "notes.md".to_string(),
            None,
            None,
            None,
        )
        .expect("send read request");

    let mut saw_read = false;
    for _ in 0..8 {
        let incoming =
            tokio::time::timeout(std::time::Duration::from_secs(1), transport.recv_incoming())
                .await
                .expect("incoming timeout")
                .expect("incoming event");
        if let crate::headless::RemoteIncoming::Message(FromAgentMessage::UtilityFileReadResult {
            content,
            ..
        }) = incoming
        {
            saw_read = content.contains("alpha");
            break;
        }
    }
    assert!(saw_read, "expected hosted runner file read event");

    transport
        .shutdown_and_wait()
        .await
        .expect("shutdown transport");
    let state_after_disconnect: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state_after_disconnect["state"]["connection_count"], 0);
    assert_eq!(state_after_disconnect["state"]["subscriber_count"], 0);
    handle.shutdown().await;
}

#[tokio::test]
async fn hosted_messages_reject_workspace_escape_before_file_work() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    let connection_id = connection["connection_id"]
        .as_str()
        .expect("connection id")
        .to_string();
    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": connection_id,
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id")
        .to_string();
    let connection_capability = subscription["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();

    let response = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", connection_id)
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            connection_capability,
        )
        .json(&json!({
            "type": "utility_file_read",
            "read_id": "escape",
            "path": "../secret.txt"
        }))
        .send()
        .await
        .expect("message response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = response.json().await.expect("error body");
    assert_eq!(body["error_type"], "workspace_violation");

    handle.shutdown().await;
}

#[tokio::test]
async fn hosted_messages_reject_symlink_escape_for_missing_child() {
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link"))
        .expect("symlink fixture");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({"sessionId": "sess_test", "role": "controller"}))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    let connection_id = connection["connection_id"]
        .as_str()
        .expect("connection id")
        .to_string();
    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": connection_id,
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id")
        .to_string();
    let connection_capability = subscription["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();

    let response = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", connection_id)
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            connection_capability,
        )
        .json(&json!({
            "type": "utility_file_read",
            "read_id": "symlink-escape",
            "path": "outside-link/missing.txt"
        }))
        .send()
        .await
        .expect("message response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = response.json().await.expect("error body");
    assert_eq!(body["error_type"], "workspace_violation");

    handle.shutdown().await;
}

#[tokio::test]
async fn duplicate_subscribe_preserves_disconnect_cleanup() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_multi",
            "role": "controller",
            "protocolVersion": HEADLESS_PROTOCOL_VERSION,
            "clientInfo": {"name": "lease-test", "version": "1.0.0"},
            "capabilities": {
                "serverRequests": ["approval"],
                "utilityOperations": ["file_read"],
                "rawAgentEvents": true
            }
        }))
        .send()
        .await
        .expect("connection response")
        .json()
        .await
        .expect("connection json");
    assert_eq!(connection["connection_id"], "conn_multi");
    assert_eq!(connection["connection_capability_required"], false);
    assert!(connection["connection_capability"].is_null());
    assert!(connection["lease_expires_at"].as_str().is_some());

    let promotion = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_multi",
            "connectionCapability": "cap_00112233445566778899aabbccddeeff",
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("legacy promotion response");
    assert_eq!(promotion.status(), StatusCode::FORBIDDEN);

    let initial_subscription = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_multi",
            "connectionCapability": connection["connection_capability"],
            "role": "controller"
        }))
        .send()
        .await
        .expect("initial subscription response");
    assert_eq!(initial_subscription.status(), StatusCode::OK);
    let initial_subscription: serde_json::Value = initial_subscription
        .json()
        .await
        .expect("initial subscription json");
    let initial_subscription_id = initial_subscription["subscription_id"]
        .as_str()
        .expect("initial subscription id")
        .to_string();

    let unproven_duplicate = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_multi",
            "role": "controller"
        }))
        .send()
        .await
        .expect("unproven duplicate response");
    assert_eq!(unproven_duplicate.status(), StatusCode::FORBIDDEN);

    let proven_duplicate = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_multi",
            "subscriptionId": initial_subscription_id,
            "role": "controller"
        }))
        .send()
        .await
        .expect("proven duplicate response");
    assert_eq!(proven_duplicate.status(), StatusCode::OK);
    let proven_duplicate: serde_json::Value = proven_duplicate
        .json()
        .await
        .expect("proven duplicate json");
    let mut subscription_ids = [
        initial_subscription_id,
        proven_duplicate["subscription_id"]
            .as_str()
            .expect("proven duplicate subscription id")
            .to_string(),
    ];
    subscription_ids.sort();

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["state"]["subscriber_count"], 2);
    assert!(state["state"]["controller_subscription_id"].is_null());
    let connection_state = state["state"]["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .find(|connection| connection["connection_id"] == "conn_multi")
        .expect("conn_multi state");
    assert_eq!(connection_state["subscription_count"], 2);
    assert_eq!(
        connection_state["client_protocol_version"],
        HEADLESS_PROTOCOL_VERSION
    );
    assert_eq!(connection_state["client_info"]["name"], "lease-test");
    assert_eq!(connection_state["capabilities"]["raw_agent_events"], true);
    assert!(connection_state["lease_expires_at"].as_str().is_some());

    let legacy_message = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_multi")
        .header("x-maestro-headless-subscriber-id", &subscription_ids[0])
        .json(&json!({"type": "prompt", "content": "legacy-compatible"}))
        .send()
        .await
        .expect("legacy message response");
    assert_eq!(legacy_message.status(), StatusCode::OK);

    let heartbeat: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/heartbeat",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_multi",
            "subscriptionId": subscription_ids[0],
            "connectionCapability": connection["connection_capability"]
        }))
        .send()
        .await
        .expect("heartbeat response")
        .json()
        .await
        .expect("heartbeat json");
    assert_eq!(heartbeat["controller_lease_granted"], true);
    assert!(heartbeat["lease_expires_at"].as_str().is_some());

    let disconnected: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/disconnect",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_multi",
            "subscriptionId": subscription_ids[0],
            "connectionCapability": connection["connection_capability"]
        }))
        .send()
        .await
        .expect("disconnect response")
        .json()
        .await
        .expect("disconnect json");
    assert_eq!(
        disconnected["disconnected_subscription_ids"]
            .as_array()
            .expect("disconnected subscriptions")
            .len(),
        2
    );

    let state_after_disconnect: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state_after_disconnect["state"]["subscriber_count"], 0);
    assert!(state_after_disconnect["state"]["controller_connection_id"].is_null());
    assert!(state_after_disconnect["state"]["controller_subscription_id"].is_null());

    handle.shutdown().await;
}

#[tokio::test]
async fn drain_manifest_filename_stays_inside_snapshot_root() {
    let workspace = tempdir().expect("workspace");
    let snapshot_root = workspace.path().join("snapshots");
    let mut config = test_config(workspace.path().to_path_buf());
    config.runner_session_id = "../evil/session.v1".to_string();
    config.snapshot_root = Some(snapshot_root.clone());
    let handle = start_hosted_runner(config)
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "sanitize-session"}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    let manifest_path = PathBuf::from(drain["manifest_path"].as_str().expect("manifest path"));
    assert_eq!(manifest_path.parent(), Some(snapshot_root.as_path()));
    assert!(manifest_path.exists());
    assert!(
        manifest_path
            .file_name()
            .expect("manifest filename")
            .to_string_lossy()
            .starts_with("___evil_session_v1-")
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_takeover_is_explicit_and_viewers_cannot_mutate() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let first_controller: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_first",
            "role": "controller"
        }))
        .send()
        .await
        .expect("first controller response")
        .json()
        .await
        .expect("first controller json");
    assert_eq!(first_controller["controller_connection_id"], "conn_first");

    let rejected_takeover = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_second",
            "role": "controller"
        }))
        .send()
        .await
        .expect("rejected takeover response");
    assert_eq!(rejected_takeover.status(), StatusCode::CONFLICT);

    let accepted_takeover: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_second",
            "role": "controller",
            "takeControl": true
        }))
        .send()
        .await
        .expect("accepted takeover response")
        .json()
        .await
        .expect("accepted takeover json");
    assert_eq!(accepted_takeover["controller_connection_id"], "conn_second");

    let controller_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_second",
            "connectionCapability": accepted_takeover["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller",
            "takeControl": true
        }))
        .send()
        .await
        .expect("controller subscription response")
        .json()
        .await
        .expect("controller subscription json");
    let controller_message: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_second")
        .header(
            "x-maestro-headless-subscriber-id",
            controller_subscription["subscription_id"]
                .as_str()
                .expect("controller subscription id"),
        )
        .header(
            "x-maestro-headless-connection-capability",
            controller_subscription["connection_capability"]
                .as_str()
                .expect("controller connection capability"),
        )
        .json(&json!({"type": "prompt", "content": "cursor please"}))
        .send()
        .await
        .expect("controller message response")
        .json()
        .await
        .expect("controller message json");
    assert_eq!(controller_message["ok"], true);
    assert!(controller_message["cursor"].as_u64().unwrap_or_default() > 0);

    let viewer: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_viewer",
            "connectionCapabilityRequired": true,
            "role": "viewer"
        }))
        .send()
        .await
        .expect("viewer response")
        .json()
        .await
        .expect("viewer json");
    assert_eq!(viewer["role"], "viewer");
    let viewer_subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_viewer",
            "connectionCapability": viewer["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "viewer"
        }))
        .send()
        .await
        .expect("viewer subscription response")
        .json()
        .await
        .expect("viewer subscription json");
    let viewer_subscription_id = viewer_subscription["subscription_id"]
        .as_str()
        .expect("viewer subscription id");
    let viewer_message = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_viewer")
        .header("x-maestro-headless-subscriber-id", viewer_subscription_id)
        .header(
            "x-maestro-headless-connection-capability",
            viewer_subscription["connection_capability"]
                .as_str()
                .expect("viewer connection capability"),
        )
        .json(&json!({"type": "prompt", "content": "nope"}))
        .send()
        .await
        .expect("viewer message response");
    assert_eq!(viewer_message.status(), StatusCode::FORBIDDEN);

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_connection_id_cannot_be_replayed_to_mint_a_subscription() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let controller: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_private",
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("controller subscription response")
        .json()
        .await
        .expect("controller subscription json");
    assert_eq!(controller["controller_connection_id"], "conn_private");
    let connection_capability = controller["connection_capability"]
        .as_str()
        .expect("connection capability");
    let public_state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert!(!public_state.to_string().contains(connection_capability));

    let replay = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": controller["controller_connection_id"],
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("replayed subscription response");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);

    let drain: serde_json::Value = client
        .post(format!(
            "{}/.well-known/evalops/remote-runner/drain",
            handle.base_url()
        ))
        .json(&json!({"reason": "capability-redaction-check"}))
        .send()
        .await
        .expect("drain response")
        .json()
        .await
        .expect("drain json");
    assert!(!drain.to_string().contains(connection_capability));

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_messages_require_the_private_connection_capability() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let controller: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_private",
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("controller subscription response")
        .json()
        .await
        .expect("controller subscription json");
    let subscription_id = controller["subscription_id"]
        .as_str()
        .expect("controller subscription id");

    let replay = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_private")
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .json(&json!({"type": "prompt", "content": "replayed authority"}))
        .send()
        .await
        .expect("replayed message response");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_heartbeat_rejects_replayed_public_authority() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let controller: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_private",
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("controller subscription response")
        .json()
        .await
        .expect("controller subscription json");

    let replay = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/heartbeat",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": controller["controller_connection_id"]
        }))
        .send()
        .await
        .expect("replayed heartbeat response");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);

    handle.shutdown().await;
}

#[tokio::test]
async fn controller_disconnect_rejects_replayed_public_authority() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let controller: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_private",
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("controller subscription response")
        .json()
        .await
        .expect("controller subscription json");

    let replay = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/disconnect",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": controller["controller_connection_id"]
        }))
        .send()
        .await
        .expect("replayed disconnect response");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("state response")
        .json()
        .await
        .expect("state json");
    assert_eq!(state["state"]["controller_connection_id"], "conn_private");
    assert_eq!(state["state"]["subscriber_count"], 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn failed_accepted_turn_persistence_rolls_back_and_allows_a_clean_retry() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(PendingThreadExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_accept_retry").await;
    let request = json!({
        "protocolVersion": "evalops.maestro.thread.v1",
        "turnId": "turn-accept-retry",
        "kind": "user_message",
        "content": "dispatch after durable acceptance"
    });
    let post_turn = || {
        client
            .post(format!(
                "{}/api/headless/threads/mrs_test/turns",
                handle.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_accept_retry")
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&request)
            .send()
    };

    handle.shared.fail_next_thread_persistences(1);
    let failed = post_turn().await.expect("failed acceptance response");
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    {
        let state = handle.shared.state.lock().expect("runner state");
        assert!(state.thread.turn("turn-accept-retry").is_none());
        assert!(!state.thread.has_active_turn());
    }

    let retried: serde_json::Value = post_turn()
        .await
        .expect("retried acceptance response")
        .error_for_status()
        .expect("retried acceptance status")
        .json()
        .await
        .expect("retried acceptance json");
    assert_eq!(retried["phase"], "running");
    assert_eq!(retried["replayed"], false);
    assert_eq!(
        executor.prompts.lock().expect("pending prompts").as_slice(),
        ["dispatch after durable acceptance"]
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn accepting_a_new_turn_clears_the_previous_terminal_receipt() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(TerminalThenPendingExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor,
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_receipt_turns").await;
    let turns_url = format!("{}/api/headless/threads/mrs_test/turns", handle.base_url());
    let append_turn = |turn_id: &str, content: &str| {
        client
            .post(&turns_url)
            .header("x-maestro-headless-connection-id", "conn_receipt_turns")
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&json!({
                "protocolVersion": "evalops.maestro.thread.v1",
                "turnId": turn_id,
                "kind": "user_message",
                "content": content
            }))
    };

    let first: serde_json::Value = append_turn("turn-a", "first turn")
        .send()
        .await
        .expect("first turn response")
        .error_for_status()
        .expect("first turn status")
        .json()
        .await
        .expect("first turn json");
    assert_eq!(first["phase"], "completed");
    assert_eq!(
        handle
            .shared
            .runtime_receipt()
            .expect("terminal receipt for first turn")
            .terminal,
        Some(RuntimeTerminalClassification::Completed)
    );

    let second: serde_json::Value = append_turn("turn-b", "second turn")
        .send()
        .await
        .expect("second turn response")
        .error_for_status()
        .expect("second turn status")
        .json()
        .await
        .expect("second turn json");
    assert_eq!(second["phase"], "running");
    assert!(handle.shared.runtime_receipt().is_none());

    let receipt = client
        .get(format!(
            "{}{}",
            handle.base_url(),
            HOSTED_RUNNER_RECEIPT_PATH
        ))
        .send()
        .await
        .expect("receipt response");
    assert_eq!(receipt.status(), StatusCode::SERVICE_UNAVAILABLE);

    handle.shutdown().await;
}

#[tokio::test]
async fn failed_post_dispatch_persistence_returns_success_without_duplicate_dispatch() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(DispatchPersistenceFailureExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    *executor
        .shared
        .lock()
        .expect("dispatch persistence shared runner") = Some(handle.shared.clone());
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_dispatch_persist").await;
    let request = json!({
        "protocolVersion": "evalops.maestro.thread.v1",
        "turnId": "turn-dispatch-persist",
        "kind": "user_message",
        "content": "dispatch exactly once"
    });
    let post_turn = || {
        client
            .post(format!(
                "{}/api/headless/threads/mrs_test/turns",
                handle.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_dispatch_persist")
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&request)
            .send()
    };

    let first: serde_json::Value = post_turn()
        .await
        .expect("post-dispatch persistence response")
        .error_for_status()
        .expect("dispatch success must not become a persistence error")
        .json()
        .await
        .expect("post-dispatch persistence json");
    assert_eq!(first["phase"], "running");
    assert_eq!(first["replayed"], false);

    let replayed: serde_json::Value = post_turn()
        .await
        .expect("post-dispatch replay response")
        .error_for_status()
        .expect("post-dispatch replay status")
        .json()
        .await
        .expect("post-dispatch replay json");
    assert_eq!(replayed["replayed"], true);
    assert_eq!(
        executor
            .prompts
            .lock()
            .expect("dispatch persistence prompts")
            .as_slice(),
        ["dispatch exactly once"]
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn durable_thread_appends_idempotent_turns_and_exposes_waiting_state() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(RecordingThreadExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_thread").await;
    let turns_url = format!("{}/api/headless/threads/mrs_test/turns", handle.base_url());

    let append_turn = |turn_id: &str, kind: &str, content: &str| {
        client
            .post(&turns_url)
            .header("x-maestro-headless-connection-id", "conn_thread")
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&json!({
                "protocolVersion": "evalops.maestro.thread.v1",
                "turnId": turn_id,
                "kind": kind,
                "content": content
            }))
            .send()
    };

    let first: serde_json::Value = append_turn("turn-1", "user_message", "hello")
        .await
        .expect("first turn response")
        .error_for_status()
        .expect("first turn status")
        .json()
        .await
        .expect("first turn json");
    assert_eq!(first["thread_id"], "mrs_test");
    assert_eq!(first["turn_id"], "turn-1");
    assert_eq!(first["run_id"], "run_turn-1");
    assert_eq!(first["phase"], "completed");
    assert_eq!(first["replayed"], false);

    let replay: serde_json::Value = append_turn("turn-1", "user_message", "hello")
        .await
        .expect("replayed turn response")
        .error_for_status()
        .expect("replayed turn status")
        .json()
        .await
        .expect("replayed turn json");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["run_id"], "run_turn-1");
    assert_eq!(
        executor
            .prompts
            .lock()
            .expect("recorded prompts")
            .as_slice(),
        ["hello"]
    );

    let conflict = append_turn("turn-1", "user_message", "different")
        .await
        .expect("conflicting turn response");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    let steer: serde_json::Value = append_turn("turn-2", "steer", "needs approval")
        .await
        .expect("steering response")
        .error_for_status()
        .expect("steering status")
        .json()
        .await
        .expect("steering json");
    assert_eq!(steer["phase"], "waiting_for_approval");

    let thread: serde_json::Value = client
        .get(format!(
            "{}/api/headless/threads/mrs_test",
            handle.base_url()
        ))
        .send()
        .await
        .expect("thread state response")
        .error_for_status()
        .expect("thread state status")
        .json()
        .await
        .expect("thread state json");
    assert_eq!(thread["protocol_version"], "evalops.maestro.thread.v1");
    assert_eq!(thread["thread_id"], "mrs_test");
    assert_eq!(thread["phase"], "waiting_for_approval");
    assert_eq!(thread["active_turn_id"], "turn-2");
    assert_eq!(thread["turns"].as_array().expect("turns").len(), 2);

    let response_request = json!({
        "type": "server_request_response",
        "request_id": "call-1",
        "request_type": "approval",
        "approved": true,
        "reason": "approved by the operator"
    });
    let response_url = format!(
        "{}/api/headless/sessions/sess_test/messages",
        handle.base_url()
    );
    let first_response: serde_json::Value = client
        .post(&response_url)
        .header("x-maestro-headless-connection-id", "conn_thread")
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .header("x-maestro-idempotency-key", "response-turn-2-call-1")
        .json(&response_request)
        .send()
        .await
        .expect("first response")
        .error_for_status()
        .expect("first response status")
        .json()
        .await
        .expect("first response json");
    assert_eq!(first_response["replayed"], false);
    let replayed_response: serde_json::Value = client
        .post(&response_url)
        .header("x-maestro-headless-connection-id", "conn_thread")
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .header("x-maestro-idempotency-key", "response-turn-2-call-1")
        .json(&response_request)
        .send()
        .await
        .expect("replayed response")
        .error_for_status()
        .expect("replayed response status")
        .json()
        .await
        .expect("replayed response json");
    assert_eq!(replayed_response["replayed"], true);

    handle.shutdown().await;
}

#[tokio::test]
async fn durable_thread_restores_turn_idempotency_and_cursor_from_workspace() {
    let workspace = tempdir().expect("workspace");
    let source_executor = Arc::new(RecordingThreadExecutor::default());
    let source = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        source_executor,
    )
    .await
    .expect("start source hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &source.base_url(), "conn_source_thread").await;
    let request = json!({
        "protocolVersion": "evalops.maestro.thread.v1",
        "turnId": "turn-durable",
        "kind": "user_message",
        "content": "persist me"
    });
    let source_turn: serde_json::Value = client
        .post(format!(
            "{}/api/headless/threads/mrs_test/turns",
            source.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_source_thread")
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .header("x-maestro-runtime-generation", "0")
        .json(&request)
        .send()
        .await
        .expect("source turn response")
        .error_for_status()
        .expect("source turn status")
        .json()
        .await
        .expect("source turn json");
    let durable_cursor = source_turn["cursor"].as_u64().expect("source cursor");
    assert!(durable_cursor > 0);
    source.shutdown().await;

    let restored_executor = Arc::new(RecordingThreadExecutor::default());
    let restored = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        restored_executor.clone(),
    )
    .await
    .expect("start restored hosted runner");
    let restored_state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/threads/mrs_test",
            restored.base_url()
        ))
        .send()
        .await
        .expect("restored thread state response")
        .error_for_status()
        .expect("restored thread state status")
        .json()
        .await
        .expect("restored thread state json");
    assert_eq!(restored_state["cursor"], durable_cursor);
    assert_eq!(restored_state["turns"][0]["turn_id"], "turn-durable");
    let (capability, subscription_id) =
        attach_thread_controller(&client, &restored.base_url(), "conn_restored_thread").await;
    let replayed: serde_json::Value = client
        .post(format!(
            "{}/api/headless/threads/mrs_test/turns",
            restored.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_restored_thread")
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .header("x-maestro-runtime-generation", "0")
        .json(&request)
        .send()
        .await
        .expect("restored turn response")
        .error_for_status()
        .expect("restored turn status")
        .json()
        .await
        .expect("restored turn json");
    assert_eq!(replayed["replayed"], true);
    assert!(
        replayed["cursor"].as_u64().expect("replayed cursor") >= durable_cursor,
        "reattachment events may advance but must never rewind the durable cursor"
    );
    assert!(
        restored_executor
            .prompts
            .lock()
            .expect("restored prompts")
            .is_empty(),
        "restoring an accepted turn must not execute it twice"
    );

    restored.shutdown().await;
}

#[tokio::test]
async fn consumed_response_finalization_recovers_without_runtime_events() {
    struct JournalFaultExecutor {
        journal: Mutex<Option<PathBuf>>,
        calls: AtomicUsize,
    }
    impl HostedRunnerHeadlessMessageExecutor for JournalFaultExecutor {
        fn execute(
            &self,
            _context: &HostedRunnerHeadlessMessageContext,
            _message: ToAgentMessage,
        ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let journal = self.journal.lock().unwrap();
            let journal = journal.as_ref().expect("journal installed before response");
            // Pending acceptance already committed. Preserve that document,
            // then make the finalization rename fail at the real filesystem.
            std::fs::rename(journal, journal.with_extension("saved")).unwrap();
            std::fs::create_dir(journal).unwrap();
            Ok(HostedRunnerHeadlessMessageResult::runtime_handled(
                Vec::new(),
                "response consumed without subsequent events",
            ))
        }
    }
    let workspace = tempdir().unwrap();
    let config = test_config(workspace.path().to_path_buf());
    let executor = Arc::new(JournalFaultExecutor {
        journal: Mutex::new(None),
        calls: AtomicUsize::new(0),
    });
    let handle = start_hosted_runner_with_message_executor(config.clone(), executor.clone())
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_finalize").await;
    handle.shared.stop_event_pump().await.unwrap();
    let journal = std::fs::read_dir(workspace.path().join(".maestro/hosted-runner/threads"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    *executor.journal.lock().unwrap() = Some(journal.clone());
    let headers = |key: &str| {
        HashMap::from([
            (
                "x-maestro-headless-connection-id".to_owned(),
                "conn_finalize".to_owned(),
            ),
            (
                "x-maestro-headless-subscriber-id".to_owned(),
                subscription_id.clone(),
            ),
            (
                "x-maestro-headless-connection-capability".to_owned(),
                capability.clone(),
            ),
            ("x-maestro-idempotency-key".to_owned(), key.to_owned()),
        ])
    };
    let message = ToAgentMessage::ToolResponse {
        call_id: "finalize-call".to_owned(),
        tool_execution_id: None,
        approved: true,
        result: None,
    };
    let ResponseBody::Json { status, body } = handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("finalize-key"),
        message.clone(),
    )
    .await
    .unwrap() else {
        panic!("expected JSON")
    };
    assert_eq!(status, 200);
    assert_eq!(body["accepted"], true);
    assert_eq!(body["replayed"], false);
    assert!(body["message"].as_str().unwrap().contains("memory-only"));
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(journal.with_extension("saved")).unwrap()).unwrap();
    assert!(
        pending["response_idempotency_keys"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        pending["pending_response_idempotency"]["finalize-key"],
        serde_json::to_value(&message).unwrap()
    );
    assert_eq!(
        pending["response_request_owners"]["finalize-call"],
        "finalize-key"
    );

    let ResponseBody::Json { body, .. } = handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("finalize-key"),
        message.clone(),
    )
    .await
    .unwrap() else {
        panic!("expected JSON")
    };
    assert_eq!(body["replayed"], true);
    let changed = ToAgentMessage::ToolResponse {
        call_id: "finalize-call".to_owned(),
        tool_execution_id: None,
        approved: false,
        result: None,
    };
    for (key, payload) in [
        ("finalize-key", changed.clone()),
        ("other-key", message.clone()),
    ] {
        let error =
            match handle_message(handle.shared.clone(), "sess_test", headers(key), payload).await {
                Err(error) => error,
                Ok(_) => panic!("response digest and ownership must survive failed persistence"),
            };
        assert_eq!(error.code, HostedRunnerErrorCode::IdempotencyConflict);
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);

    {
        let _lifecycle = handle.shared.mutation_lifecycle.lock().await;
        std::fs::remove_dir(&journal).unwrap();
        std::fs::rename(journal.with_extension("saved"), &journal).unwrap();
    }
    // No events, pump ticks, new requests, or shutdown flush may rescue this
    // boundary: the existing independent persistence supervisor must commit it.
    let recovered = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let document: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
            if document["response_idempotency_keys"]
                .as_array()
                .unwrap()
                .contains(&json!("finalize-key"))
            {
                break document;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consumed response finalization must recover without runtime events");
    assert_eq!(
        recovered["response_idempotency_digests"]["finalize-key"],
        response_message_digest(&message)
    );
    assert_eq!(
        recovered["response_request_owners"]["finalize-call"],
        "finalize-key"
    );
    assert!(
        recovered["pending_response_idempotency"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert!(
        recovered["flush_watermark"].as_u64().unwrap()
            > pending["flush_watermark"].as_u64().unwrap()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    handle.shutdown().await;

    let restored_executor = Arc::new(ResponseRecordingExecutor::default());
    let restored = start_hosted_runner_with_message_executor(config, restored_executor.clone())
        .await
        .unwrap();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &restored.base_url(), "conn_finalize_restored").await;
    let response = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            restored.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_finalize_restored")
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .header("x-maestro-headless-connection-capability", capability)
        .header("x-maestro-idempotency-key", "finalize-key")
        .json(&message)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(response["replayed"], true);
    assert!(restored_executor.messages.lock().unwrap().is_empty());
    restored.shutdown().await;
}

#[tokio::test]
async fn response_messages_cover_input_client_tool_retry_and_persist_idempotency() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(ResponseRecordingExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_response_types").await;
    let headers = |key: &str| {
        [
            (
                "x-maestro-headless-connection-id".to_string(),
                "conn_response_types".to_string(),
            ),
            (
                "x-maestro-headless-subscriber-id".to_string(),
                subscription_id.clone(),
            ),
            (
                "x-maestro-headless-connection-capability".to_string(),
                capability.clone(),
            ),
            ("x-maestro-idempotency-key".to_string(), key.to_string()),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>()
    };

    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("response-input"),
        ToAgentMessage::ServerRequestResponse {
            request_id: "input-1".into(),
            request_type: ServerRequestType::UserInput,
            approved: None,
            result: None,
            content: Some(Vec::new()),
            is_error: Some(false),
            decision_action: None,
            reason: Some("answer".into()),
        },
    )
    .await
    .expect("input response");
    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("response-client-tool"),
        ToAgentMessage::ClientToolResult {
            call_id: "client-call-1".into(),
            content: Vec::new(),
            is_error: false,
        },
    )
    .await
    .expect("client tool response");
    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("response-retry"),
        ToAgentMessage::ServerRequestResponse {
            request_id: "retry-1".into(),
            request_type: ServerRequestType::ToolRetry,
            approved: None,
            result: None,
            content: None,
            is_error: None,
            decision_action: Some(ToolRetryDecisionAction::Retry),
            reason: Some("retry once".into()),
        },
    )
    .await
    .expect("retry response");

    let replay = handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("response-retry"),
        ToAgentMessage::ServerRequestResponse {
            request_id: "retry-1".into(),
            request_type: ServerRequestType::ToolRetry,
            approved: None,
            result: None,
            content: None,
            is_error: None,
            decision_action: Some(ToolRetryDecisionAction::Retry),
            reason: Some("retry once".into()),
        },
    )
    .await
    .expect("replayed retry response");
    let replay_json = match replay {
        ResponseBody::Json { body, .. } => body,
        ResponseBody::Sse { .. } => panic!("unexpected SSE response"),
    };
    assert_eq!(replay_json["replayed"], true);
    let conflict = match handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("response-retry"),
        ToAgentMessage::ClientToolResult {
            call_id: "different-response".into(),
            content: Vec::new(),
            is_error: false,
        },
    )
    .await
    {
        Ok(_) => panic!("reusing a response key for a different payload must conflict"),
        Err(error) => error,
    };
    assert_eq!(conflict.code, HostedRunnerErrorCode::IdempotencyConflict);
    let messages = {
        let messages = executor.messages.lock().expect("recorded responses");
        messages.clone()
    };
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        messages[0],
        ToAgentMessage::ServerRequestResponse {
            request_type: ServerRequestType::UserInput,
            ..
        }
    ));
    assert!(matches!(
        messages[1],
        ToAgentMessage::ClientToolResult { .. }
    ));
    assert!(matches!(
        messages[2],
        ToAgentMessage::ServerRequestResponse {
            request_type: ServerRequestType::ToolRetry,
            ..
        }
    ));

    let prompt = ToAgentMessage::Prompt {
        content: "idempotency headers do not turn prompts into responses".into(),
        attachments: None,
        managed_inference_authorization: None,
    };
    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("prompt-key"),
        prompt.clone(),
    )
    .await
    .expect("prompt with an incidental idempotency header");
    let prompt_replay = handle_message(
        handle.shared.clone(),
        "sess_test",
        headers("prompt-key"),
        prompt,
    )
    .await
    .expect("repeated prompt with an incidental idempotency header");
    let ResponseBody::Json { body, .. } = prompt_replay else {
        panic!("prompt response must be JSON");
    };
    assert_eq!(body["replayed"], false);
    assert_eq!(
        executor.messages.lock().expect("recorded responses").len(),
        5,
        "non-response messages with a key must execute independently"
    );
    handle.shutdown().await;

    let restored_executor = Arc::new(ResponseRecordingExecutor::default());
    let restored = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        restored_executor,
    )
    .await
    .expect("restore hosted runner");
    let (capability, subscription_id) =
        attach_thread_controller(&client, &restored.base_url(), "conn_response_restored").await;
    let replayed: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/messages",
            restored.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_response_restored")
        .header("x-maestro-headless-subscriber-id", &subscription_id)
        .header("x-maestro-headless-connection-capability", &capability)
        .header("x-maestro-idempotency-key", "response-retry")
        .json(&json!({
            "type": "server_request_response",
            "request_id": "retry-1",
            "request_type": "tool_retry",
            "decision_action": "retry",
            "reason": "retry once"
        }))
        .send()
        .await
        .expect("restored response")
        .error_for_status()
        .expect("restored response status")
        .json()
        .await
        .expect("restored response json");
    assert_eq!(replayed["replayed"], true);
    restored.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_response_restarts_pending_and_consumes_once_after_child_exit() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let first_log = fixtures.path().join("first.log");
    let first_script =
        create_response_consumer_script(fixtures.path(), "first-child.sh", &first_log, false, None);
    let first_supervisor = connected_supervisor_for_script(&first_script).await;
    let first_executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &first_supervisor,
    )));
    let first = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        first_executor.clone(),
    )
    .await
    .expect("first hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &first.base_url(), "conn_restart_first").await;
    let headers = |connection_id: &str, capability: &str, subscription_id: &str| {
        [
            (
                "x-maestro-headless-connection-id".to_string(),
                connection_id.to_string(),
            ),
            (
                "x-maestro-headless-subscriber-id".to_string(),
                subscription_id.to_string(),
            ),
            (
                "x-maestro-headless-connection-capability".to_string(),
                capability.to_string(),
            ),
            (
                "x-maestro-idempotency-key".to_string(),
                "restart-key".to_string(),
            ),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>()
    };
    let response = ToAgentMessage::ToolResponse {
        call_id: "restart-call".to_string(),
        tool_execution_id: Some("restart-execution".to_string()),
        approved: true,
        result: None,
    };

    let first_result = handle_message(
        first.shared.clone(),
        "sess_test",
        headers("conn_restart_first", &capability, &subscription_id),
        response.clone(),
    )
    .await
    .expect("queue response in first child");
    let ResponseBody::Json { body, .. } = first_result else {
        panic!("queued response must return JSON");
    };
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("pending native consumption"))
    );
    let duplicate_while_queued = handle_message(
        first.shared.clone(),
        "sess_test",
        headers("conn_restart_first", &capability, &subscription_id),
        response.clone(),
    )
    .await
    .expect("live queued response must reconcile without redispatch");
    let ResponseBody::Json { body, .. } = duplicate_while_queued else {
        panic!("queued reconciliation must return JSON");
    };
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("pending native consumption"))
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    first.shutdown().await;
    first_supervisor
        .lock()
        .expect("first supervisor")
        .shutdown();
    assert_eq!(
        std::fs::read_to_string(&first_log)
            .expect("first child log")
            .lines()
            .count(),
        1
    );

    let second_log = fixtures.path().join("second.log");
    let second_script = create_response_consumer_script(
        fixtures.path(),
        "second-child.sh",
        &second_log,
        true,
        None,
    );
    let second_supervisor = connected_supervisor_for_script(&second_script).await;
    let second_executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &second_supervisor,
    )));
    let second = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()).with_runtime_generation(1),
        second_executor,
    )
    .await
    .expect("restarted hosted runner");
    let (capability, subscription_id) =
        attach_thread_controller(&client, &second.base_url(), "conn_restart_second").await;
    let retry_headers = headers("conn_restart_second", &capability, &subscription_id);

    handle_message(
        second.shared.clone(),
        "sess_test",
        retry_headers.clone(),
        response.clone(),
    )
    .await
    .expect("restart must redispatch pending response");
    let replay = handle_message(second.shared.clone(), "sess_test", retry_headers, response)
        .await
        .expect("completed response replay");
    let ResponseBody::Json { body, .. } = replay else {
        panic!("replay must return JSON");
    };
    assert_eq!(body["replayed"], true);
    assert_eq!(
        std::fs::read_to_string(&second_log)
            .expect("second child log")
            .lines()
            .count(),
        1,
        "restart retry must be consumed exactly once"
    );
    second.shutdown().await;
    second_supervisor
        .lock()
        .expect("second supervisor")
        .shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlated_protocol_rejection_rolls_back_ownership_and_allows_retry() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("rejected-responses.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log_path, None);
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_rejection").await;
    let headers = response_headers(
        "conn_rejection",
        &subscription_id,
        &capability,
        "rejected-key",
    );
    let response = ToAgentMessage::ToolResponse {
        call_id: "retry-call".to_string(),
        tool_execution_id: Some("retry-execution".to_string()),
        approved: true,
        result: None,
    };

    let error = match handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("correlated protocol rejection must not return success"),
    };
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    assert!(error.message.contains("not awaiting a decision"));
    {
        let state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !state
                .pending_response_idempotency
                .contains_key("rejected-key")
        );
    }
    assert!(
        !executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("rejected-key")
    );
    assert!(
        !load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger")
            .iter()
            .any(|(key, _)| key == "rejected-key")
    );

    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    .expect("corrected retry dispatches");
    let replay = handle_message(handle.shared.clone(), "sess_test", headers, response)
        .await
        .expect("accepted retry replays");
    let ResponseBody::Json { body, .. } = replay else {
        panic!("accepted retry replay must return JSON");
    };
    assert_eq!(body["replayed"], true);
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("rejected response log")
            .lines()
            .count(),
        2,
        "one rejected dispatch and one corrected dispatch are expected"
    );
    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_protocol_rejection_after_queued_return_is_rolled_back_by_event_pump() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("delayed-rejection.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log_path, Some("0.65"));
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_delayed_rejection").await;
    let headers = response_headers(
        "conn_delayed_rejection",
        &subscription_id,
        &capability,
        "delayed-rejected-key",
    );
    let response = ToAgentMessage::ToolResponse {
        call_id: "retry-call".to_string(),
        tool_execution_id: Some("delayed-retry-execution".to_string()),
        approved: true,
        result: None,
    };

    let queued = handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    .expect("response is queued before delayed rejection");
    let ResponseBody::Json { body, .. } = queued else {
        panic!("queued response must return JSON");
    };
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("pending native consumption"))
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    {
        let state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !state
                .pending_response_idempotency
                .contains_key("delayed-rejected-key")
        );
    }
    assert!(
        !executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("delayed-rejected-key")
    );
    assert!(
        !executor
            .memory_completed_responses
            .lock()
            .expect("memory completion")
            .contains_key("delayed-rejected-key")
    );
    assert!(
        !load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger")
            .iter()
            .any(|(key, _)| key == "delayed-rejected-key")
    );

    handle_message(handle.shared.clone(), "sess_test", headers, response)
        .await
        .expect("corrected same-key retry dispatches after delayed rollback");
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("delayed rejection log")
            .lines()
            .count(),
        2
    );
    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_rejection_rollback_survives_thread_persistence_failure() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("delayed-rejection-persistence.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log_path, Some("0.65"));
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_rejection_persist").await;
    let headers = response_headers(
        "conn_rejection_persist",
        &subscription_id,
        &capability,
        "rejection-persist-key",
    );
    let response = ToAgentMessage::ToolResponse {
        call_id: "retry-call".to_string(),
        tool_execution_id: Some("rejection-persist-execution".to_string()),
        approved: true,
        result: None,
    };

    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    .expect("response is queued before delayed rejection");
    // The pump's rejection tick persists twice: publishing the correlated
    // protocol Error is a lifecycle boundary, then the rejection rollback
    // persists the removed pending records. Fail both so the rollback path
    // itself observes a journal write failure.
    handle.shared.fail_next_thread_persistences(2);
    tokio::time::sleep(Duration::from_millis(900)).await;

    // The in-memory rollback happened even though the journal write failed,
    // and the event pump must survive to retry the persistence.
    {
        let state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !state
                .pending_response_idempotency
                .contains_key("rejection-persist-key")
        );
    }
    assert!(
        !executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("rejection-persist-key")
    );
    assert!(
        !handle
            .shared
            .event_pump_task
            .lock()
            .await
            .as_ref()
            .expect("event pump task")
            .is_finished()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !handle
                .shared
                .thread_persistence_retry_pending
                .load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("event pump retries the thread journal persistence");
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("delayed rejection log")
            .lines()
            .count(),
        1,
        "only the rejected dispatch reaches the native child in this test"
    );
    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_rejection_survives_ledger_cleanup_failure() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("delayed-rejection-ledger.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log_path, Some("0.65"));
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_rejection_ledger_pump").await;
    let headers = response_headers(
        "conn_rejection_ledger_pump",
        &subscription_id,
        &capability,
        "pump-ledger-key",
    );
    let response = ToAgentMessage::ToolResponse {
        call_id: "retry-call".to_string(),
        tool_execution_id: Some("pump-ledger-execution".to_string()),
        approved: true,
        result: None,
    };

    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    .expect("response is queued before delayed rejection");
    executor.fail_next_ledger_persistences(1);
    tokio::time::sleep(Duration::from_millis(900)).await;

    // The executor drain observed the rejection; the ledger cleanup failure
    // must not fail the drain and kill the event pump.
    assert!(
        !handle
            .shared
            .event_pump_task
            .lock()
            .await
            .as_ref()
            .expect("event pump task")
            .is_finished()
    );
    {
        let state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !state
                .pending_response_idempotency
                .contains_key("pump-ledger-key")
        );
    }
    assert!(
        !executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("pump-ledger-key")
    );
    assert!(
        load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger")
            .iter()
            .any(|(key, dispatched)| key == "pump-ledger-key" && !dispatched)
    );

    handle_message(handle.shared.clone(), "sess_test", headers, response)
        .await
        .expect("corrected same-key retry dispatches despite the stale pending entry");
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("delayed rejection ledger log")
            .lines()
            .count(),
        2
    );
    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_response_releases_ownership_when_ledger_cleanup_fails() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("rejected-ledger-failure.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log_path, None);
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_rejection_ledger").await;
    let headers = response_headers(
        "conn_rejection_ledger",
        &subscription_id,
        &capability,
        "ownership-rejected-key",
    );
    let response = ToAgentMessage::ToolResponse {
        call_id: "retry-call".to_string(),
        tool_execution_id: Some("ownership-retry-execution".to_string()),
        approved: true,
        result: None,
    };

    executor.fail_next_ledger_persistences(1);
    let error = match handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("correlated protocol rejection must not return success"),
    };
    // The caller still sees the rejection, not the internal ledger failure.
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    assert!(error.message.contains("not awaiting a decision"));
    // The in-memory ownership slot is released even though the ledger
    // cleanup failed; only the stale pending entry remains on disk.
    assert!(
        !executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("ownership-rejected-key")
    );
    assert!(
        load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger")
            .iter()
            .any(|(key, dispatched)| key == "ownership-rejected-key" && !dispatched)
    );

    // A same-key retry is admitted (pending entries never dedup) and the
    // accepted retry marks the stale entry dispatched.
    handle_message(
        handle.shared.clone(),
        "sess_test",
        headers.clone(),
        response.clone(),
    )
    .await
    .expect("corrected retry dispatches despite the stale pending entry");
    assert!(
        load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger after retry")
            .iter()
            .any(|(key, dispatched)| key == "ownership-rejected-key" && *dispatched)
    );
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("rejected ledger failure log")
            .lines()
            .count(),
        2
    );
    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

fn executor_context_with_response_key(
    workspace: &Path,
    key: &str,
) -> HostedRunnerHeadlessMessageContext {
    HostedRunnerHeadlessMessageContext {
        session_id: "sess_test".to_string(),
        connection_id: "conn_exec".to_string(),
        subscription_id: Some("sub_exec".to_string()),
        role: ConnectionRole::Controller,
        controller_connection_id: Some("conn_exec".to_string()),
        client_protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: None,
        capabilities: None,
        opt_out_notifications: None,
        lease_expires_at: Utc::now().to_rfc3339(),
        workspace_root: workspace.to_path_buf(),
        response_idempotency_key: Some(key.to_string()),
    }
}

#[test]
fn transport_error_releases_ownership_when_ledger_cleanup_fails() {
    let workspace = tempdir().expect("workspace");
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(
        crate::headless::SupervisorConfig::default(),
    )));
    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(&supervisor));
    let context = executor_context_with_response_key(workspace.path(), "transport-ledger-key");
    let response = ToAgentMessage::ToolResponse {
        call_id: "transport-call".to_string(),
        tool_execution_id: Some("transport-execution".to_string()),
        approved: true,
        result: None,
    };

    executor.fail_next_ledger_persistences(1);
    let error = executor
        .execute(&context, response.clone())
        .expect_err("disconnected supervisor must fail the dispatch");
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeNotReady);
    assert_ne!(error.code, HostedRunnerErrorCode::Internal);
    assert!(
        !executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("transport-ledger-key")
    );
    assert!(
        load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger")
            .iter()
            .any(|(key, dispatched)| key == "transport-ledger-key" && !dispatched)
    );

    let retry = executor
        .execute(&context, response)
        .expect_err("retry is admitted and still fails on the disconnected supervisor");
    assert_eq!(retry.code, HostedRunnerErrorCode::RuntimeNotReady);
    assert_ne!(retry.code, HostedRunnerErrorCode::Internal);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmatched_governed_ack_does_not_finalize_or_redispatch() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("unmatched-governed-ack.log");
    let script = create_delayed_identity_ack_script(
        fixtures.path(),
        "unmatched-governed-ack.sh",
        &log_path,
        "governed_client_tool_result",
        "unrelated-call",
    );
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(&supervisor));
    let context = executor_context_with_response_key(workspace.path(), "identity-owner-key");
    let response = governed_response_for_ack_test();

    let queued = executor
        .execute_async(&context, response.clone())
        .await
        .expect("the consumer has not acknowledged this call");
    assert!(!queued.idempotency_finalized);
    let replay = executor
        .reconcile_pending_async(&context, response)
        .await
        .expect("an unrelated acknowledgment leaves this response queued");
    assert!(!replay.idempotency_finalized);
    assert!(
        executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("identity-owner-key")
    );
    assert!(
        load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("response ledger")
            .iter()
            .any(|(key, dispatched)| key == "identity-owner-key" && !dispatched)
    );
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("child log")
            .lines()
            .count(),
        1,
        "an unacknowledged response must not be dispatched twice"
    );
    supervisor.lock().expect("supervisor").shutdown();
}

#[cfg(unix)]
async fn assert_unique_protocol_request_owner_across_restart(
    message: ToAgentMessage,
    message_type: &str,
    request_id: &str,
) {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let first_log = fixtures.path().join(format!("{message_type}-first.log"));
    let first_script = create_delayed_identity_ack_script(
        fixtures.path(),
        &format!("{message_type}-first.sh"),
        &first_log,
        message_type,
        request_id,
    );
    let first_supervisor = connected_supervisor_for_script(&first_script).await;
    let first_executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &first_supervisor,
    )));
    let first = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        first_executor,
    )
    .await
    .expect("first hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &first.base_url(), "conn_identity_first").await;
    let owner_headers = response_headers(
        "conn_identity_first",
        &subscription_id,
        &capability,
        "identity-owner-key",
    );
    let competing_headers = response_headers(
        "conn_identity_first",
        &subscription_id,
        &capability,
        "identity-competing-key",
    );

    handle_message(
        first.shared.clone(),
        "sess_test",
        owner_headers.clone(),
        message.clone(),
    )
    .await
    .expect("owner response queues");
    let conflict = match handle_message(
        first.shared.clone(),
        "sess_test",
        competing_headers.clone(),
        message.clone(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("second key must not own the same protocol request"),
    };
    assert_eq!(conflict.code, HostedRunnerErrorCode::IdempotencyConflict);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let replay = handle_message(
        first.shared.clone(),
        "sess_test",
        owner_headers,
        message.clone(),
    )
    .await
    .expect("owner key replays after delayed acknowledgement");
    let ResponseBody::Json { body, .. } = replay else {
        panic!("owner replay must return JSON");
    };
    assert_eq!(body["replayed"], true);
    assert_eq!(
        std::fs::read_to_string(&first_log)
            .expect("first child log")
            .lines()
            .count(),
        1
    );
    first.shutdown().await;
    first_supervisor
        .lock()
        .expect("first supervisor")
        .shutdown();

    let second_log = fixtures.path().join(format!("{message_type}-second.log"));
    let second_script = create_delayed_identity_ack_script(
        fixtures.path(),
        &format!("{message_type}-second.sh"),
        &second_log,
        message_type,
        request_id,
    );
    let second_supervisor = connected_supervisor_for_script(&second_script).await;
    let second_executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &second_supervisor,
    )));
    let second = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        second_executor,
    )
    .await
    .expect("restarted hosted runner");
    let second_connection_id =
        if matches!(&message, ToAgentMessage::GovernedClientToolResult { .. }) {
            "conn_identity_first"
        } else {
            "conn_identity_second"
        };
    let (capability, subscription_id) =
        attach_thread_controller(&client, &second.base_url(), second_connection_id).await;
    let replay = handle_message(
        second.shared.clone(),
        "sess_test",
        response_headers(
            second_connection_id,
            &subscription_id,
            &capability,
            "identity-owner-key",
        ),
        message.clone(),
    )
    .await
    .expect("durable owner key replays");
    let ResponseBody::Json { body, .. } = replay else {
        panic!("durable owner replay must return JSON");
    };
    assert_eq!(body["replayed"], true);
    let conflict = match handle_message(
        second.shared.clone(),
        "sess_test",
        response_headers(
            second_connection_id,
            &subscription_id,
            &capability,
            "identity-competing-key",
        ),
        message,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("request ownership must survive restart"),
    };
    assert_eq!(conflict.code, HostedRunnerErrorCode::IdempotencyConflict);
    assert!(
        !second_log.exists(),
        "restart must not redispatch either key"
    );
    second.shutdown().await;
    second_supervisor
        .lock()
        .expect("second supervisor")
        .shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_tool_response_has_one_idempotency_owner_across_restart() {
    assert_unique_protocol_request_owner_across_restart(
        ToAgentMessage::ToolResponse {
            call_id: "unique-tool-call".to_string(),
            tool_execution_id: Some("unique-tool-execution".to_string()),
            approved: true,
            result: None,
        },
        "tool_response",
        "unique-tool-call",
    )
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_server_request_response_has_one_idempotency_owner_across_restart() {
    assert_unique_protocol_request_owner_across_restart(
        ToAgentMessage::ServerRequestResponse {
            request_id: "unique-server-request".to_string(),
            request_type: ServerRequestType::UserInput,
            approved: None,
            result: None,
            content: Some(Vec::new()),
            is_error: Some(false),
            decision_action: None,
            reason: Some("answer".to_string()),
        },
        "server_request_response",
        "unique-server-request",
    )
    .await;
}

#[cfg(unix)]
fn governed_response_for_ack_test() -> ToAgentMessage {
    ToAgentMessage::GovernedClientToolResult {
        process_tool_cost_micros: None,
        call_id: "unique-governed-call".to_string(),
        content: Vec::new(),
        is_error: false,
        tool_execution_id: "unique-governed-execution".to_string(),
        client_instance_id: "conn_identity_first".to_string(),
        grant_id: "grant-1".to_string(),
        grant_version: 1,
        grant_hash: "hash".to_string(),
        turn_digest: "turn-digest".to_string(),
        definition_digest: "definition-digest".to_string(),
        args_digest: "args-digest".to_string(),
        owner_lease_epoch: 1,
        idempotency_key: "identity-owner-key".to_string(),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_governed_result_has_one_idempotency_owner_across_restart() {
    assert_unique_protocol_request_owner_across_restart(
        governed_response_for_ack_test(),
        "governed_client_tool_result",
        "unique-governed-call",
    )
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_ack_event_pump_finalizes_before_restart_without_redispatch() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let first_log = fixtures.path().join("delayed-first.log");
    let first_script = create_response_consumer_script(
        fixtures.path(),
        "delayed-first-child.sh",
        &first_log,
        true,
        Some("0.65"),
    );
    let first_supervisor = connected_supervisor_for_script(&first_script).await;
    let first_executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &first_supervisor,
    )));
    let first = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        first_executor.clone(),
    )
    .await
    .expect("first hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &first.base_url(), "conn_delayed_first").await;
    let response = ToAgentMessage::ToolResponse {
        call_id: "restart-call".to_string(),
        tool_execution_id: Some("delayed-restart-execution".to_string()),
        approved: true,
        result: None,
    };
    let headers = response_headers(
        "conn_delayed_first",
        &subscription_id,
        &capability,
        "delayed-restart-key",
    );

    handle_message(first.shared.clone(), "sess_test", headers, response.clone())
        .await
        .expect("queued delayed response");
    first_executor.fail_next_ledger_persistences(1);
    first.shared.fail_next_thread_persistences(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let memory_empty = first_executor
                .memory_completed_responses
                .lock()
                .expect("memory-completed responses")
                .is_empty();
            let ledger_completed = load_executor_response_ledger(workspace.path(), "sess_test")
                .is_ok_and(|ledger| {
                    ledger
                        .iter()
                        .any(|(key, dispatched)| key == "delayed-restart-key" && *dispatched)
                });
            if memory_empty && ledger_completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("event pump ledger persistence recovery");
    {
        let state = first
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            state
                .response_idempotency_keys
                .contains("delayed-restart-key")
        );
        assert!(
            !state
                .pending_response_idempotency
                .contains_key("delayed-restart-key")
        );
    }
    assert!(
        first_executor
            .memory_completed_responses
            .lock()
            .expect("memory-completed responses")
            .is_empty()
    );
    assert!(
        load_executor_response_ledger(workspace.path(), "sess_test")
            .expect("recovered executor ledger")
            .iter()
            .any(|(key, dispatched)| key == "delayed-restart-key" && *dispatched)
    );
    assert!(
        !first
            .shared
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
    );
    assert!(
        !first
            .shared
            .event_pump_task
            .lock()
            .await
            .as_ref()
            .expect("event pump task")
            .is_finished()
    );
    first.shutdown().await;
    first_supervisor
        .lock()
        .expect("first supervisor")
        .shutdown();
    assert_eq!(
        std::fs::read_to_string(&first_log)
            .expect("first child log")
            .lines()
            .count(),
        1
    );

    let second_log = fixtures.path().join("delayed-second.log");
    let second_script = create_response_consumer_script(
        fixtures.path(),
        "delayed-second-child.sh",
        &second_log,
        true,
        None,
    );
    let second_supervisor = connected_supervisor_for_script(&second_script).await;
    let second_executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &second_supervisor,
    )));
    let second = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        second_executor,
    )
    .await
    .expect("restarted hosted runner");
    let (capability, subscription_id) =
        attach_thread_controller(&client, &second.base_url(), "conn_delayed_second").await;
    let replay = handle_message(
        second.shared.clone(),
        "sess_test",
        response_headers(
            "conn_delayed_second",
            &subscription_id,
            &capability,
            "delayed-restart-key",
        ),
        response,
    )
    .await
    .expect("completed delayed response replay");
    let ResponseBody::Json { body, .. } = replay else {
        panic!("replay must return JSON");
    };
    assert_eq!(body["replayed"], true);
    assert!(
        !second_log.exists(),
        "restart must not redispatch the response"
    );
    second.shutdown().await;
    second_supervisor
        .lock()
        .expect("second supervisor")
        .shutdown();
}

#[tokio::test]
async fn pending_response_capacity_deduplicates_retries_without_evicting_live_key() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("hosted runner");
    let live_message = ToAgentMessage::ToolResponse {
        call_id: "live-call".to_string(),
        tool_execution_id: Some("live-execution".to_string()),
        approved: true,
        result: None,
    };
    {
        let mut state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending_response_idempotency
            .insert("live-key".to_string(), live_message.clone());
        state.pending_response_idempotency_order =
            std::iter::repeat_n("live-key".to_string(), MAX_RESPONSE_IDEMPOTENCY_RECORDS).collect();

        upsert_pending_response_idempotency(
            &mut state,
            "live-key".to_string(),
            live_message.clone(),
        )
        .unwrap();
        for index in 0..(MAX_RESPONSE_IDEMPOTENCY_RECORDS - 1) {
            upsert_pending_response_idempotency(
                &mut state,
                format!("other-key-{index:04}"),
                ToAgentMessage::ToolResponse {
                    call_id: format!("other-call-{index:04}"),
                    tool_execution_id: None,
                    approved: true,
                    result: None,
                },
            )
            .unwrap();
        }

        assert!(state.pending_response_idempotency.contains_key("live-key"));
        assert_eq!(
            state.pending_response_idempotency.len(),
            MAX_RESPONSE_IDEMPOTENCY_RECORDS
        );
        assert_eq!(
            state
                .pending_response_idempotency_order
                .iter()
                .filter(|key| key.as_str() == "live-key")
                .count(),
            1
        );
    }
    handle.shutdown().await;
}

#[test]
fn executor_queue_capacity_rejects_new_ownership_without_evicting_live_keys() {
    let workspace = tempdir().expect("workspace");
    let supervisor = Arc::new(Mutex::new(AgentSupervisor::new(
        crate::headless::SupervisorConfig::default(),
    )));
    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(supervisor);
    {
        let mut queued = executor.queued_responses.lock().expect("queued responses");
        for index in 0..MAX_RESPONSE_IDEMPOTENCY_RECORDS {
            queued.insert(
                format!("live-key-{index}"),
                QueuedResponseOwnership {
                    request_id: format!("live-call-{index}"),
                    transport_generation: 0,
                    workspace_root: workspace.path().to_path_buf(),
                    session_id: "sess_test".to_string(),
                },
            );
        }
    }
    let context = HostedRunnerHeadlessMessageContext {
        session_id: "sess_test".to_string(),
        connection_id: "conn_test".to_string(),
        subscription_id: None,
        role: ConnectionRole::Controller,
        controller_connection_id: Some("conn_test".to_string()),
        client_protocol_version: None,
        client_info: None,
        capabilities: None,
        opt_out_notifications: None,
        lease_expires_at: Utc::now().to_rfc3339(),
        workspace_root: workspace.path().to_path_buf(),
        response_idempotency_key: Some("overflow-key".to_string()),
    };

    let error = executor
        .execute(
            &context,
            ToAgentMessage::ToolResponse {
                call_id: "overflow-call".to_string(),
                tool_execution_id: None,
                approved: true,
                result: None,
            },
        )
        .expect_err("new ownership must be backpressured");

    assert_eq!(error.code, HostedRunnerErrorCode::ResponseCapacity);
    let queued = executor.queued_responses.lock().expect("queued responses");
    assert_eq!(queued.len(), MAX_RESPONSE_IDEMPOTENCY_RECORDS);
    assert!(queued.contains_key("live-key-0"));
    drop(queued);
    executor
        .queued_responses
        .lock()
        .expect("queued responses")
        .clear();
    let live_ledger = (0..MAX_RESPONSE_IDEMPOTENCY_RECORDS)
        .map(|index| (format!("ledger-live-{index}"), false))
        .collect::<Vec<_>>();
    persist_executor_response_ledger(workspace.path(), "sess_test", &live_ledger).unwrap();
    let error = executor
        .execute(
            &context,
            ToAgentMessage::ToolResponse {
                call_id: "ledger-overflow-call".to_string(),
                tool_execution_id: None,
                approved: true,
                result: None,
            },
        )
        .expect_err("live ledger capacity must be backpressured");
    assert_eq!(error.code, HostedRunnerErrorCode::ResponseCapacity);
    let reloaded = load_executor_response_ledger(workspace.path(), "sess_test").unwrap();
    assert_eq!(reloaded.len(), MAX_RESPONSE_IDEMPOTENCY_RECORDS);
    assert!(
        reloaded
            .iter()
            .any(|(key, dispatched)| key == "ledger-live-0" && !*dispatched)
    );
}

#[test]
fn ledger_transaction_prevents_stale_admission_from_overwriting_delayed_ack() {
    let workspace = tempdir().expect("workspace");
    persist_executor_response_ledger(
        workspace.path(),
        "sess_test",
        &[("acknowledged-key".to_string(), false)],
    )
    .unwrap();
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::new(
        Mutex::new(AgentSupervisor::new(
            crate::headless::SupervisorConfig::default(),
        )),
    )));
    let loaded = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    executor.set_ledger_admission_barriers(Arc::clone(&loaded), Arc::clone(&resume));

    let admission_executor = Arc::clone(&executor);
    let admission_workspace = workspace.path().to_path_buf();
    let admission = std::thread::spawn(move || {
        admission_executor.admit_response_key(
            &admission_workspace,
            "sess_test",
            "new-admission-key",
            true,
        )
    });
    loaded.wait();

    let ownership = QueuedResponseOwnership {
        request_id: "acknowledged-request".to_string(),
        transport_generation: 0,
        workspace_root: workspace.path().to_path_buf(),
        session_id: "sess_test".to_string(),
    };
    let acknowledgement_executor = Arc::clone(&executor);
    let (ack_done_tx, ack_done_rx) = std::sync::mpsc::channel();
    let acknowledgement = std::thread::spawn(move || {
        let result =
            acknowledgement_executor.persist_consumed_response("acknowledged-key", &ownership);
        ack_done_tx.send(()).unwrap();
        result
    });
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        ack_done_rx.try_recv().is_err(),
        "delayed acknowledgement must wait for the admission transaction"
    );

    resume.wait();
    assert!(!admission.join().unwrap().unwrap());
    acknowledgement.join().unwrap().unwrap();
    let ledger = load_executor_response_ledger(workspace.path(), "sess_test").unwrap();
    assert!(
        ledger
            .iter()
            .any(|(key, dispatched)| key == "acknowledged-key" && *dispatched)
    );
    assert!(
        ledger
            .iter()
            .any(|(key, dispatched)| key == "new-admission-key" && !*dispatched)
    );

    let restarted = AgentSupervisorHostedRunnerMessageExecutor::new(Arc::new(Mutex::new(
        AgentSupervisor::new(crate::headless::SupervisorConfig::default()),
    )));
    let result = restarted
        .execute(
            &HostedRunnerHeadlessMessageContext {
                session_id: "sess_test".to_string(),
                connection_id: "conn_restart".to_string(),
                subscription_id: None,
                role: ConnectionRole::Controller,
                controller_connection_id: Some("conn_restart".to_string()),
                client_protocol_version: None,
                client_info: None,
                capabilities: None,
                opt_out_notifications: None,
                lease_expires_at: Utc::now().to_rfc3339(),
                workspace_root: workspace.path().to_path_buf(),
                response_idempotency_key: Some("acknowledged-key".to_string()),
            },
            ToAgentMessage::ToolResponse {
                call_id: "acknowledged-request".to_string(),
                tool_execution_id: None,
                approved: true,
                result: None,
            },
        )
        .expect("restart must reconcile the completed ledger without transport");
    assert_eq!(
        result.execution,
        HostedRunnerHeadlessMessageExecution::RuntimeHandled
    );
    assert!(
        restarted
            .queued_responses
            .lock()
            .expect("restarted queued responses")
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_4097_is_backpressured_and_first_live_key_dispatches_once() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("capacity.log");
    // This exercises capacity while the child is alive, not child-exit recovery.
    let script = create_response_consumer_script_with_action(
        fixtures.path(),
        "capacity-child.sh",
        &log_path,
        ":",
    );
    let supervisor = connected_supervisor_for_script(&script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor,
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_capacity").await;
    let first_message = ToAgentMessage::ToolResponse {
        call_id: "restart-call".to_string(),
        tool_execution_id: Some("capacity-first".to_string()),
        approved: true,
        result: None,
    };
    let first_headers = response_headers(
        "conn_capacity",
        &subscription_id,
        &capability,
        "capacity-key-0000",
    );
    handle_message(
        handle.shared.clone(),
        "sess_test",
        first_headers.clone(),
        first_message.clone(),
    )
    .await
    .expect("first queued response");
    {
        let mut state = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for index in 1..MAX_RESPONSE_IDEMPOTENCY_RECORDS {
            let key = format!("capacity-key-{index:04}");
            state.pending_response_idempotency.insert(
                key.clone(),
                ToAgentMessage::ToolResponse {
                    call_id: format!("capacity-call-{index:04}"),
                    tool_execution_id: None,
                    approved: true,
                    result: None,
                },
            );
            state.pending_response_idempotency_order.push_back(key);
        }
    }

    let overflow = match handle_message(
        handle.shared.clone(),
        "sess_test",
        response_headers(
            "conn_capacity",
            &subscription_id,
            &capability,
            "capacity-key-4096",
        ),
        ToAgentMessage::ToolResponse {
            call_id: "capacity-call-4096".to_string(),
            tool_execution_id: None,
            approved: true,
            result: None,
        },
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("4097th distinct live response must be backpressured"),
    };
    assert_eq!(overflow.code, HostedRunnerErrorCode::ResponseCapacity);

    handle_message(
        handle.shared.clone(),
        "sess_test",
        first_headers,
        first_message,
    )
    .await
    .expect("existing key reconciliation remains admitted");
    assert_eq!(
        std::fs::read_to_string(&log_path)
            .expect("capacity child log")
            .lines()
            .count(),
        1,
        "the first live response must not be evicted or redispatched"
    );
    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

#[test]
fn response_ledger_read_errors_fail_closed() {
    let workspace = tempdir().expect("workspace");
    let path = executor_response_ledger_path(workspace.path(), "sess_test");
    std::fs::create_dir_all(path.parent().expect("ledger parent")).expect("ledger directory");
    std::fs::write(&path, [0xff]).expect("invalid ledger fixture");

    let error = load_executor_response_ledger(workspace.path(), "sess_test")
        .expect_err("invalid ledger bytes must not be treated as an empty ledger");
    assert_eq!(error.code, HostedRunnerErrorCode::Internal);
}

#[test]
fn acknowledged_response_ledger_write_failure_is_memory_only() {
    let workspace = tempdir().expect("workspace");
    std::fs::write(workspace.path().join(".maestro"), b"not a directory")
        .expect("blocking ledger parent");

    let executor = AgentSupervisorHostedRunnerMessageExecutor::new(Arc::new(Mutex::new(
        AgentSupervisor::new(crate::headless::SupervisorConfig::default()),
    )));
    let ownership = QueuedResponseOwnership {
        request_id: "request-acknowledged".to_string(),
        transport_generation: 0,
        workspace_root: workspace.path().to_path_buf(),
        session_id: "sess_acknowledged".to_string(),
    };
    assert!(
        executor
            .persist_consumed_response("response-key", &ownership)
            .is_err()
    );
}

#[tokio::test]
async fn durable_thread_rejects_stale_runtime_generation() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_stale_thread").await;

    let response = client
        .post(format!(
            "{}/api/headless/threads/mrs_test/turns",
            handle.base_url()
        ))
        .header("x-maestro-headless-connection-id", "conn_stale_thread")
        .header("x-maestro-headless-subscriber-id", subscription_id)
        .header("x-maestro-headless-connection-capability", capability)
        .header("x-maestro-runtime-generation", "1")
        .json(&json!({
            "protocolVersion": "evalops.maestro.thread.v1",
            "turnId": "turn-stale",
            "kind": "user_message",
            "content": "must not run"
        }))
        .send()
        .await
        .expect("stale generation response");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    handle.shutdown().await;
}

#[tokio::test]
async fn durable_thread_only_accepts_steering_while_a_turn_is_active() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(PendingThreadExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_steering").await;
    let post = |turn_id: &str, kind: &str, content: &str| {
        client
            .post(format!(
                "{}/api/headless/threads/mrs_test/turns",
                handle.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_steering")
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&json!({
                "protocolVersion": "evalops.maestro.thread.v1",
                "turnId": turn_id,
                "kind": kind,
                "content": content
            }))
            .send()
    };

    let first: serde_json::Value = post("turn-active", "user_message", "work for a while")
        .await
        .expect("active turn response")
        .error_for_status()
        .expect("active turn status")
        .json()
        .await
        .expect("active turn json");
    assert_eq!(first["phase"], "running");

    let unrelated = post("turn-unrelated", "user_message", "start something else")
        .await
        .expect("unrelated turn response");
    assert_eq!(unrelated.status(), StatusCode::CONFLICT);

    let steer: serde_json::Value = post("turn-steer", "steer", "also inspect the logs")
        .await
        .expect("steer response")
        .error_for_status()
        .expect("steer status")
        .json()
        .await
        .expect("steer json");
    assert_eq!(steer["phase"], "running");
    assert_eq!(
        executor.prompts.lock().expect("pending prompts").as_slice(),
        ["work for a while", "also inspect the logs"]
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn durable_thread_requires_turn_completed_after_response_end_for_active_run_and_steers() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(SteeringLifecycleExecutor::default());
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_steer_completion").await;
    let post = |turn_id: &str, kind: &str, content: &str| {
        client
            .post(format!(
                "{}/api/headless/threads/mrs_test/turns",
                handle.base_url()
            ))
            .header("x-maestro-headless-connection-id", "conn_steer_completion")
            .header("x-maestro-headless-subscriber-id", &subscription_id)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&json!({
                "protocolVersion": "evalops.maestro.thread.v1",
                "turnId": turn_id,
                "kind": kind,
                "content": content
            }))
            .send()
    };

    post("turn-active", "user_message", "work for a while")
        .await
        .expect("active turn response")
        .error_for_status()
        .expect("active turn status");
    post("turn-steer", "steer", "also inspect the logs")
        .await
        .expect("steer response")
        .error_for_status()
        .expect("steer status");

    executor.end_model_response();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let running: serde_json::Value = client
        .get(format!(
            "{}/api/headless/threads/mrs_test",
            handle.base_url()
        ))
        .send()
        .await
        .expect("running thread response")
        .error_for_status()
        .expect("running thread status")
        .json()
        .await
        .expect("running thread json");
    assert_eq!(running["phase"], "running");

    executor.complete_active_run();
    let thread = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let thread: serde_json::Value = client
                .get(format!(
                    "{}/api/headless/threads/mrs_test",
                    handle.base_url()
                ))
                .send()
                .await
                .expect("thread response")
                .error_for_status()
                .expect("thread status")
                .json()
                .await
                .expect("thread json");
            if thread["phase"] == "completed" {
                break thread;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("thread completion timeout");

    assert!(thread["active_turn_id"].is_null());
    assert_eq!(thread["turns"][0]["phase"], "completed");
    assert_eq!(thread["turns"][1]["phase"], "completed");

    handle.shutdown().await;
}

#[tokio::test]
async fn durable_thread_generation_lock_fences_overlapping_and_stale_runtimes() {
    let workspace = tempdir().expect("workspace");
    let first_config = test_config(workspace.path().to_path_buf()).with_runtime_generation(1);
    let first = start_hosted_runner(first_config)
        .await
        .expect("start first generation");

    let overlapping = match start_hosted_runner(
        test_config(workspace.path().to_path_buf()).with_runtime_generation(2),
    )
    .await
    {
        Ok(handle) => {
            handle.shutdown().await;
            panic!("new generation must not overlap a live journal writer");
        }
        Err(error) => error,
    };
    assert!(
        matches!(
            overlapping.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
        ),
        "unexpected overlapping generation error: {overlapping}"
    );

    first.shutdown().await;
    let second =
        start_hosted_runner(test_config(workspace.path().to_path_buf()).with_runtime_generation(2))
            .await
            .expect("start replacement generation after old writer exits");
    second.shutdown().await;

    let stale = match start_hosted_runner(
        test_config(workspace.path().to_path_buf()).with_runtime_generation(1),
    )
    .await
    {
        Ok(handle) => {
            handle.shutdown().await;
            panic!("older generation must not reclaim a newer journal");
        }
        Err(error) => error,
    };
    assert_eq!(stale.kind(), std::io::ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn connection_only_controller_disconnect_requires_exact_private_capability() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner(test_config(workspace.path().to_path_buf()))
        .await
        .expect("start hosted runner");
    let client = reqwest::Client::new();

    let controller: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_cleanup",
            "connectionCapabilityRequired": true,
            "role": "controller"
        }))
        .send()
        .await
        .expect("controller connection response")
        .json()
        .await
        .expect("controller connection json");
    let connection_capability = controller["connection_capability"]
        .as_str()
        .expect("connection capability");

    for body in [
        json!({"connectionId": "conn_cleanup"}),
        json!({
            "connectionId": "conn_cleanup",
            "connectionCapability": "cap_00000000000000000000000000000000"
        }),
    ] {
        let rejected = client
            .post(format!(
                "{}/api/headless/sessions/sess_test/disconnect",
                handle.base_url()
            ))
            .json(&body)
            .send()
            .await
            .expect("rejected disconnect response");
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    }

    let retained: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("retained state response")
        .json()
        .await
        .expect("retained state json");
    assert_eq!(
        retained["state"]["controller_connection_id"],
        "conn_cleanup"
    );
    assert_eq!(retained["state"]["connection_count"], 1);

    let disconnected: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/disconnect",
            handle.base_url()
        ))
        .json(&json!({
            "connectionId": "conn_cleanup",
            "connectionCapability": connection_capability
        }))
        .send()
        .await
        .expect("authorized disconnect response")
        .json()
        .await
        .expect("authorized disconnect json");
    assert_eq!(disconnected["success"], true);

    let state: serde_json::Value = client
        .get(format!(
            "{}/api/headless/sessions/sess_test/state",
            handle.base_url()
        ))
        .send()
        .await
        .expect("disconnected state response")
        .json()
        .await
        .expect("disconnected state json");
    assert!(state["state"]["controller_connection_id"].is_null());
    assert_eq!(state["state"]["connection_count"], 0);

    handle.shutdown().await;
}

/// A controller reaching the agent through the hosted runner must clear the
/// same protocol-version bar as one on the stdio path; otherwise the handshake
/// guarantee only holds for direct spawns.
#[tokio::test]
async fn hosted_hello_rejects_a_client_protocol_version_this_build_does_not_serve() {
    let workspace = tempdir().expect("workspace");
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()).with_auth_token("controller-test-token"),
        Arc::new(ScriptedRuntimeExecutor),
    )
    .await
    .expect("start hosted runner");
    let client = reqwest::Client::new();

    let connection: serde_json::Value = client
        .post(format!("{}/api/headless/connections", handle.base_url()))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "sessionId": "sess_test",
            "connectionId": "conn_version",
            "role": "controller",
        }))
        .send()
        .await
        .expect("controller connection")
        .json()
        .await
        .expect("controller connection json");
    let subscription: serde_json::Value = client
        .post(format!(
            "{}/api/headless/sessions/sess_test/subscribe",
            handle.base_url()
        ))
        .header("authorization", "Bearer controller-test-token")
        .json(&json!({
            "connectionId": "conn_version",
            "connectionCapability": connection["connection_capability"],
            "connectionCapabilityRequired": true,
            "role": "controller",
        }))
        .send()
        .await
        .expect("subscription response")
        .json()
        .await
        .expect("subscription json");
    let subscription_id = subscription["subscription_id"]
        .as_str()
        .expect("subscription id");
    let connection_capability = subscription["connection_capability"]
        .as_str()
        .expect("connection capability");

    let hello = |protocol_version: &'static str| {
        client
            .post(format!(
                "{}/api/headless/sessions/sess_test/messages",
                handle.base_url()
            ))
            .header("authorization", "Bearer controller-test-token")
            .header("x-maestro-headless-connection-id", "conn_version")
            .header("x-maestro-headless-subscriber-id", subscription_id)
            .header(
                "x-maestro-headless-connection-capability",
                connection_capability,
            )
            .json(&json!({"type": "hello", "protocol_version": protocol_version}))
            .send()
    };

    let rejected = hello("2026-08-01")
        .await
        .expect("previous-version hello response");
    assert_eq!(rejected.status(), StatusCode::NOT_IMPLEMENTED);
    let rejected: serde_json::Value = rejected.json().await.expect("rejected hello json");
    assert_eq!(rejected["code"], "unsupported_capability");
    assert!(
        rejected["error"]
            .as_str()
            .expect("rejection message")
            .contains(crate::headless::HEADLESS_PROTOCOL_VERSION),
        "the rejection must name the versions this build serves: {rejected}"
    );

    let accepted = hello(crate::headless::HEADLESS_PROTOCOL_VERSION)
        .await
        .expect("accepted hello response");
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted: serde_json::Value = accepted.json().await.expect("accepted hello json");
    assert_eq!(accepted["ok"], true);

    handle.shutdown().await;
}

#[test]
fn connection_create_rejects_unsupported_protocol_version() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));

    let request = ConnectionCreateRequest {
        protocol_version: Some("1999-01-01".to_string()),
        client_info: None,
        session_id: Some("sess_test".to_string()),
        connection_id: Some("conn_unsupported".to_string()),
        connection_capability: None,
        connection_capability_required: false,
        _thinking_level: None,
        capabilities: None,
        opt_out_notifications: Vec::new(),
        role: Some(ConnectionRole::Controller),
        take_control: false,
    };

    match handle_connection_create(shared, request) {
        Ok(_) => panic!("connection create must reject an unsupported protocol version"),
        Err(error) => assert_eq!(error.code, HostedRunnerErrorCode::UnsupportedCapability),
    }
}

#[test]
fn connection_and_subscription_advertise_only_configured_ed25519_verifier() {
    use base64::Engine as _;

    const ENV: &str = "MAESTRO_PLATFORM_TOOL_GRANT_ED25519_PUBLIC_KEYS";
    let _guard = crate::headless_server::GOVERNED_GRANT_ENV_LOCK
        .lock()
        .unwrap();
    let previous = std::env::var_os(ENV);
    let configured = serde_json::json!({
        "active": {
            "algorithm": "ed25519",
            "public_key": "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
            "state": "active"
        }
    })
    .to_string();
    // SAFETY: this test holds the shared lock for this dedicated env key and
    // restores its prior value before releasing the lock.
    unsafe { std::env::set_var(ENV, configured) };

    let workspace = tempdir().expect("workspace");
    let current_agent_state = AgentState {
        server_capabilities: Some(native_server_capabilities()),
        ..AgentState::default()
    };
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(current_agent_state)),
        None,
    );
    let connection = handle_connection_create(
        shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_capable".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("capable connection response");
    let ResponseBody::Json {
        body: connection, ..
    } = connection
    else {
        panic!("connection response must be JSON")
    };
    assert_eq!(
        connection["governed_tool_grant_algorithms"],
        serde_json::json!(["ed25519"])
    );
    let capability = connection["connection_capability"]
        .as_str()
        .expect("connection capability")
        .to_string();

    let subscription = handle_subscribe(
        shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_capable".to_string()),
            subscription_id: None,
            connection_capability: Some(capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("capable subscription response");
    let ResponseBody::Json {
        body: subscription, ..
    } = subscription
    else {
        panic!("subscription response must be JSON")
    };
    assert_eq!(
        subscription["governed_tool_grant_algorithms"],
        serde_json::json!(["ed25519"])
    );

    // A newer hosted wrapper must report the exact attached child's verifier
    // capability. An older child deserializes without the new field, so its
    // empty default keeps both hosted response boundaries on v1 even though
    // the wrapper environment itself contains a valid public key.
    let legacy_capabilities: ServerCapabilities = serde_json::from_value(serde_json::json!({
        "server_requests": [],
        "native_tools": []
    }))
    .expect("older child capabilities remain wire compatible");
    let legacy_agent_state = AgentState {
        server_capabilities: Some(legacy_capabilities),
        ..AgentState::default()
    };
    let workspace = tempdir().expect("older child workspace");
    let legacy_child_shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(StatefulRuntimeExecutor::new(legacy_agent_state)),
        None,
    );
    let legacy_child_connection = handle_connection_create(
        legacy_child_shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_older_child".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("older child connection response");
    let ResponseBody::Json {
        body: legacy_child_connection,
        ..
    } = legacy_child_connection
    else {
        panic!("older child connection response must be JSON")
    };
    assert_eq!(
        legacy_child_connection["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );
    let legacy_child_capability = legacy_child_connection["connection_capability"]
        .as_str()
        .expect("older child connection capability")
        .to_string();
    let legacy_child_subscription = handle_subscribe(
        legacy_child_shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_older_child".to_string()),
            subscription_id: None,
            connection_capability: Some(legacy_child_capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("older child subscription response");
    let ResponseBody::Json {
        body: legacy_child_subscription,
        ..
    } = legacy_child_subscription
    else {
        panic!("older child subscription response must be JSON")
    };
    assert_eq!(
        legacy_child_subscription["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );

    // A blank signing key id cannot be selected by any valid grant and must
    // never advertise governed v2 on either response boundary.
    unsafe {
        std::env::set_var(
            ENV,
            serde_json::json!({
                "  ": {
                    "algorithm": "ed25519",
                    "public_key": base64::engine::general_purpose::STANDARD.encode([8_u8; 32]),
                    "state": "active"
                }
            })
            .to_string(),
        );
    };
    let workspace = tempdir().expect("legacy workspace");
    let legacy_shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let legacy = handle_connection_create(
        legacy_shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_legacy".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("legacy-compatible response");
    let ResponseBody::Json { body: legacy, .. } = legacy else {
        panic!("legacy response must be JSON")
    };
    assert_eq!(
        legacy["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );
    let legacy_capability = legacy["connection_capability"]
        .as_str()
        .expect("legacy connection capability")
        .to_string();
    let legacy_subscription = handle_subscribe(
        legacy_shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_legacy".to_string()),
            subscription_id: None,
            connection_capability: Some(legacy_capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("legacy-compatible subscription response");
    let ResponseBody::Json {
        body: legacy_subscription,
        ..
    } = legacy_subscription
    else {
        panic!("legacy subscription response must be JSON")
    };
    assert_eq!(
        legacy_subscription["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );

    // Length and canonical point decoding are not enough: a weak small-order
    // point must fail at load time and cannot be advertised by either response.
    let mut weak_public_key = [0_u8; 32];
    weak_public_key[0] = 1;
    unsafe {
        std::env::set_var(
            ENV,
            serde_json::json!({
                "active": {
                    "algorithm": "ed25519",
                    "public_key": base64::engine::general_purpose::STANDARD.encode(weak_public_key),
                    "state": "active"
                }
            })
            .to_string(),
        );
    };
    let workspace = tempdir().expect("weak point workspace");
    let weak_point_shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));
    let weak_point = handle_connection_create(
        weak_point_shared.clone(),
        ConnectionCreateRequest {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            session_id: Some("sess_test".to_string()),
            connection_id: Some("conn_weak_point".to_string()),
            connection_capability: None,
            connection_capability_required: true,
            _thinking_level: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("weak point remains legacy compatible");
    let ResponseBody::Json {
        body: weak_point, ..
    } = weak_point
    else {
        panic!("weak point response must be JSON")
    };
    assert_eq!(
        weak_point["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );
    let weak_point_capability = weak_point["connection_capability"]
        .as_str()
        .expect("weak point connection capability")
        .to_string();
    let weak_point_subscription = handle_subscribe(
        weak_point_shared,
        "sess_test",
        SubscribeRequest {
            connection_id: Some("conn_weak_point".to_string()),
            subscription_id: None,
            connection_capability: Some(weak_point_capability),
            connection_capability_required: true,
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            client_info: None,
            capabilities: None,
            opt_out_notifications: Vec::new(),
            role: Some(ConnectionRole::Controller),
            take_control: true,
        },
    )
    .expect("weak point subscription remains legacy compatible");
    let ResponseBody::Json {
        body: weak_point_subscription,
        ..
    } = weak_point_subscription
    else {
        panic!("weak point subscription response must be JSON")
    };
    assert_eq!(
        weak_point_subscription["governed_tool_grant_algorithms"],
        serde_json::json!([])
    );

    unsafe {
        match previous {
            Some(value) => std::env::set_var(ENV, value),
            None => std::env::remove_var(ENV),
        }
    }
}

#[test]
fn subscribe_rejects_unsupported_protocol_version() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));

    let request = SubscribeRequest {
        connection_id: Some("conn_unsupported".to_string()),
        subscription_id: None,
        connection_capability: None,
        connection_capability_required: false,
        protocol_version: Some("1999-01-01".to_string()),
        client_info: None,
        capabilities: None,
        opt_out_notifications: Vec::new(),
        role: Some(ConnectionRole::Controller),
        take_control: false,
    };

    match handle_subscribe(shared, "sess_test", request) {
        Ok(_) => panic!("subscribe must reject an unsupported protocol version"),
        Err(error) => assert_eq!(error.code, HostedRunnerErrorCode::UnsupportedCapability),
    }
}

#[test]
fn subscribe_accepts_supported_protocol_version() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new(test_config(workspace.path().to_path_buf()));

    let request = SubscribeRequest {
        connection_id: Some("conn_supported".to_string()),
        subscription_id: None,
        connection_capability: None,
        connection_capability_required: false,
        protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
        client_info: None,
        capabilities: None,
        opt_out_notifications: Vec::new(),
        role: Some(ConnectionRole::Controller),
        take_control: false,
    };

    match handle_subscribe(shared, "sess_test", request) {
        Ok(_) => {}
        Err(error) => panic!("subscribe must accept the current protocol version: {error:?}"),
    }
}

#[cfg(unix)]
async fn assert_unkeyed_delayed_response_releases_acknowledgement(script: &Path, call_id: &str) {
    let workspace = tempdir().expect("workspace");
    let supervisor = connected_supervisor_for_script(script).await;
    let executor = Arc::new(AgentSupervisorHostedRunnerMessageExecutor::new(Arc::clone(
        &supervisor,
    )));
    let handle = start_hosted_runner_with_message_executor(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
    )
    .await
    .expect("hosted runner");
    let client = reqwest::Client::new();
    let (capability, subscription_id) =
        attach_thread_controller(&client, &handle.base_url(), "conn_unkeyed").await;
    let mut request_headers = response_headers(
        "conn_unkeyed",
        &subscription_id,
        &capability,
        "removed-before-send",
    );
    request_headers.remove("x-maestro-idempotency-key");

    handle_message(
        handle.shared.clone(),
        "sess_test",
        request_headers,
        ToAgentMessage::ToolResponse {
            call_id: call_id.to_string(),
            tool_execution_id: Some(format!("{call_id}-execution")),
            approved: true,
            result: None,
        },
    )
    .await
    .expect("queue unkeyed delayed response");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let _ = executor.drain().expect("drain response outcome");
            if executor
                .queued_unkeyed_responses
                .lock()
                .expect("queued unkeyed responses")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("unkeyed response acknowledgement cleanup");
    assert_eq!(
        supervisor
            .lock()
            .expect("supervisor")
            .response_acknowledgement_count(),
        0
    );

    handle.shutdown().await;
    supervisor.lock().expect("supervisor").shutdown();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_unkeyed_acceptance_releases_acknowledgement_capacity() {
    let fixtures = tempdir().expect("fixtures");
    let log = fixtures.path().join("accepted.log");
    let script = create_response_consumer_script(
        fixtures.path(),
        "accept-unkeyed.sh",
        &log,
        true,
        Some("0.65"),
    );
    assert_unkeyed_delayed_response_releases_acknowledgement(&script, "restart-call").await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_unkeyed_rejection_releases_acknowledgement_capacity() {
    let fixtures = tempdir().expect("fixtures");
    let log = fixtures.path().join("rejected.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log, Some("0.65"));
    assert_unkeyed_delayed_response_releases_acknowledgement(&script, "retry-call").await;
}

/// Executor whose `drain()` returns a scripted sequence of results, so pump
/// ticks can be driven deterministically without a child process or the
/// `EVENT_PUMP_INTERVAL` timer.
struct ScriptedDrainExecutor {
    results: Mutex<std::collections::VecDeque<HostedRunnerDrainResult>>,
}

impl ScriptedDrainExecutor {
    fn new(results: Vec<HostedRunnerDrainResult>) -> Self {
        Self {
            results: Mutex::new(results.into()),
        }
    }
}

impl HostedRunnerHeadlessMessageExecutor for ScriptedDrainExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "scripted",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        Ok(self
            .results
            .lock()
            .expect("scripted drain results")
            .pop_front()
            .unwrap_or_default())
    }
}

fn scripted_shared(results: Vec<HostedRunnerDrainResult>, workspace: &Path) -> SharedRunner {
    SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.to_path_buf()),
        Arc::new(ScriptedDrainExecutor::new(results)),
        None,
    )
}

fn seed_pending_response(shared: &SharedRunner, key: &str, request_id: &str) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.pending_response_idempotency.insert(
        key.to_string(),
        ToAgentMessage::ToolResponse {
            call_id: "matrix-call".to_string(),
            tool_execution_id: None,
            approved: true,
            result: None,
        },
    );
    state
        .pending_response_idempotency_order
        .push_back(key.to_string());
    state
        .response_request_owners
        .insert(request_id.to_string(), key.to_string());
}

/// Fault matrix: for each response outcome, a thread-journal write failure at
/// the pump's persistence point must leave the pump running, apply the
/// in-memory transition, arm the retry flag, and clear it on the next tick.
#[tokio::test]
async fn pump_tick_survives_journal_failure_for_each_response_outcome() {
    for outcome in ["consumed", "rejected"] {
        let workspace = tempdir().expect("workspace");
        let drained = match outcome {
            "consumed" => HostedRunnerDrainResult {
                consumed_response_keys: vec!["matrix-key".to_string()],
                ..Default::default()
            },
            _ => HostedRunnerDrainResult {
                rejected_response_keys: vec!["matrix-key".to_string()],
                ..Default::default()
            },
        };
        let shared = scripted_shared(vec![drained], workspace.path());
        seed_pending_response(&shared, "matrix-key", "matrix-request");
        // Two injected failures: the first defers the helper's write, the
        // second fails the same tick's in-line retry, so the flag stays armed
        // across the tick boundary and the next tick must recover.
        shared.fail_next_thread_persistences(2);

        assert_eq!(
            pump_tick(&shared),
            PumpTick::Continue,
            "{outcome}: a journal write failure must not stop the pump"
        );
        {
            let state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                !state
                    .pending_response_idempotency
                    .contains_key("matrix-key"),
                "{outcome}: pending record must be transitioned in memory"
            );
            match outcome {
                "consumed" => {
                    assert!(state.response_idempotency_keys.contains("matrix-key"));
                    assert!(state.response_request_owners.contains_key("matrix-request"));
                }
                _ => {
                    assert!(!state.response_idempotency_keys.contains("matrix-key"));
                    assert!(
                        !state.response_request_owners.contains_key("matrix-request"),
                        "rejected: request ownership must be rolled back"
                    );
                }
            }
        }
        assert!(
            shared
                .thread_persistence_retry_pending
                .load(Ordering::Acquire),
            "{outcome}: the failed write must arm the retry flag"
        );

        assert_eq!(pump_tick(&shared), PumpTick::Continue);
        assert!(
            !shared
                .thread_persistence_retry_pending
                .load(Ordering::Acquire),
            "{outcome}: the next tick must retry the journal write to completion"
        );
    }
}

#[tokio::test]
async fn pump_tick_stops_on_executor_drain_error_and_when_draining() {
    struct FailingDrainExecutor;
    impl HostedRunnerHeadlessMessageExecutor for FailingDrainExecutor {
        fn execute(
            &self,
            _context: &HostedRunnerHeadlessMessageContext,
            _message: ToAgentMessage,
        ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
            Ok(HostedRunnerHeadlessMessageResult::transport_only(
                Vec::new(),
                "scripted",
            ))
        }
        fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
            Err(HostedRunnerError::internal("scripted drain failure"))
        }
    }

    let workspace = tempdir().expect("workspace");
    let failing = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(FailingDrainExecutor),
        None,
    );
    assert_eq!(pump_tick(&failing), PumpTick::Stop);

    let workspace = tempdir().expect("workspace");
    let draining = scripted_shared(Vec::new(), workspace.path());
    draining
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .draining = true;
    assert_eq!(pump_tick(&draining), PumpTick::Stop);
}

#[tokio::test]
async fn drain_start_persistence_failure_is_synchronous_and_retryable() {
    let workspace = tempdir().expect("workspace");
    let shared = scripted_shared(Vec::new(), workspace.path());
    shared.fail_next_thread_persistences(1);

    let error = match handle_drain(
        shared.clone(),
        DrainRequest {
            reason: Some("start-write-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("drain must fail before stopping the event pump"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);

    {
        let state = shared.state.lock().expect("runner state");
        assert!(state.ready, "failed drain start must roll back readiness");
        assert!(!state.draining, "failed drain start must remain retryable");
        assert_eq!(state.last_status.as_deref(), Some("Ready"));
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Ready)
        );
    }
    assert!(
        !shared.event_pump_cancellation.is_cancelled(),
        "the event pump must not be stopped before the draining boundary is durable"
    );
    assert!(
        !shared
            .thread_persistence_retry_pending
            .load(Ordering::Acquire)
    );

    let response = handle_drain(
        shared,
        DrainRequest {
            reason: Some("retry-after-start-write-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("a retry must be able to start the drain");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
}

#[test]
fn durable_draining_boundary_is_recovered_before_executor_handoff() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        None,
    );
    {
        let mut state = shared.state.lock().expect("runner state");
        state.ready = false;
        state.draining = true;
        state.last_status = Some("Draining".to_string());
        shared
            .persist_receipt_boundary(&mut state, RuntimeReceiptKind::Draining, "test")
            .expect("draining boundary must be durable");
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Draining)
        );
        assert!(state.envelopes.iter().any(|envelope| {
            matches!(
                envelope,
                StreamEnvelope::Snapshot { snapshot }
                    if snapshot.state.last_status.as_deref() == Some("Draining")
            )
        }));
    }
    drop(shared);

    let restarted = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        None,
    );
    let state = restarted.state.lock().expect("restarted runner state");
    assert!(
        !state.ready,
        "a restarted draining generation is not attachable"
    );
    assert!(state.draining);
    assert!(state.drain_executor_pending);
    assert_eq!(state.last_status.as_deref(), Some("Draining"));
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Draining)
    );
}

#[test]
fn draining_terminal_messages_keep_the_draining_receipt_until_finalization() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    {
        let mut state = shared.state.lock().expect("runner state");
        let cursor = state.cursor;
        state.thread.append(
            AppendTurnRequest {
                protocol_version: THREAD_PROTOCOL_VERSION.to_string(),
                turn_id: "draining-terminal-turn".to_string(),
                kind: ThreadTurnKind::UserMessage,
                content: "finish while draining".to_string(),
                attachments: None,
                code_mode: None,
                tool_grant: None,
                managed_inference_authorization: None,
            },
            cursor,
        );
        state.thread.mark_dispatched(cursor.saturating_add(1));
        shared
            .persist_thread_for_request(&state)
            .expect("active turn must be durable before drain");
        state.ready = false;
        state.draining = true;
        state.last_status = Some("Draining".to_string());
        shared
            .persist_receipt_boundary(&mut state, RuntimeReceiptKind::Draining, "test")
            .expect("draining boundary must be durable");

        shared.publish_message(
            &mut state,
            FromAgentMessage::TurnCompleted {
                response_id: "draining-terminal-response".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            },
        );

        assert_eq!(state.last_status.as_deref(), Some("Draining"));
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Draining)
        );
    }

    drop(shared);
    let restarted = SharedRunner::new(config);
    let restarted_receipt = restarted
        .runtime_receipt()
        .expect("restart must retain the draining boundary");
    assert_eq!(restarted_receipt.kind, RuntimeReceiptKind::Draining);
    let state = restarted.state.lock().expect("restarted runner state");
    assert!(state.draining);
    assert!(!state.ready);
}

struct FailOnceDrainExecutor {
    failed: AtomicBool,
}

impl HostedRunnerHeadlessMessageExecutor for FailOnceDrainExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "scripted",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        if !self.failed.swap(true, Ordering::AcqRel) {
            Err(HostedRunnerError::internal(
                "scripted retryable drain failure",
            ))
        } else {
            Ok(HostedRunnerDrainResult::default())
        }
    }
}

#[tokio::test]
async fn executor_drain_failure_preserves_a_retryable_draining_phase() {
    let workspace = tempdir().expect("workspace");
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        Arc::new(FailOnceDrainExecutor {
            failed: AtomicBool::new(false),
        }),
        None,
    );

    let error = match handle_drain(
        shared.clone(),
        DrainRequest {
            reason: Some("retryable-executor-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("the first executor drain should fail"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::Internal);
    {
        let state = shared.state.lock().expect("runner state");
        assert!(!state.ready);
        assert!(state.draining);
        assert!(state.drain_executor_pending);
        assert!(!state.drain_finalization_pending);
        assert_eq!(state.last_status.as_deref(), Some("Draining"));
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Draining)
        );
    }

    let response = handle_drain(
        shared,
        DrainRequest {
            reason: Some("retryable-executor-failure-retry".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("the executor drain should be retryable");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
}

struct FlushOnceAfterDrainExecutor {
    drain_calls: Arc<AtomicUsize>,
    fail_flush_once: Arc<AtomicBool>,
}

impl HostedRunnerHeadlessMessageExecutor for FlushOnceAfterDrainExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "scripted",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        self.drain_calls.fetch_add(1, Ordering::AcqRel);
        Ok(HostedRunnerDrainResult::default())
    }

    fn flush_session(&self) -> Result<Option<PathBuf>, HostedRunnerError> {
        if self.fail_flush_once.swap(false, Ordering::AcqRel) {
            return Err(HostedRunnerError::internal(
                "scripted snapshot flush failure",
            ));
        }
        Ok(None)
    }
}

#[tokio::test]
async fn persisted_executor_complete_drain_resumes_without_second_executor_drain() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let drain_calls = Arc::new(AtomicUsize::new(0));
    let fail_flush_once = Arc::new(AtomicBool::new(true));
    let first = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(FlushOnceAfterDrainExecutor {
            drain_calls: drain_calls.clone(),
            fail_flush_once: fail_flush_once.clone(),
        }),
        None,
    );

    let error = match handle_drain(
        first.clone(),
        DrainRequest {
            reason: Some("persisted-executor-complete-phase".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("the scripted manifest flush must fail after executor drain"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::Internal);
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    {
        let state = first.state.lock().expect("first runner state");
        assert!(state.drain_finalization_pending);
        assert!(!state.drain_executor_pending);
        assert_eq!(
            state.last_status.as_deref(),
            Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS)
        );
        assert!(state.envelopes.iter().any(|envelope| {
            matches!(
                envelope,
                StreamEnvelope::Snapshot { snapshot }
                    if snapshot.state.last_status.as_deref()
                        == Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS)
            )
        }));
    }
    drop(first);

    let restarted = SharedRunner::new_with_message_executor_and_restore(
        config,
        Arc::new(FlushOnceAfterDrainExecutor {
            drain_calls: drain_calls.clone(),
            fail_flush_once,
        }),
        None,
    );
    {
        let state = restarted.state.lock().expect("restarted runner state");
        assert!(state.drain_finalization_pending);
        assert!(!state.drain_executor_pending);
        assert_eq!(
            state.last_status.as_deref(),
            Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS)
        );
    }

    let response = handle_drain(
        restarted.clone(),
        DrainRequest {
            reason: Some("resume-persisted-executor-complete-phase".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("finalization retry should not drain the executor again");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    let state = restarted.state.lock().expect("final runner state");
    assert!(!state.drain_finalization_pending);
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Drained)
    );
}

struct FatalDrainFlushOnCallExecutor {
    drain_calls: Arc<AtomicUsize>,
    flush_calls: Arc<AtomicUsize>,
    messages: Mutex<Option<Vec<FromAgentMessage>>>,
}

impl HostedRunnerHeadlessMessageExecutor for FatalDrainFlushOnCallExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "scripted",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        self.drain_calls.fetch_add(1, Ordering::AcqRel);
        Ok(HostedRunnerDrainResult {
            messages: self
                .messages
                .lock()
                .expect("drain messages")
                .take()
                .unwrap_or_default(),
            ..Default::default()
        })
    }

    fn flush_session(&self) -> Result<Option<PathBuf>, HostedRunnerError> {
        let call = self.flush_calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == 2 {
            return Err(HostedRunnerError::internal(
                "scripted snapshot flush failure after fatal drain",
            ));
        }
        Ok(None)
    }
}

#[tokio::test]
async fn fatal_drain_failure_fence_survives_restart_before_finalization() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let drain_calls = Arc::new(AtomicUsize::new(0));
    let flush_calls = Arc::new(AtomicUsize::new(0));
    let first = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(FatalDrainFlushOnCallExecutor {
            drain_calls: drain_calls.clone(),
            flush_calls: flush_calls.clone(),
            messages: Mutex::new(Some(vec![FromAgentMessage::Error {
                request_id: None,
                message: "fatal runtime during drain".to_string(),
                fatal: true,
                terminal: true,
                error_type: Some(crate::headless::messages::HeadlessErrorType::Fatal),
            }])),
        }),
        None,
    );

    let error = match handle_drain(
        first.clone(),
        DrainRequest {
            reason: Some("fatal-drain-before-finalization".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("manifest flush must fail after the fatal drain message"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::Internal);
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    {
        let state = first.state.lock().expect("first runner state");
        assert!(state.runtime_failed);
        assert!(state.drain_finalization_pending);
        assert!(state.drain_runtime_failed_before_finalization);
    }
    drop(first);

    let restarted = SharedRunner::new_with_message_executor_and_restore(
        config,
        Arc::new(FatalDrainFlushOnCallExecutor {
            drain_calls: drain_calls.clone(),
            flush_calls,
            messages: Mutex::new(None),
        }),
        None,
    );
    {
        let state = restarted.state.lock().expect("restarted runner state");
        assert!(state.drain_finalization_pending);
        assert!(state.drain_runtime_failed_before_finalization);
        assert!(state.runtime_failed);
    }

    let response = handle_drain(
        restarted.clone(),
        DrainRequest {
            reason: Some("retry-fatal-drain-finalization".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("fatal drain finalization retry");
    let ResponseBody::Json { status, .. } = response else {
        panic!("drain retry must be JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    let state = restarted.state.lock().expect("final runner state");
    assert!(!state.drain_finalization_pending);
    assert!(state.runtime_failed);
}

#[tokio::test]
async fn current_generation_journal_wins_over_configured_restore_manifest() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let shared = SharedRunner::new(config.clone());
    let response = handle_drain(
        shared.clone(),
        DrainRequest {
            reason: Some("journal-precedes-restore".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("drain should produce a restore input");
    let ResponseBody::Json { body, .. } = response else {
        panic!("drain must return JSON");
    };
    let manifest_path = PathBuf::from(body["manifest_path"].as_str().expect("drain manifest path"));
    let manifest: SnapshotManifest =
        serde_json::from_value(body["manifest"].clone()).expect("restore manifest");
    drop(shared);

    let mut restore_config = config;
    restore_config.restore_manifest_path = Some(manifest_path);
    let restored = SharedRunner::new_with_message_executor_and_restore(
        restore_config,
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        Some(manifest),
    );
    let receipt = restored
        .runtime_receipt()
        .expect("restored receipt should be available");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Drained);
    assert_eq!(receipt.lifecycle_state, RuntimeLifecycleState::Drained);
    let state = restored.state.lock().expect("restored state");
    assert!(!state.ready);
    assert!(state.draining);
    assert_eq!(state.last_status.as_deref(), Some("Drained"));
}

#[tokio::test]
async fn replacement_runner_journal_owner_is_persisted_after_restore() {
    let workspace = tempdir().expect("workspace");
    let source_config = test_config(workspace.path().to_path_buf());
    let source = SharedRunner::new(source_config.clone());
    {
        let mut state = source.state.lock().expect("source state");
        state.thread.initial_actions.insert(
            "turn-initial".to_string(),
            serde_json::from_value(initial_action_test_payload()).expect("source action receipt"),
        );
        source
            .persist_thread_for_request(&state)
            .expect("persist source action");
        source.publish_message(
            &mut state,
            FromAgentMessage::ResponseStart {
                response_id: "replacement-owner-response".to_string(),
            },
        );
    }
    let response = handle_drain(
        source.clone(),
        DrainRequest {
            reason: Some("replacement-owner-lineage".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("drain should produce a restore input");
    let ResponseBody::Json { body, .. } = response else {
        panic!("drain must return JSON");
    };
    let manifest_path = PathBuf::from(body["manifest_path"].as_str().expect("manifest path"));
    let manifest: SnapshotManifest =
        serde_json::from_value(body["manifest"].clone()).expect("restore manifest");
    drop(source);
    let source_restart = SharedRunner::new(source_config.clone());
    assert_eq!(
        source_restart
            .state
            .lock()
            .expect("source restart")
            .thread
            .initial_actions
            .len(),
        1
    );
    drop(source_restart);

    let mut replacement_config = source_config;
    replacement_config.runner_session_id = "mrs_replacement".to_string();
    replacement_config.restore_manifest_path = Some(manifest_path);
    let first_start = SharedRunner::new_with_message_executor_and_restore(
        replacement_config.clone(),
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        Some(manifest.clone()),
    );
    assert_eq!(
        first_start
            .runtime_receipt()
            .expect("replacement restore receipt")
            .kind,
        RuntimeReceiptKind::Restored
    );
    {
        let mut state = first_start.state.lock().expect("replacement state");
        assert!(
            state.thread.initial_actions.is_empty(),
            "source receipts must not cross the owner boundary"
        );
        let mut replacement_action = initial_action_test_payload();
        replacement_action["action"]["receiptId"] = json!("receipt-replacement");
        state.thread.initial_actions.insert(
            "turn-initial".to_string(),
            serde_json::from_value(replacement_action).expect("replacement action receipt"),
        );
        first_start
            .persist_thread_for_request(&state)
            .expect("persist replacement action");
    }
    drop(first_start);

    let restarted = SharedRunner::new_with_message_executor_and_restore(
        replacement_config,
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        Some(manifest),
    );
    let receipt = restarted
        .runtime_receipt()
        .expect("replacement restart receipt");
    assert_eq!(receipt.kind, RuntimeReceiptKind::Restored);
    assert_eq!(
        receipt.lifecycle_state,
        RuntimeLifecycleState::ExecutionReady
    );
    let state = restarted.state.lock().expect("restarted state");
    let receipt = serde_json::to_value(
        state
            .thread
            .initial_actions
            .get("turn-initial")
            .expect("replacement receipt survives restart"),
    )
    .expect("serialize receipt");
    assert_eq!(receipt["action"]["receiptId"], "receipt-replacement");
    assert!(state.ready);
    assert!(!state.draining);
    assert_eq!(state.last_status.as_deref(), Some("Restored from snapshot"));
}

#[tokio::test]
async fn replacement_restore_removes_source_executor_drain_handoff() {
    let workspace = tempdir().expect("workspace");
    let source_config = test_config(workspace.path().to_path_buf());
    let source = SharedRunner::new(source_config.clone());
    let response = handle_drain(
        source.clone(),
        DrainRequest {
            reason: Some("source-handoff-cleanup".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("source drain should produce a restore input");
    let ResponseBody::Json { body, .. } = response else {
        panic!("source drain must return JSON");
    };
    let manifest: SnapshotManifest =
        serde_json::from_value(body["manifest"].clone()).expect("source restore manifest");
    drop(source);

    persist_executor_drain_result(
        workspace.path(),
        "sess_test",
        source_config.runtime_generation,
        0,
        &HostedRunnerDrainResult {
            messages: vec![FromAgentMessage::ResponseChunk {
                response_id: "source-only-response".to_string(),
                content: "must not cross replacement ownership".to_string(),
                is_thinking: false,
            }],
            ..Default::default()
        },
    )
    .expect("write source drain handoff");
    assert!(
        load_executor_drain_result(
            workspace.path(),
            "sess_test",
            source_config.runtime_generation
        )
        .expect("load source drain handoff")
        .is_some()
    );

    let mut replacement_config = source_config;
    replacement_config.runner_session_id = "mrs_replacement_handoff_cleanup".to_string();
    replacement_config.restore_manifest_path = None;
    let first_replacement = SharedRunner::new_with_message_executor_and_restore(
        replacement_config.clone(),
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        Some(manifest.clone()),
    );
    {
        let state = first_replacement.state.lock().expect("replacement state");
        assert!(state.pending_executor_drain_result.is_none());
        assert!(
            state.envelopes.iter().all(|envelope| {
                !serde_json::to_string(envelope)
                    .expect("serialize replacement envelope")
                    .contains("source-only-response")
            }),
            "replacement must not replay the source runner handoff"
        );
    }
    assert!(
        load_executor_drain_result(
            workspace.path(),
            "sess_test",
            replacement_config.runtime_generation
        )
        .expect("load cleaned source drain handoff")
        .is_none()
    );
    drop(first_replacement);

    let restarted_replacement = SharedRunner::new_with_message_executor_and_restore(
        replacement_config,
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        Some(manifest),
    );
    let state = restarted_replacement
        .state
        .lock()
        .expect("restarted replacement state");
    assert!(state.pending_executor_drain_result.is_none());
    assert!(
        state.envelopes.iter().all(|envelope| {
            !serde_json::to_string(envelope)
                .expect("serialize restarted replacement envelope")
                .contains("source-only-response")
        }),
        "replacement restart must not reload the source handoff"
    );
}

struct ArmFinalJournalFailureExecutor {
    shared: Mutex<std::sync::Weak<SharedRunner>>,
    drained: Mutex<Option<HostedRunnerDrainResult>>,
}

impl HostedRunnerHeadlessMessageExecutor for ArmFinalJournalFailureExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "scripted",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        if let Some(shared) = self.shared.lock().expect("failure target").upgrade() {
            shared.fail_next_thread_persistences(1);
        }
        Ok(self
            .drained
            .lock()
            .expect("drained result")
            .take()
            .unwrap_or_default())
    }
}

#[tokio::test]
async fn drain_message_persistence_failure_keeps_retained_batch_retryable() {
    let workspace = tempdir().expect("workspace");
    let drained = HostedRunnerDrainResult {
        messages: vec![
            FromAgentMessage::ServerRequest {
                request_id: "approval-before-failure".to_string(),
                request_type: ServerRequestType::Approval,
                call_id: "approval-call".to_string(),
                tool_execution_id: None,
                tool: "bash".to_string(),
                args: json!({"command": "printf safe"}),
                reason: "approval boundary".to_string(),
                started_at_ms: None,
            },
            FromAgentMessage::Status {
                message: "gated suffix must survive publication failure".to_string(),
            },
        ],
        ..Default::default()
    };
    let executor = Arc::new(ArmFinalJournalFailureExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drained: Mutex::new(Some(drained)),
    });
    let shared = Arc::new(SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
        None,
    ));
    *executor.shared.lock().expect("failure target") = Arc::downgrade(&shared);

    let error = match handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("retained-publication-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("failed retained message publication must stop finalization"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    {
        let state = shared.state.lock().expect("runner state");
        assert!(state.runtime_failed);
        assert!(state.drain_finalization_pending);
        assert!(!state.executor_drain_result_applied);
        assert_eq!(state.executor_drain_result_applied_count, 0);
        assert!(state.pending_executor_drain_result.is_some());
        assert!(state.envelopes.iter().all(|envelope| {
            !serde_json::to_string(envelope)
                .expect("serialize failed drain envelopes")
                .contains("gated suffix must survive publication failure")
        }));
        assert_ne!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Drained),
            "a failed retained message must not publish a Drained receipt"
        );
    }

    let response = handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("retained-publication-failure-retry".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("retained drain result must be retryable");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    let state = shared.state.lock().expect("final runner state");
    assert!(state.pending_executor_drain_result.is_none());
    assert!(state.envelopes.iter().any(|envelope| {
        serde_json::to_string(envelope)
            .expect("serialize successful drain envelopes")
            .contains("gated suffix must survive publication failure")
    }));
}

/// A failed final journal write must leave the completed drain retryable rather
/// than rejecting every later request as "already draining".
#[tokio::test]
async fn drain_succeeds_despite_journal_failure_for_drained_keys() {
    let workspace = tempdir().expect("workspace");
    let drained = HostedRunnerDrainResult {
        consumed_response_keys: vec!["drain-key".to_string()],
        ..Default::default()
    };
    let executor = Arc::new(ArmFinalJournalFailureExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drained: Mutex::new(Some(drained)),
    });
    let shared = Arc::new(SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
        None,
    ));
    *executor.shared.lock().expect("failure target") = Arc::downgrade(&shared);
    seed_pending_response(&shared, "drain-key", "drain-request");

    let response = handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("matrix-test".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("drain must succeed despite the journal write failure");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    assert!(
        body["manifest_path"]
            .as_str()
            .is_some_and(|p| !p.is_empty())
    );
    {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.response_idempotency_keys.contains("drain-key"));
    }
}

#[tokio::test]
async fn drain_returns_failure_without_publishing_drained_receipt_after_final_journal_failure() {
    let workspace = tempdir().expect("workspace");
    let executor = Arc::new(ArmFinalJournalFailureExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drained: Mutex::new(Some(HostedRunnerDrainResult::default())),
    });
    let shared = Arc::new(SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
        None,
    ));
    *executor.shared.lock().expect("failure target") = Arc::downgrade(&shared);
    // The start boundary succeeds. The scripted executor arms one failure
    // after draining, so the final snapshot write is the failed hand-off.

    let error = match handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("final-write-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("drain must fail when its final journal write fails"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR.as_u16());

    {
        let state = shared.state.lock().expect("runner state");
        assert!(state.runtime_failed);
        assert!(!state.ready);
        assert_eq!(state.last_status.as_deref(), Some("Runtime failed"));
        assert_eq!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Draining),
            "failed flush must retain the last durably persisted receipt"
        );
        assert_ne!(
            state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
            Some(RuntimeReceiptKind::Drained)
        );
        assert!(state.drain_finalization_pending);
    }

    let response = handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("final-write-failure-retry".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("the completed drain must retry its final journal hand-off");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    let state = shared.state.lock().expect("runner state");
    assert!(!state.drain_finalization_pending);
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Drained)
    );
}

struct CrashSafeDrainExecutor {
    shared: Mutex<std::sync::Weak<SharedRunner>>,
    drain_calls: Arc<AtomicUsize>,
    arm_executor_handoff_failure: bool,
    arm_final_journal_failure: bool,
    duplicate_response_chunk: bool,
}

impl HostedRunnerHeadlessMessageExecutor for CrashSafeDrainExecutor {
    fn execute(
        &self,
        _context: &HostedRunnerHeadlessMessageContext,
        _message: ToAgentMessage,
    ) -> Result<HostedRunnerHeadlessMessageResult, HostedRunnerError> {
        Ok(HostedRunnerHeadlessMessageResult::transport_only(
            Vec::new(),
            "scripted",
        ))
    }

    fn drain(&self) -> Result<HostedRunnerDrainResult, HostedRunnerError> {
        let call = self.drain_calls.fetch_add(1, Ordering::AcqRel) + 1;
        if self.arm_executor_handoff_failure && call == 1 {
            self.shared
                .lock()
                .expect("failure target")
                .upgrade()
                .expect("first runner remains live during drain")
                .fail_next_executor_drain_result_persistences(1);
        }
        if self.arm_final_journal_failure && call == 1 {
            self.shared
                .lock()
                .expect("failure target")
                .upgrade()
                .expect("first runner remains live during drain")
                .fail_next_thread_persistences(1);
        }
        let response_chunk = FromAgentMessage::ResponseChunk {
            response_id: "crash-safe-drain-response".to_string(),
            content: "first batch survives marker failure".to_string(),
            is_thinking: false,
        };
        let mut messages = vec![response_chunk.clone()];
        if self.duplicate_response_chunk {
            messages.push(response_chunk);
        }
        messages.push(FromAgentMessage::ClientToolRequest {
            call_id: "crash-safe-drain-tool".to_string(),
            tool_execution_id: None,
            tool: "bash".to_string(),
            args: json!({
                "command": "printf drain-result",
                "authorization": "drain-result-secret-must-not-persist"
            }),
        });
        Ok(HostedRunnerDrainResult {
            messages,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn executor_drain_result_survives_completion_marker_failure_and_restart() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let drain_calls = Arc::new(AtomicUsize::new(0));
    let first_executor = Arc::new(CrashSafeDrainExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drain_calls: drain_calls.clone(),
        arm_executor_handoff_failure: false,
        arm_final_journal_failure: true,
        duplicate_response_chunk: false,
    });
    let first = Arc::new(SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        first_executor.clone(),
        None,
    ));
    *first_executor.shared.lock().expect("failure target") = Arc::downgrade(&first);

    let error = match handle_drain(
        (*first).clone(),
        DrainRequest {
            reason: Some("crash-safe-marker-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("completion marker failure must be reported"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    let retained =
        load_executor_drain_result(workspace.path(), "sess_test", config.runtime_generation)
            .expect("load crash recovery handoff")
            .expect("crash recovery handoff");
    assert!(
        !serde_json::to_string(&retained)
            .expect("serialize retained handoff")
            .contains("drain-result-secret-must-not-persist")
    );
    drop(first);

    let restarted = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(CrashSafeDrainExecutor {
            shared: Mutex::new(std::sync::Weak::new()),
            drain_calls: drain_calls.clone(),
            arm_executor_handoff_failure: false,
            arm_final_journal_failure: false,
            duplicate_response_chunk: false,
        }),
        None,
    );
    {
        let state = restarted.state.lock().expect("restarted runner state");
        assert!(state.drain_finalization_pending);
        assert!(!state.drain_executor_pending);
        assert!(state.pending_executor_drain_result.is_some());
        assert!(!state.executor_drain_result_applied);
    }

    let response = handle_drain(
        restarted.clone(),
        DrainRequest {
            reason: Some("crash-safe-marker-retry".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("restart must finalize the retained executor result");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must be JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    let state = restarted.state.lock().expect("final runner state");
    assert!(state.envelopes.iter().any(|envelope| {
        matches!(
            envelope,
            StreamEnvelope::Message { message, .. }
                if matches!(
                    message.as_ref(),
                    FromAgentMessage::ResponseChunk { content, .. }
                        if content == "first batch survives marker failure"
                )
        )
    }));
    assert_eq!(
        state.runtime_receipt.as_ref().map(|receipt| receipt.kind),
        Some(RuntimeReceiptKind::Drained)
    );
    assert!(state.pending_executor_drain_result.is_none());
    assert!(
        load_executor_drain_result(workspace.path(), "sess_test", config.runtime_generation,)
            .expect("load cleaned crash recovery handoff")
            .is_none()
    );
}

#[tokio::test]
async fn executor_drain_handoff_failure_retries_without_second_executor_drain() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let drain_calls = Arc::new(AtomicUsize::new(0));
    let executor = Arc::new(CrashSafeDrainExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drain_calls: drain_calls.clone(),
        arm_executor_handoff_failure: true,
        arm_final_journal_failure: false,
        duplicate_response_chunk: false,
    });
    let shared = Arc::new(SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        executor.clone(),
        None,
    ));
    *executor.shared.lock().expect("failure target") = Arc::downgrade(&shared);

    let error = match handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("executor-handoff-failure".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    {
        Ok(_) => panic!("handoff persistence failure must be reported"),
        Err(error) => error,
    };
    assert_eq!(error.code, HostedRunnerErrorCode::RuntimeFailed);
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
    {
        let state = shared.state.lock().expect("runner state");
        assert!(state.drain_finalization_pending);
        assert!(!state.drain_executor_pending);
        assert!(state.pending_executor_drain_result.is_some());
    }

    let response = handle_drain(
        (*shared).clone(),
        DrainRequest {
            reason: Some("executor-handoff-retry".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("retry must persist the retained result and finish drain");
    let ResponseBody::Json { status, body } = response else {
        panic!("drain retry must return JSON");
    };
    assert_eq!(status, 200);
    assert_eq!(body["status"], "drained");
    assert_eq!(drain_calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn retained_drain_batch_deduplication_starts_at_capture_cursor() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let drain_calls = Arc::new(AtomicUsize::new(0));
    let executor = Arc::new(CrashSafeDrainExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drain_calls,
        arm_executor_handoff_failure: false,
        arm_final_journal_failure: false,
        duplicate_response_chunk: false,
    });
    let shared = SharedRunner::new_with_message_executor_and_restore(config, executor, None);
    let prior = FromAgentMessage::ResponseChunk {
        response_id: "crash-safe-drain-response".to_string(),
        content: "first batch survives marker failure".to_string(),
        is_thinking: false,
    };
    {
        let mut state = shared.state.lock().expect("runner state");
        shared.publish_message(&mut state, prior.clone());
    }

    handle_drain(
        shared.clone(),
        DrainRequest {
            reason: Some("capture-cursor-deduplication".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("drain must succeed");

    let state = shared.state.lock().expect("runner state");
    let matching_chunks = state
        .envelopes
        .iter()
        .filter(|envelope| {
            matches!(
                envelope,
                StreamEnvelope::Message { message, .. }
                    if matches!(
                        message.as_ref(),
                        FromAgentMessage::ResponseChunk {
                            response_id,
                            content,
                            is_thinking,
                        } if response_id == "crash-safe-drain-response"
                            && content == "first batch survives marker failure"
                            && !*is_thinking
                    )
            )
        })
        .count();
    assert_eq!(matching_chunks, 2);
}

#[tokio::test]
async fn retained_drain_batch_preserves_duplicate_messages() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let executor = Arc::new(CrashSafeDrainExecutor {
        shared: Mutex::new(std::sync::Weak::new()),
        drain_calls: Arc::new(AtomicUsize::new(0)),
        arm_executor_handoff_failure: false,
        arm_final_journal_failure: false,
        duplicate_response_chunk: true,
    });
    let shared = SharedRunner::new_with_message_executor_and_restore(config, executor, None);

    handle_drain(
        shared.clone(),
        DrainRequest {
            reason: Some("duplicate-drain-batch".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("drain must succeed");

    let state = shared.state.lock().expect("runner state");
    let duplicate_chunks = state
        .envelopes
        .iter()
        .filter(|envelope| {
            matches!(
                envelope,
                StreamEnvelope::Message { message, .. }
                    if matches!(
                        message.as_ref(),
                        FromAgentMessage::ResponseChunk {
                            response_id,
                            content,
                            is_thinking,
                        } if response_id == "crash-safe-drain-response"
                            && content == "first batch survives marker failure"
                            && !*is_thinking
                    )
            )
        })
        .count();
    assert_eq!(duplicate_chunks, 2);
}

#[tokio::test]
async fn executor_drain_recovery_uses_durable_applied_position_past_replay_limit() {
    let workspace = tempdir().expect("workspace");
    let config = test_config(workspace.path().to_path_buf());
    let messages = (0..(MAX_EVENTS + 32))
        .map(|index| FromAgentMessage::Status {
            message: format!("retained-drain-message-{index}"),
        })
        .collect::<Vec<_>>();
    let result = HostedRunnerDrainResult {
        messages: messages.clone(),
        ..Default::default()
    };
    let shared = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        None,
    );
    persist_executor_drain_result(
        workspace.path(),
        "sess_test",
        config.runtime_generation,
        0,
        &result,
    )
    .expect("persist retained executor result");
    {
        let mut state = shared.state.lock().expect("runner state");
        state.ready = false;
        state.draining = true;
        state.drain_executor_pending = false;
        state.drain_finalization_pending = true;
        state.last_status = Some(HOSTED_RUNNER_DRAIN_FINALIZATION_PENDING_STATUS.to_string());
        state.pending_executor_drain_result = Some(PendingExecutorDrainResult {
            replay_cursor: 0,
            result,
        });
        shared.apply_pending_executor_drain_result(&mut state);
        assert_eq!(state.executor_drain_result_applied_count, messages.len());
        shared
            .persist_drain_finalization_boundary(&mut state)
            .expect("persist executor-complete marker");
    }
    drop(shared);

    let restarted = SharedRunner::new_with_message_executor_and_restore(
        config.clone(),
        Arc::new(ScriptedDrainExecutor::new(Vec::new())),
        None,
    );
    {
        let state = restarted.state.lock().expect("restarted runner state");
        assert!(state.drain_finalization_pending);
        assert!(state.pending_executor_drain_result.is_some());
        assert!(state.executor_drain_result_applied);
        assert_eq!(state.executor_drain_result_applied_count, messages.len());
        assert_eq!(state.cursor, messages.len() as u64);
    }

    handle_drain(
        restarted.clone(),
        DrainRequest {
            reason: Some("replay-limit-recovery".to_string()),
            requested_by: Some("tests".to_string()),
            export_paths: None,
        },
    )
    .await
    .expect("restart must finalize the already-applied drain result");

    let state = restarted.state.lock().expect("final runner state");
    assert_eq!(state.cursor, messages.len() as u64);
    assert!(state.envelopes.iter().all(|envelope| {
        !matches!(
            envelope,
            StreamEnvelope::Message { message, .. }
                if matches!(
                    message.as_ref(),
                    FromAgentMessage::Status { message }
                        if message == "retained-drain-message-0"
                )
        )
    }));
    assert!(state.pending_executor_drain_result.is_none());
}

async fn initial_action_test_runner(
    workspace: &Path,
    executor: Arc<PendingThreadExecutor>,
) -> HostedRunnerHandle {
    // Supply the already-exchanged workload binding to isolate the HTTP/journal
    // boundary from the separately tested Identity network exchange.
    let mut prepared = prepare_hosted_runner(test_config(workspace.to_path_buf()))
        .await
        .expect("prepare initial-action runner");
    prepared.config.workload_identity = Some(HostedRunnerWorkloadIdentityConfig {
        kubernetes_token_file: workspace.join("unused-token"),
        identity_tls_ca_file: workspace.join("unused-ca"),
        identity_exchange_url: "https://identity.example.test/exchange".parse().unwrap(),
        organization_id: "org_test".into(),
        workspace_id: "ws_test".into(),
        sandbox_id: Uuid::nil(),
        placement_generation: 0,
    });
    start_prepared_hosted_runner(prepared, executor).expect("start initial-action runner")
}

fn initial_action_test_payload() -> serde_json::Value {
    json!({
        "protocolVersion": "evalops.maestro.thread.v1",
        "organizationId": "org_test", "workspaceId": "ws_test",
        "turnId": "turn-initial", "idempotencyKey": "execution-initial",
        "action": {
            "executionId": "execution-initial", "callId": "call-initial",
            "toolName": "sandbox.write", "readOnly": false, "state": 5,
            "receiptId": "receipt-initial", "safeSummary": "Write completed.",
            "assistantDelta": "The file was written.", "safeOutput": {"path": "/workspace/proof.txt"},
            "evidenceRefs": [], "threadId": "operating-thread", "turnId": "turn-initial",
            "authorityRevision": 1, "acceptedAt": {"seconds": 1_788_792_900, "nanos": 0}
        }
    })
}

#[tokio::test]
async fn initial_action_http_delivery_is_durable_scoped_and_never_executes_twice() {
    let workspace = tempdir().unwrap();
    let executor = Arc::new(PendingThreadExecutor::default());
    let handle = initial_action_test_runner(workspace.path(), executor.clone()).await;
    let client = reqwest::Client::new();
    let (capability, subscription) =
        attach_thread_controller(&client, &handle.base_url(), "initial-controller").await;
    let url = format!(
        "{}/api/headless/threads/mrs_test/initial-actions",
        handle.base_url()
    );
    let post = |payload: serde_json::Value, generation: &str, cap: &str| {
        client
            .post(&url)
            .header("x-maestro-headless-connection-id", "initial-controller")
            .header("x-maestro-headless-subscriber-id", &subscription)
            .header("x-maestro-headless-connection-capability", cap)
            .header("x-maestro-runtime-generation", generation)
            .json(&payload)
            .send()
    };
    let payload = initial_action_test_payload();
    assert!(
        !post(payload.clone(), "1", &capability)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        !post(payload.clone(), "0", "invalid-capability")
            .await
            .unwrap()
            .status()
            .is_success()
    );
    for field in ["organizationId", "workspaceId"] {
        let mut wrong = payload.clone();
        wrong[field] = json!("different-tenant");
        assert!(
            !post(wrong, "0", &capability)
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    let mut pending = payload.clone();
    pending["action"]["state"] = json!(4);
    assert_eq!(
        post(pending, "0", &capability).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    handle.shared.fail_next_thread_persistences(1);
    assert_eq!(
        post(payload.clone(), "0", &capability)
            .await
            .unwrap()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(
        handle
            .shared
            .state
            .lock()
            .unwrap()
            .thread
            .initial_actions
            .is_empty()
    );
    for replayed in [false, true] {
        let ack: serde_json::Value = post(payload.clone(), "0", &capability)
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ack["accepted"], true);
        assert_eq!(ack["replayed"], replayed);
        assert_eq!(ack["thread_id"], "mrs_test");
        assert_eq!(ack["execution_id"], "execution-initial");
    }
    let mut changed = payload.clone();
    changed["action"]["safeOutput"] = json!({"different": true});
    assert_eq!(
        post(changed, "0", &capability).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert!(executor.prompts.lock().unwrap().is_empty());
    handle.shutdown().await;

    let restored = initial_action_test_runner(workspace.path(), executor.clone()).await;
    let (capability, subscription) =
        attach_thread_controller(&client, &restored.base_url(), "restored-controller").await;
    let request = |suffix: &str, body: serde_json::Value| {
        client
            .post(format!(
                "{}/api/headless/threads/mrs_test/{suffix}",
                restored.base_url()
            ))
            .header("x-maestro-headless-connection-id", "restored-controller")
            .header("x-maestro-headless-subscriber-id", &subscription)
            .header("x-maestro-headless-connection-capability", &capability)
            .header("x-maestro-runtime-generation", "0")
            .json(&body)
            .send()
    };
    let replay: serde_json::Value = request("initial-actions", payload)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    let turn = json!({"protocolVersion": "evalops.maestro.thread.v1", "turnId": "turn-initial", "kind": "user_message", "content": "Explain the result."});
    for _ in 0..2 {
        request("turns", turn.clone())
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let prompts = executor.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("execution-initial"));
    assert!(prompts[0].contains("/workspace/proof.txt"));
    assert!(prompts[0].starts_with("Explain the result."));
    restored.shutdown().await;
}

#[test]
fn managed_authorization_pending_request_survives_replay_eviction_until_ack() {
    let workspace = tempdir().unwrap();
    let executor = Arc::new(StatefulRuntimeExecutor::new(AgentState {
        pending_managed_authorizations: vec!["invocation-1".into()],
        ..Default::default()
    }));
    let shared = SharedRunner::new_with_message_executor_and_restore(
        test_config(workspace.path().to_path_buf()),
        executor.clone(),
        None,
    );
    let mut state = shared.state.lock().unwrap();
    shared.publish_message(
        &mut state,
        FromAgentMessage::ManagedAuthorizationRequest {
            request_id: "invocation-1".into(),
        },
    );
    for index in 0..=MAX_EVENTS {
        shared.publish_message(
            &mut state,
            FromAgentMessage::Status {
                message: format!("progress {index}"),
            },
        );
    }
    assert!(
        matches!(shared.controller_pending_events(&mut state).as_slice(),
        [FromAgentMessage::ManagedAuthorizationRequest { request_id }] if request_id == "invocation-1")
    );
    executor
        .state
        .lock()
        .unwrap()
        .handle_message(FromAgentMessage::ResponseAccepted {
            request_id: "invocation-1".into(),
        });
    assert!(shared.controller_pending_events(&mut state).is_empty());
    assert!(state.pending_controller_events.is_empty());
}
