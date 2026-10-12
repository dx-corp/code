use super::*;

#[tokio::test]
async fn codex_headless_bridge_bounds_shutdown_wait_after_valid_output() {
    assert_bounded_shutdown_after_valid_output(0).await;
}

#[tokio::test]
async fn codex_headless_bridge_shutdown_budget_excludes_allowed_response_delay() {
    assert_bounded_shutdown_after_valid_output(3000).await;
}

async fn assert_bounded_shutdown_after_valid_output(response_delay_ms: u64) {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-headless-hung-shutdown");
    let cli_path = root.path().join("cli.js");
    let completion_marker = root.path().join("turn-completed");
    fs::write(
            &cli_path,
            r#"const fs = require("fs");
const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, terminal: false });
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    const respond = () => {
    send({ type: "response_start", response_id: "response-1" });
    send({ type: "response_chunk", response_id: "response-1", content: "bounded output", is_thinking: false });
    send({ type: "response_end", response_id: "response-1", duration_ms: 1 });
    fs.writeFileSync(COMPLETION_MARKER, "completed");
    send({ type: "turn_completed", response_id: "response-1" });
    };
    if (RESPONSE_DELAY_MS === 0) respond();
    else setTimeout(respond, RESPONSE_DELAY_MS);
  } else if (msg.type === "shutdown") {
    setInterval(() => {}, 1000);
  }
});
"#
        .replace("COMPLETION_MARKER", &serde_json::to_string(completion_marker.to_str().expect("completion marker path")).expect("serialize completion marker path"))
        .replace("RESPONSE_DELAY_MS", &response_delay_ms.to_string()),
        )
        .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    let previous_shutdown_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_SHUTDOWN_TIMEOUT_MS");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "5000");
    env::set_var("MAESTRO_CODEX_APP_SERVER_SHUTDOWN_TIMEOUT_MS", "50");

    let state = test_app_state_with_sessions(HashMap::new());
    let (_client, mut server) = tcp_stream_pair().await;
    let output = run_codex_app_server_headless_cli(
        &mut server,
        CodexBridgeTransport::Sse,
        &state,
        None,
        root.path(),
        "gpt-5.5",
        "hello",
        &[],
    )
    .await;

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
    if let Some(previous) = previous_shutdown_timeout {
        env::set_var("MAESTRO_CODEX_APP_SERVER_SHUTDOWN_TIMEOUT_MS", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_SHUTDOWN_TIMEOUT_MS");
    }

    assert_eq!(
        output
            .expect("valid output should survive a hung shutdown")
            .text,
        "bounded output"
    );
    assert!(
        fs::metadata(&completion_marker)
            .expect("valid turn must record its completion boundary")
            .modified()
            .expect("completion marker must have a timestamp")
            .elapsed()
            .expect("completion boundary must not be in the future")
            < Duration::from_secs(2),
        "hung shutdown should be bounded"
    );
}
