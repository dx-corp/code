use super::*;

// Reuse the fixture's bounded condition wait, rather than assuming elapsed
// time means the child and event pump have persisted consumed ownership.
async fn await_acknowledged_owner(
    handle: &HostedRunnerHandle,
    workspace: &Path,
    key: &str,
    request_id: &str,
) {
    let journal_path = std::fs::read_dir(workspace.join(".maestro/hosted-runner/threads"))
        .expect("thread journal directory")
        .map(|entry| entry.expect("thread journal entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("thread journal JSON path");
    wait_for_condition(|| {
        let completed = {
            let state = handle
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.response_idempotency_keys.contains(key)
                && !state.pending_response_idempotency.contains_key(key)
                && state
                    .response_request_owners
                    .get(request_id)
                    .map(String::as_str)
                    == Some(key)
        };
        let ledger_consumed = load_executor_response_ledger(workspace, "sess_test")
            .expect("executor response ledger")
            .iter()
            .any(|(owner, consumed)| owner == key && *consumed);
        let journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).expect("durable thread journal"))
                .expect("thread journal JSON");
        completed
            && journal["thread_id"] == "sess_test"
            && ledger_consumed
            && journal["response_idempotency_keys"]
                .as_array()
                .is_some_and(|keys| keys.contains(&json!(key)))
            && journal["pending_response_idempotency"]
                .as_object()
                .is_some_and(|pending| !pending.contains_key(key))
            && journal["response_request_owners"][request_id].as_str() == Some(key)
    })
    .await;
}

fn gated_identity_ack_script(
    directory: &Path,
    name: &str,
    log_path: &Path,
    message_type: &str,
    request_id: &str,
) -> (PathBuf, PathBuf) {
    let script_path = directory.join(name);
    let release = directory.join(format!("{name}.release"));
    std::fs::write(
        &script_path,
        format!(
            r#"#!/bin/sh
printf '{{"type":"ready","model":"test","provider":"test"}}\n'
while IFS= read -r line; do
  case "$line" in
    *'"type":"{message_type}"'*)
      printf '%s\n' "$line" >> "{}"
      while [ ! -f '{}' ]; do sleep 0.01; done
      printf '{{"type":"response_accepted","request_id":"{request_id}"}}\n'
      ;;
  esac
done
"#,
            log_path.display(),
            release.display(),
        ),
    )
    .expect("gated identity acknowledgement script");
    let mut permissions = std::fs::metadata(&script_path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).expect("script permissions");
    (script_path, release)
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
    let (first_script, release_ack) = gated_identity_ack_script(
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
    // Until the native child acknowledges, a retry reconciles the same owner;
    // it is not yet a replay of completed ownership.
    let pending = handle_message(
        first.shared.clone(),
        "sess_test",
        owner_headers.clone(),
        message.clone(),
    )
    .await
    .expect("same owner reconciles while acknowledgement is withheld");
    let ResponseBody::Json { body, .. } = pending else {
        panic!("pending owner retry must return JSON");
    };
    assert_eq!(body["replayed"], false);
    assert_eq!(
        std::fs::read_to_string(&first_log)
            .expect("pending child log")
            .lines()
            .count(),
        1,
        "pending reconciliation must not redispatch the response"
    );
    std::fs::write(&release_ack, b"accept").expect("release native acknowledgement");
    await_acknowledged_owner(&first, workspace.path(), "identity-owner-key", request_id).await;
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
    let second_connection_id = if matches!(
        &message,
        ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult { .. })
    ) {
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
pub(super) fn governed_response_for_ack_test() -> ToAgentMessage {
    ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
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
    })
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
async fn correlated_protocol_rejection_rolls_back_ownership_and_allows_retry() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("rejected-responses.log");
    let script = create_reject_then_accept_script(fixtures.path(), &log_path, None);
    let release_ack = fixtures.path().join("release-corrected-ack");
    let scripted = std::fs::read_to_string(&script).expect("rejection script");
    std::fs::write(
        &script,
        scripted.replace(
            "      else\n",
            &format!(
                "      else\n        while [ ! -f '{}' ]; do sleep 0.01; done\n",
                release_ack.display()
            ),
        ),
    )
    .expect("gate corrected native acknowledgement");
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
    std::fs::write(&release_ack, b"accept").expect("release corrected acknowledgement");
    await_acknowledged_owner(&handle, workspace.path(), "rejected-key", "retry-call").await;
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
