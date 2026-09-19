use super::*;

#[test]
fn agent_cancel_interrupts_an_approval_wait_without_dropping_the_request() {
    let request_token = CancellationToken::new();
    let approval_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: None,
        approval: Some(approval_token.clone()),
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let agent = super::super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: active.clone(),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token: CancellationToken::new(),
        runner_handle: None,
    };

    agent.cancel_keep_queue();

    assert!(approval_token.is_cancelled());
    assert!(!request_token.is_cancelled());
    assert!(
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_interrupted
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(AgentCommand::Cancel {
            clear_pending: false
        })
    ));
}

#[tokio::test]
async fn agent_cancel_keeps_tool_batch_cleanup_alive_between_operations() {
    let request_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: None,
        approval: None,
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let agent = super::super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: active.clone(),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token: CancellationToken::new(),
        runner_handle: None,
    };

    agent.cancel_keep_queue();

    assert!(!request_token.is_cancelled());
    assert!(
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_interrupted
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(AgentCommand::Cancel {
            clear_pending: false
        })
    ));
}

#[tokio::test]
async fn session_transition_cancel_waits_for_an_explicit_cleanup_acknowledgement() {
    let request_token = CancellationToken::new();
    let active = Arc::new(Mutex::new(ActiveCancellation {
        request: Some(request_token.clone()),
        tool: None,
        approval: None,
        tool_batch_active: true,
        terminal_drain_required: false,
        operation_interrupted: false,
    }));
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (tool_response_tx, _tool_response_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let agent = super::super::NativeAgent {
        managed_authorization: Arc::new(crate::agent::ManagedAuthorizationCoordinator::new(
            event_tx.clone(),
        )),
        host: runtime_test_host_handle(),
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        managed_run_id: "test-run".to_owned(),
        command_tx,
        tool_response_tx,
        active_cancellation: active.clone(),
        event_tx,
        model_name: "test-model".to_string(),
        provider_name: "test-provider".to_string(),
        runtime_audit: empty_runtime_audit(),
        shutdown_token: CancellationToken::new(),
        runner_handle: None,
    };

    let mut settled = agent.cancel_for_session_transition().unwrap();

    assert!(!request_token.is_cancelled());
    assert!(
        active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .operation_interrupted
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(AgentCommand::Cancel {
            clear_pending: true
        })
    ));
    assert!(matches!(
        settled.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let AgentCommand::AwaitIdle { reply } = command_rx.try_recv().unwrap() else {
        panic!("cancellation must be followed by its idle barrier");
    };
    reply.send(()).unwrap();
    settled.await.unwrap();
}
