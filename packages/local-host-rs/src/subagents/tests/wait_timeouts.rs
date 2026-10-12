use super::*;

#[tokio::test(start_paused = true)]
async fn orb_wait_bounds_remote_refresh_by_remaining_timeout() {
    let root = tempfile::tempdir().expect("temp root");
    let manager =
        SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
    let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
    let caller = OwnerBindingCaller::with_status_delay("active", &[], Duration::from_secs(5));
    manager
        .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
            caller.clone(),
            binding.clone(),
        ))
        .await;
    let record = running_orb_record(root.path(), &binding);
    manager
        .write_record(&record)
        .expect("persist Computer record");

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        manager.wait(
            &serde_json::json!({
                "subagent_id": record.id,
                "timeout_ms": 25,
            }),
            None,
        ),
    )
    .await
    .expect("wait deadline must bound a slow remote refresh");

    assert!(result.success, "the cached running snapshot remains valid");
    assert_eq!(
        caller
            .calls
            .lock()
            .expect("owner caller calls lock")
            .as_slice(),
        ["orb_task_status"],
        "a timed-out status refresh must not proceed to collect"
    );
}

#[tokio::test(start_paused = true)]
async fn orb_wait_counts_completed_refresh_and_sleep_on_the_same_clock() {
    let root = tempfile::tempdir().expect("temp root");
    let manager =
        SubagentManager::with_root(root.path().to_path_buf(), root.path().join("records"));
    let binding = owner_binding("org-a", "workspace-a", "connection-a", 1);
    let caller = OwnerBindingCaller::with_status_delay("active", &[], Duration::ZERO);
    manager
        .set_orb_adapter(OrbDelegationAdapter::from_caller_with_owner_binding(
            caller.clone(),
            binding.clone(),
        ))
        .await;
    let record = running_orb_record(root.path(), &binding);
    manager
        .write_record(&record)
        .expect("persist Computer record");

    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        manager.wait(
            &serde_json::json!({
                "subagent_id": record.id,
                "timeout_ms": 25,
            }),
            None,
        ),
    )
    .await
    .expect("wait deadline must bound a slow remote refresh");

    assert!(result.success, "the cached running snapshot remains valid");
    assert_eq!(
        tokio::time::Instant::now().duration_since(started),
        Duration::from_millis(25),
        "completed refresh and sleep must share the original wait budget"
    );
}
