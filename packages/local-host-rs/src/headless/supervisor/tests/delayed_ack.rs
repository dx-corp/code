use super::*;

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
      while [ ! -f "$MAESTRO_TEST_ACK_GATE" ]; do sleep 0.01; done
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
    let gate_path = temp.path().join("ack-release");
    config.transport.env.push((
        "MAESTRO_TEST_ACK_GATE".to_owned(),
        gate_path.display().to_string(),
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
    // Release the child only after observing queue admission. A fixed sleep
    // raced full-workspace scheduling against the unchanged 800ms wait budget.
    fs::write(&gate_path, b"release").expect("release response acknowledgement");
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
