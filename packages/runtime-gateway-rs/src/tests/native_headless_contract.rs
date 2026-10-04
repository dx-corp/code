use super::*;

#[tokio::test]
async fn native_tool_decisions_wait_for_turn_completion_and_preserve_execution_owner() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("native-headless-tool-decision");
    let cli_path = root.path().join("cli.js");
    let marker = root.path().join("decision.json");
    fs::write(
        &cli_path,
        r#"const fs = require("fs");
const rl = require("readline").createInterface({ input: process.stdin });
const send = value => process.stdout.write(JSON.stringify(value) + "\n");
rl.on("line", line => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    send({type:"response_start", response_id:"model-1"});
    send({type:"response_chunk", response_id:"model-1", content:"before approval", is_thinking:false});
    send({type:"response_end", response_id:"model-1", usage:{input_tokens:1}});
    send({type:"tool_call", call_id:"native-write", tool_execution_id:"native-execution", tool:"write",
      args:{path:"file.txt", content:"native owner executes"}, requires_approval:true});
  } else if (msg.type === "tool_response") {
    fs.writeFileSync(process.env.MAESTRO_NATIVE_DECISION_MARKER,
      JSON.stringify({decision:msg, model:process.env.MAESTRO_MODEL}));
    send({type:"tool_end", call_id:"native-write", success:msg.approved});
    if (msg.approved) send({type:"response_start", response_id:"model-2"});
    send({type:"response_chunk", response_id:"model-2", is_thinking:false,
      content:msg.approved ? "approved final" : "denied final"});
    send({type:"response_end", response_id:"model-2", usage:{input_tokens:5}});
    send({type:"turn_completed", response_id:"turn-1"});
  } else if (msg.type === "shutdown") process.exit(0);
});
"#,
    )
    .unwrap();
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_marker = env::var_os("MAESTRO_NATIVE_DECISION_MARKER");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_NATIVE_DECISION_MARKER", &marker);

    for approved in [true, false] {
        let state = test_app_state_with_sessions(HashMap::new());
        let (mut client, server) = tcp_stream_pair().await;
        let state_for_run = state.clone();
        let cwd = root.path().to_path_buf();
        let run = tokio::spawn(async move {
            let mut server = server;
            run_codex_app_server_headless_cli(
                &mut server,
                CodexBridgeTransport::Sse,
                &state_for_run,
                Some("native-session"),
                &cwd,
                "gpt-5.6",
                "write the file",
                &[],
            )
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        let (id, sender) = loop {
            let mut pending = state.pending_tool_responses.lock().await;
            if let Some(id) = pending.keys().next().cloned() {
                let sender = pending.remove(&id).unwrap();
                break (id, sender);
            }
            drop(pending);
            assert!(
                !run.is_finished(),
                "model response end is not turn completion"
            );
            assert!(Instant::now() < deadline, "native approval must be exposed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(id.starts_with("codex:native-session:"));
        assert!(id.ends_with(":native-write"));
        assert!(matches!(
            state.pending_tool_response_sessions.lock().await.get(&id),
            Some(PendingToolResponseOwner::Session(session)) if session == "native-session"
        ));
        sender
            .send((id, approved, None, ExecutionSource::RemoteClient, None))
            .unwrap();
        let output = run
            .await
            .unwrap()
            .expect("native turn completes after decision");
        let captured: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        assert_eq!(captured["model"], "openai-codex/gpt-5.6");
        let decision = captured["decision"].clone();
        assert_eq!(decision["tool_execution_id"], "native-execution");
        assert!(matches!(
            serde_json::from_value::<maestro_local_host::headless::ToAgentMessage>(decision.clone()).unwrap(),
            maestro_local_host::headless::ToAgentMessage::ToolResponse { call_id, approved: actual, result: None, .. }
                if call_id == "native-write" && actual == approved
        ));
        assert!(
            decision.get("result").is_none(),
            "approval must not forge tool output"
        );
        assert_eq!(
            output.text,
            if approved {
                "before approval\n\napproved final"
            } else {
                "before approval\n\ndenied final"
            }
        );
        assert_eq!(output.usage.unwrap().input_tokens, 5);
        assert_eq!(output.tool_events.len(), 2);
        assert!(state.pending_tool_responses.lock().await.is_empty());
        assert!(state.pending_tool_response_sessions.lock().await.is_empty());
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        let wire = String::from_utf8(bytes).unwrap();
        assert!(wire.contains("action_approval_required"));
        assert!(wire.contains("native-write"));
    }
    if let Some(value) = previous_cli {
        env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", value);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_CLI");
    }
    if let Some(value) = previous_marker {
        env::set_var("MAESTRO_NATIVE_DECISION_MARKER", value);
    } else {
        env::remove_var("MAESTRO_NATIVE_DECISION_MARKER");
    }
}

#[tokio::test]
async fn native_terminal_and_provider_errors_fail_without_waiting_for_response_end() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("native-headless-terminal-error");
    let cli_path = root.path().join("cli.js");
    fs::write(
        &cli_path,
        r#"const rl = require("readline").createInterface({input:process.stdin});
rl.on("line", line => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    const failure = msg.content.includes("provider")
      ? {type:"provider_error",message:"native turn denied",kind:"quota_exceeded"}
      : {type:"error",message:"native turn denied",fatal:false,terminal:true};
    process.stdout.write(JSON.stringify(failure) + "\n");
  }
});"#,
    )
    .unwrap();
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    for prompt in ["hello", "provider"] {
        let state = test_app_state_with_sessions(HashMap::new());
        let (_client, mut server) = tcp_stream_pair().await;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_codex_app_server_headless_cli(
                &mut server,
                CodexBridgeTransport::Sse,
                &state,
                None,
                root.path(),
                "gpt-5.6",
                prompt,
                &[],
            ),
        )
        .await
        .expect("terminal rejection must be immediate");
        assert!(matches!(result, Err(message) if message == "native turn denied"));
    }
    if let Some(value) = previous_cli {
        env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", value);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_CLI");
    }
}
