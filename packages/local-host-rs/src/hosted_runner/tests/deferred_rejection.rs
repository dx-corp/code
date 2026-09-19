use super::*;

#[cfg(unix)]
fn gated_rejection_script(directory: &Path, log_path: &Path) -> (PathBuf, PathBuf) {
    let script = create_reject_then_accept_script(directory, log_path, None);
    let release = directory.join("release-rejection");
    let quoted_release = format!("'{}'", release.to_string_lossy().replace('\'', "'\\''"));
    let contents = std::fs::read_to_string(&script).expect("fixture script");
    let boundary = "if [ \"$count\" -eq 1 ]; then\n";
    assert_eq!(contents.matches(boundary).count(), 1);
    std::fs::write(
        &script,
        contents.replacen(
            boundary,
            &format!("{boundary}        while [ ! -f {quoted_release} ]; do sleep 0.01; done\n"),
            1,
        ),
    )
    .expect("gated rejection fixture");
    (script, release)
}

#[cfg(unix)]
async fn await_rollback(complete: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !complete() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("event pump rolls back the released rejection");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_protocol_rejection_after_queued_return_is_rolled_back_by_event_pump() {
    let workspace = tempdir().expect("workspace");
    let fixtures = tempdir().expect("fixtures");
    let log_path = fixtures.path().join("delayed-rejection.log");
    let (script, release_rejection) = gated_rejection_script(fixtures.path(), &log_path);
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
    std::fs::write(&release_rejection, b"reject").expect("release native rejection");
    await_rollback(|| {
        let pending = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_response_idempotency
            .contains_key("delayed-rejected-key");
        let queued = executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("delayed-rejected-key");
        !pending && !queued
    })
    .await;
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
    let (script, release_rejection) = gated_rejection_script(fixtures.path(), &log_path);
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
    std::fs::write(&release_rejection, b"reject").expect("release native rejection");
    await_rollback(|| {
        let pending = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_response_idempotency
            .contains_key("rejection-persist-key");
        let queued = executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("rejection-persist-key");
        !pending && !queued
    })
    .await;

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
    let (script, release_rejection) = gated_rejection_script(fixtures.path(), &log_path);
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
    std::fs::write(&release_rejection, b"reject").expect("release native rejection");
    await_rollback(|| {
        let pending = handle
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_response_idempotency
            .contains_key("pump-ledger-key");
        let queued = executor
            .queued_responses
            .lock()
            .expect("queued responses")
            .contains_key("pump-ledger-key");
        !pending && !queued
    })
    .await;

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
