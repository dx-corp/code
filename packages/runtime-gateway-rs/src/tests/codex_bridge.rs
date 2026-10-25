#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn codex_app_server_model_ids_require_openai_codex_prefix() {
    assert_eq!(
        codex_app_server_model_id("openai-codex/gpt-5.5").as_deref(),
        Some("gpt-5.5")
    );
    assert_eq!(
        codex_app_server_model_id(" openai-codex/gpt-5.1-codex-max ").as_deref(),
        Some("gpt-5.1-codex-max")
    );
    assert!(codex_app_server_model_id("openai/gpt-5.5").is_none());
    assert!(codex_app_server_model_id("openai-codex/").is_none());
}

#[test]
pub(crate) fn codex_app_server_sandbox_mode_inherits_without_forcing_read_only() {
    assert_eq!(codex_app_server_sandbox_mode_from_values(None, None), None);
    assert_eq!(
        codex_app_server_sandbox_mode_from_values(None, Some("workspace-write")).as_deref(),
        Some("workspace-write")
    );
    assert_eq!(
        codex_app_server_sandbox_mode_from_values(Some("danger-full-access"), Some("read-only"))
            .as_deref(),
        Some("danger-full-access")
    );
    assert_eq!(
        codex_app_server_sandbox_mode_from_values(Some("inherit"), Some("workspace-write")),
        None
    );
}

#[test]
pub(crate) fn assistant_output_from_jsonl_records_codex_collab_tool_items() {
    let jsonl = r#"
{"type":"item.started","item":{"id":"collab-call-1","type":"collab_tool_call","tool":"spawn_agent","sender_thread_id":"thread-main","receiver_thread_ids":["thread-child"],"prompt":"Inspect routing","agents_states":{},"status":"in_progress"}}
{"type":"item.completed","item":{"id":"collab-call-1","type":"collab_tool_call","tool":"spawn_agent","sender_thread_id":"thread-main","receiver_thread_ids":["thread-child"],"prompt":"Inspect routing","agents_states":{"thread-child":{"status":"completed","message":"Done"}},"status":"completed"}}
{"type":"turn","phase":"start","role":"assistant","turnId":"turn-1"}
{"type":"item","subtype":"message_complete","turnId":"turn-1","data":{"text":"assistant text","stopReason":"stop"}}
{"type":"turn","phase":"end","role":"assistant","turnId":"turn-1"}
"#;
    let output = assistant_output_from_jsonl(jsonl).expect("assistant output");

    assert_eq!(output.text, "assistant text");
    assert_eq!(output.tool_events.len(), 2);
    assert_eq!(output.tool_events[0].event_type, "tool_execution_start");
    assert_eq!(output.tool_events[0].tool_call_id, "collab-call-1");
    assert_eq!(output.tool_events[0].tool_name, "codex.subagent.spawnAgent");
    assert_eq!(
        output.tool_events[0].args["receiverThreadIds"],
        serde_json::json!(["thread-child"])
    );
    assert_eq!(
        output.tool_events[0].args["childRunIds"],
        serde_json::json!(["codex-thread:thread-child"])
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["schemaVersion"],
        CODEX_SUBAGENT_WORK_GRAPH_SCHEMA
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["childRunId"],
        "codex-thread:thread-child"
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["edgeId"],
        "collab-call-1:0:spawnAgent:codex-thread:thread-child"
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["targetIndex"],
        serde_json::json!(0)
    );
    assert!(output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["status"].is_null());
    assert_eq!(output.tool_events[1].event_type, "tool_execution_end");
    assert_eq!(output.tool_events[1].is_error, Some(false));
    assert_eq!(
        output.tool_events[1].result["details"]["codexWorkGraph"]["status"],
        "completed"
    );
    assert_eq!(
        output.tool_events[1].result["details"]["codexWorkGraph"]["childRuns"][0]["status"],
        "completed"
    );
    assert_eq!(
        output.tool_events[1].result["details"]["agentsStates"]["thread-child"]["status"],
        "completed"
    );
}

#[test]
pub(crate) fn assistant_output_from_jsonl_normalizes_codex_collab_tool_aliases() {
    let jsonl = r#"
{"type":"item.started","item":{"id":"collab-call-wait","type":"collab_tool_call","tool":"wait_agent","sender_thread_id":"thread-main","receiver_thread_ids":["thread-child"],"child_run_ids":["agent-run-child-1"],"agents_states":{},"status":"in_progress"}}
{"type":"item.completed","item":{"id":"collab-call-wait","type":"collab_tool_call","tool":"wait_agent","sender_thread_id":"thread-main","receiver_thread_ids":["thread-child"],"child_run_ids":["agent-run-child-1"],"agents_states":{},"status":"completed"}}
{"type":"turn","phase":"start","role":"assistant","turnId":"turn-1"}
{"type":"item","subtype":"message_complete","turnId":"turn-1","data":{"text":"assistant text","stopReason":"stop"}}
{"type":"turn","phase":"end","role":"assistant","turnId":"turn-1"}
"#;
    let output = assistant_output_from_jsonl(jsonl).expect("assistant output");

    assert_eq!(output.tool_events.len(), 2);
    assert_eq!(output.tool_events[0].tool_name, "codex.subagent.wait");
    assert_eq!(
        output.tool_events[0].display_name.as_deref(),
        Some("Codex subagent: wait")
    );
    assert_eq!(
        output.tool_events[0].args["codexTool"],
        serde_json::json!("wait")
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["tool"],
        serde_json::json!("wait")
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["operation"],
        serde_json::json!("wait")
    );
    assert_eq!(output.tool_events[1].tool_name, "codex.subagent.wait");
    assert_eq!(
        output.tool_events[1].result["details"]["codexTool"],
        serde_json::json!("wait")
    );
}

#[test]
pub(crate) fn assistant_output_from_jsonl_preserves_explicit_codex_child_run_ids() {
    let jsonl = r#"
{"type":"item.started","item":{"id":"collab-call-1","type":"collab_tool_call","tool":"spawn_agent","sender_thread_id":"thread-main","receiver_thread_ids":["thread-child"],"child_run_ids":["agent-run-child-1"],"prompt":"Inspect routing","agents_states":{},"status":"in_progress"}}
{"type":"item.completed","item":{"id":"collab-call-1","type":"collab_tool_call","tool":"spawn_agent","sender_thread_id":"thread-main","receiver_thread_ids":["thread-child"],"child_run_ids":["agent-run-child-1"],"prompt":"Inspect routing","agents_states":{},"status":"completed"}}
{"type":"turn","phase":"start","role":"assistant","turnId":"turn-1"}
{"type":"item","subtype":"message_complete","turnId":"turn-1","data":{"text":"assistant text","stopReason":"stop"}}
{"type":"turn","phase":"end","role":"assistant","turnId":"turn-1"}
"#;
    let output = assistant_output_from_jsonl(jsonl).expect("assistant output");

    assert_eq!(
        output.tool_events[0].args["childRunIds"],
        serde_json::json!(["agent-run-child-1"])
    );
    assert_eq!(
        output.tool_events[1].result["details"]["childRunIds"],
        serde_json::json!(["agent-run-child-1"])
    );
    assert_eq!(
        output.tool_events[1].result["details"]["codexWorkGraph"]["childRuns"][0]["childRunId"],
        "agent-run-child-1"
    );
}

#[test]
pub(crate) fn codex_headless_usage_omits_empty_usage() {
    let event = serde_json::json!({
        "type": "response_end",
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0
        }
    });

    assert!(codex_headless_usage_from_json(&event).is_none());
}

#[test]
pub(crate) fn codex_headless_tool_events_preserve_subagent_lifecycle() {
    let start = codex_headless_tool_event_from_json(&serde_json::json!({
        "type": "tool_call",
        "call_id": "collab-call-3",
        "tool": "codex.subagent.sendInput",
        "args": {
            "codexTool": "sendInput",
            "receiverThreadIds": ["thread-child"],
            "prompt": "Please continue"
        }
    }))
    .expect("start event");
    let end = codex_headless_tool_event_from_json(&serde_json::json!({
        "type": "tool_end",
        "call_id": "collab-call-3",
        "tool": "codex.subagent.sendInput",
        "success": false,
        "error_code": "subagent_not_found",
        "details": {
            "receiverThreadIds": ["thread-child"],
            "childRunIds": ["agent-run-child-1"],
            "agentsStates": {
                "thread-child": { "status": "failed" }
            }
        }
    }))
    .expect("end event");

    assert_eq!(start.event_type, "tool_execution_start");
    assert_eq!(start.tool_name, "codex.subagent.sendInput");
    assert_eq!(start.args["prompt"], "Please continue");
    assert_eq!(start.args["toolCallId"], "collab-call-3");
    assert_eq!(start.args["codexWorkGraph"]["toolCallId"], "collab-call-3");
    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["edgeId"],
        "collab-call-3:0:sendInput:codex-thread:thread-child"
    );
    assert_eq!(end.event_type, "tool_execution_end");
    assert_eq!(end.tool_call_id, "collab-call-3");
    assert_eq!(end.is_error, Some(true));
    assert_eq!(end.result["details"]["errorCode"], "subagent_not_found");
    assert_eq!(
        end.result["details"]["receiverThreadIds"],
        serde_json::json!(["thread-child"])
    );
    assert_eq!(
        end.result["details"]["childRunIds"],
        serde_json::json!(["agent-run-child-1"])
    );
    assert_eq!(
        end.result["details"]["agentsStates"]["thread-child"]["status"],
        "failed"
    );
    assert_eq!(
        end.result["content"][0]["text"],
        "codex.subagent.sendInput failed"
    );
}

#[test]
pub(crate) fn codex_headless_tool_events_normalize_snake_case_subagent_targets() {
    let start = codex_headless_tool_event_from_json(&serde_json::json!({
        "type": "tool_call",
        "call_id": "collab-call-snake",
        "tool": "codex.subagent.send_input",
        "args": {
            "codex_tool": "send_input",
            "sender_thread_id": "thread-parent",
            "receiver_thread_ids": ["thread-child"],
            "child_run_ids": ["agent-run-child-1"],
            "agents_states": {
                "thread-child": { "status": "acknowledged" }
            }
        }
    }))
    .expect("start event");

    assert_eq!(start.tool_name, "codex.subagent.sendInput");
    assert_eq!(start.args["codexTool"], serde_json::json!("sendInput"));
    assert_eq!(
        start.args["receiverThreadIds"],
        serde_json::json!(["thread-child"])
    );
    assert_eq!(
        start.args["childRunIds"],
        serde_json::json!(["agent-run-child-1"])
    );
    assert_eq!(start.args["senderThreadId"], "thread-parent");
    assert_eq!(
        start.args["codexWorkGraph"]["parent"]["senderThreadId"],
        "thread-parent"
    );
    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["threadId"],
        "thread-child"
    );
    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["childRunId"],
        "agent-run-child-1"
    );
    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["status"],
        "acknowledged"
    );
    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["edgeId"],
        "collab-call-snake:0:sendInput:agent-run-child-1"
    );
}

#[test]
pub(crate) fn codex_headless_tool_events_build_work_graph_from_child_run_ids_only() {
    let start = codex_headless_tool_event_from_json(&serde_json::json!({
        "type": "tool_call",
        "call_id": "collab-call-child-only",
        "tool": "codex.subagent.wait",
        "args": {
            "child_run_ids": ["agent-run-child-only"]
        }
    }))
    .expect("start event");

    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["childRunId"],
        "agent-run-child-only"
    );
    assert!(start.args["codexWorkGraph"]["childRuns"][0]["threadId"].is_null());
}

#[test]
pub(crate) fn codex_headless_tool_events_normalize_subagent_aliases() {
    let start = codex_headless_tool_event_from_json(&serde_json::json!({
        "type": "tool_call",
        "call_id": "collab-call-5",
        "tool": "codex.subagent.resume_subagent",
        "args": {
            "codexTool": "resume_subagent",
            "receiverThreadIds": ["thread-child"],
            "childRunIds": ["agent-run-child-1"]
        }
    }))
    .expect("start event");
    let end = codex_headless_tool_event_from_json(&serde_json::json!({
        "type": "tool_end",
        "call_id": "collab-call-5",
        "tool": "codex.subagent.resume_subagent",
        "success": true
    }))
    .expect("end event");

    assert_eq!(start.tool_name, "codex.subagent.resumeAgent");
    assert_eq!(start.args["codexTool"], serde_json::json!("resumeAgent"));
    assert_eq!(
        start.args["codexWorkGraph"]["tool"],
        serde_json::json!("resumeAgent")
    );
    assert_eq!(
        start.args["codexWorkGraph"]["childRuns"][0]["operation"],
        serde_json::json!("resumeAgent")
    );
    assert_eq!(end.tool_name, "codex.subagent.resumeAgent");
    assert_eq!(
        end.result["content"][0]["text"],
        "codex.subagent.resumeAgent completed"
    );
}

#[test]
pub(crate) fn codex_headless_subagent_lifecycle_matches_workgraph_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../docs/protocols/codex-subagent-workgraph-v1.json"
    ))
    .expect("work graph fixture should parse");
    assert_eq!(
        fixture["schemaVersion"],
        serde_json::json!(CODEX_SUBAGENT_WORK_GRAPH_SCHEMA)
    );
    assert_eq!(fixture["toolPrefix"], "codex.subagent.");
    assert_eq!(fixture["threadChildRunPrefix"], "codex-thread:");

    let operations = fixture["operations"]
        .as_array()
        .expect("fixture operations should be an array");
    let mut seen_tools = Vec::new();
    for operation in operations {
        let tool = operation["tool"].as_str().expect("tool");
        let operation_name = operation["operation"].as_str().expect("operation");
        let aliases = operation["aliases"].as_array().expect("aliases");
        assert_eq!(codex_canonical_collab_tool(tool), tool);
        assert_eq!(codex_canonical_collab_tool(operation_name), tool);
        seen_tools.push(tool.to_string());

        for alias in aliases {
            let alias = alias.as_str().expect("alias");
            assert_eq!(codex_canonical_collab_tool(alias), tool);
            let start = codex_headless_tool_event_from_json(&serde_json::json!({
                "type": "tool_call",
                "call_id": format!("collab-{operation_name}"),
                "tool": format!("codex.subagent.{alias}"),
                "args": {
                    "codexTool": alias,
                    "senderThreadId": "thread-parent",
                    "receiverThreadIds": ["thread-child"],
                    "childRunIds": ["agent-run-child-1"]
                }
            }))
            .expect("start event");

            assert_eq!(start.tool_name, format!("codex.subagent.{tool}"));
            assert_eq!(start.args["codexTool"], serde_json::json!(tool));
            assert_eq!(start.args["senderThreadId"], "thread-parent");
            assert_eq!(
                start.args["codexWorkGraph"]["schemaVersion"],
                serde_json::json!(CODEX_SUBAGENT_WORK_GRAPH_SCHEMA)
            );
            assert_eq!(
                start.args["codexWorkGraph"]["tool"],
                serde_json::json!(tool)
            );
            assert_eq!(
                start.args["codexWorkGraph"]["childRuns"][0]["operation"],
                serde_json::json!(tool)
            );
            assert_eq!(
                start.args["codexWorkGraph"]["childRuns"][0]["childRunId"],
                "agent-run-child-1"
            );
        }
    }

    assert_eq!(
        seen_tools,
        vec![
            "spawnAgent",
            "sendInput",
            "resumeAgent",
            "wait",
            "closeAgent"
        ]
    );
}

#[test]
pub(crate) fn codex_headless_tool_end_uses_start_context_when_end_omits_tool_name() {
    let mut contexts = HashMap::new();
    let _ = codex_headless_tool_event_from_json_with_context(
        &serde_json::json!({
            "type": "tool_call",
            "call_id": "collab-call-4",
            "tool": "codex.subagent.spawnAgent",
            "args": {
                "codexTool": "spawnAgent",
                "receiverThreadIds": []
            }
        }),
        &mut contexts,
    )
    .expect("start event");
    let end = codex_headless_tool_event_from_json_with_context(
        &serde_json::json!({
            "type": "tool_end",
            "call_id": "collab-call-4",
            "success": true,
            "details": {
                "receiverThreadIds": ["thread-child-complete"],
                "childRunIds": ["agent-run-child-complete"]
            }
        }),
        &mut contexts,
    )
    .expect("end event");

    assert_eq!(end.tool_name, "codex.subagent.spawnAgent");
    assert_eq!(end.summary_label.as_deref(), Some("spawn agent"));
    assert_eq!(
        end.result["content"][0]["text"],
        "codex.subagent.spawnAgent completed"
    );
    assert_eq!(end.result["details"]["args"]["codexTool"], "spawnAgent");
    assert_eq!(
        end.result["details"]["receiverThreadIds"],
        serde_json::json!(["thread-child-complete"])
    );
    assert_eq!(
        end.result["details"]["childRunIds"],
        serde_json::json!(["agent-run-child-complete"])
    );
}

#[test]
pub(crate) fn codex_bridge_prompt_body_lists_attachment_paths() {
    let body = codex_bridge_prompt_body(
        "Summarize the upload",
        &[
            "/tmp/maestro-chat/a/report.pdf".to_string(),
            "/tmp/maestro-chat/a/screenshot.png".to_string(),
        ],
    );
    assert!(body.contains("Summarize the upload"));
    assert!(body.contains("/tmp/maestro-chat/a/report.pdf"));
    assert!(body.contains("/tmp/maestro-chat/a/screenshot.png"));
    assert!(!body.contains("Use the read tool"));
    assert!(!body.contains("prompt.md"));
}

#[tokio::test]
pub(crate) async fn codex_bridge_prompt_uses_direct_argument_for_normal_requests() {
    let cwd = TestDir::new("codex-bridge-direct-prompt");
    let prepared = prepare_codex_bridge_prompt(cwd.path(), "Summarize the upload", &[])
        .await
        .expect("prompt should be prepared");
    assert!(prepared.argument.contains("Summarize the upload"));
    assert!(!prepared.argument.contains("Use the read tool"));
    assert!(!prepared.temp_dir.join("prompt.md").exists());
    let _ = tokio::fs::remove_dir_all(&prepared.temp_dir).await;
}

#[tokio::test]
pub(crate) async fn codex_bridge_prompt_uses_file_for_oversized_requests() {
    let cwd = TestDir::new("codex-bridge-file-prompt");
    let prompt = "x".repeat(CODEX_BRIDGE_DIRECT_ARG_MAX_BYTES + 1);
    let prepared = prepare_codex_bridge_prompt(cwd.path(), &prompt, &[])
        .await
        .expect("prompt should be prepared");
    let prompt_path = prepared.temp_dir.join("prompt.md");
    let stored = tokio::fs::read_to_string(&prompt_path)
        .await
        .expect("prompt file should be readable");
    assert!(prepared.argument.contains("Use the read tool"));
    assert!(
        prepared
            .argument
            .contains(&prompt_path.display().to_string())
    );
    assert!(stored.contains(&prompt));
    let _ = tokio::fs::remove_dir_all(&prepared.temp_dir).await;
}

#[test]
pub(crate) fn codex_bridge_temp_dirs_are_unique_per_request() {
    let _guard = ENV_LOCK.blocking_lock();
    let previous_bridge = env::var_os("MAESTRO_CODEX_APP_SERVER_SANDBOX");
    let previous_sandbox = env::var_os("MAESTRO_SANDBOX_MODE");
    env::remove_var("MAESTRO_CODEX_APP_SERVER_SANDBOX");
    env::remove_var("MAESTRO_SANDBOX_MODE");
    let cwd = TestDir::new("codex-bridge-temp-cwd");
    let first = codex_bridge_temp_dir(cwd.path());
    let second = codex_bridge_temp_dir(cwd.path());
    if let Some(previous) = previous_bridge {
        env::set_var("MAESTRO_CODEX_APP_SERVER_SANDBOX", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_SANDBOX");
    }
    if let Some(previous) = previous_sandbox {
        env::set_var("MAESTRO_SANDBOX_MODE", previous);
    } else {
        env::remove_var("MAESTRO_SANDBOX_MODE");
    }

    assert_ne!(first, second);
    assert!(
        first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("maestro-codex-bridge-"))
    );
}

#[test]
pub(crate) fn codex_bridge_temp_dir_uses_workspace_for_docker_sandbox() {
    let _guard = ENV_LOCK.blocking_lock();
    let cwd = TestDir::new("codex-bridge-docker-cwd");
    let previous_bridge = env::var_os("MAESTRO_CODEX_APP_SERVER_SANDBOX");
    let previous_sandbox = env::var_os("MAESTRO_SANDBOX_MODE");
    env::set_var("MAESTRO_CODEX_APP_SERVER_SANDBOX", "docker");
    env::remove_var("MAESTRO_SANDBOX_MODE");

    let temp_dir = codex_bridge_temp_dir(cwd.path());

    if let Some(previous) = previous_bridge {
        env::set_var("MAESTRO_CODEX_APP_SERVER_SANDBOX", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_SANDBOX");
    }
    if let Some(previous) = previous_sandbox {
        env::set_var("MAESTRO_SANDBOX_MODE", previous);
    } else {
        env::remove_var("MAESTRO_SANDBOX_MODE");
    }

    assert!(temp_dir.starts_with(cwd.path()));
    assert!(
        temp_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".maestro-codex-bridge-"))
    );
}

#[test]
pub(crate) fn codex_headless_pending_request_ids_include_run_id() {
    let first = codex_headless_pending_request_id(Some("session-1"), "run-a", "approval-1");
    let second = codex_headless_pending_request_id(Some("session-1"), "run-b", "approval-1");

    assert_eq!(first, "codex:session-1:run-a:approval-1");
    assert_eq!(second, "codex:session-1:run-b:approval-1");
    assert_ne!(first, second);
}

#[test]
pub(crate) fn codex_app_server_approval_mode_preserves_prompt() {
    assert_eq!(codex_app_server_approval_mode("auto"), "auto");
    assert_eq!(codex_app_server_approval_mode("prompt"), "prompt");
    assert_eq!(codex_app_server_approval_mode("fail"), "fail");
    assert_eq!(codex_app_server_approval_mode(""), "prompt");
}

#[test]
pub(crate) fn codex_app_server_cli_path_resolves_from_package_root_not_workspace() {
    let package = TestDir::new("codex-package-root");
    let workspace = TestDir::new("codex-workspace");
    let package_bin = package.path().join("bin");
    let package_dist = package.path().join("bin");
    let workspace_dist = workspace.path().join("bin");
    fs::create_dir_all(&package_bin).expect("package bin should be created");
    fs::create_dir_all(&package_dist).expect("package dist should be created");
    fs::create_dir_all(&workspace_dist).expect("workspace dist should be created");
    fs::write(package.path().join("package.json"), "{}").expect("package json should exist");
    fs::write(package_dist.join("maestro"), "").expect("package cli should exist");
    fs::write(workspace_dist.join("maestro"), "").expect("workspace cli should exist");

    let resolved = codex_app_server_cli_path_from_start_dir(&package_bin);

    assert_eq!(resolved, package_dist.join("maestro"));
    assert_ne!(resolved, workspace_dist.join("maestro"));
}

#[test]
pub(crate) fn codex_app_server_cli_path_missing_cli_still_points_at_package_root() {
    let package = TestDir::new("codex-package-root-missing-cli");
    let package_bin = package.path().join("bin");
    fs::create_dir_all(&package_bin).expect("package bin should be created");
    fs::write(package.path().join("package.json"), "{}").expect("package json should exist");

    let resolved = codex_app_server_cli_path_from_start_dir(&package_bin);

    assert_eq!(resolved, package.path().join("bin/maestro"));
}

#[tokio::test]
pub(crate) async fn codex_app_server_cli_isolates_child_usage_file() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-isolated-usage");
    let cli_path = root.path().join("cli.js");
    let marker_path = root.path().join("child-usage-file.txt");
    let argv_marker_path = root.path().join("child-argv.json");
    let server_usage_path = root.path().join("server-usage.json");
    fs::write(
            &cli_path,
            r#"const fs = require("fs");
fs.writeFileSync(process.env.MAESTRO_USAGE_MARKER, process.env.MAESTRO_USAGE_FILE || "");
fs.writeFileSync(process.env.MAESTRO_ARGV_MARKER, JSON.stringify(process.argv.slice(2)));
console.log(JSON.stringify({ type: "turn", phase: "start", role: "assistant" }));
console.log(JSON.stringify({ type: "item", subtype: "message_complete", data: { text: "ok", stopReason: "stop" } }));
"#,
        )
        .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_usage_file = env::var_os("MAESTRO_USAGE_FILE");
    let previous_marker = env::var_os("MAESTRO_USAGE_MARKER");
    let previous_argv_marker = env::var_os("MAESTRO_ARGV_MARKER");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_USAGE_FILE", &server_usage_path);
    env::set_var("MAESTRO_USAGE_MARKER", &marker_path);
    env::set_var("MAESTRO_ARGV_MARKER", &argv_marker_path);

    let output = run_codex_app_server_cli(root.path(), "gpt-5.5", "fail", "hello", &[])
        .await
        .expect("fixture should emit assistant output");
    let child_usage_file =
        fs::read_to_string(&marker_path).expect("child should record usage file path");
    let child_argv: Vec<String> = serde_json::from_str(
        &fs::read_to_string(&argv_marker_path).expect("child should record argv"),
    )
    .expect("child argv should be JSON");

    if let Some(previous) = previous_cli {
        env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_CLI");
    }
    if let Some(previous) = previous_usage_file {
        env::set_var("MAESTRO_USAGE_FILE", previous);
    } else {
        env::remove_var("MAESTRO_USAGE_FILE");
    }
    if let Some(previous) = previous_marker {
        env::set_var("MAESTRO_USAGE_MARKER", previous);
    } else {
        env::remove_var("MAESTRO_USAGE_MARKER");
    }
    if let Some(previous) = previous_argv_marker {
        env::set_var("MAESTRO_ARGV_MARKER", previous);
    } else {
        env::remove_var("MAESTRO_ARGV_MARKER");
    }

    assert_eq!(output.text, "ok");
    assert_ne!(PathBuf::from(child_usage_file.trim()), server_usage_path);
    assert!(child_usage_file.trim().ends_with("usage.json"));
    assert_eq!(child_argv.first().map(String::as_str), Some("exec"));
    assert!(child_argv.iter().any(|arg| arg == "--json"));
    assert!(!child_argv.iter().any(|arg| arg == "--mode"));
}

#[tokio::test]
pub(crate) async fn codex_headless_bridge_round_trips_prompt_approval() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-headless-approval");
    let cli_path = root.path().join("cli.js");
    let marker_path = root.path().join("approval-response.json");
    fs::write(
            &cli_path,
            r#"const fs = require("fs");
const readline = require("readline");
const marker = process.env.MAESTRO_APPROVAL_RESPONSE_MARKER;
const rl = readline.createInterface({ input: process.stdin, terminal: false });
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    send({
      type: "server_request",
      request_id: "approval-1",
      request_type: "approval",
      call_id: "approval-1",
      tool: "write",
      args: { path: "file.txt" },
      reason: "Need approval"
    });
  } else if (msg.type === "server_request_response") {
    fs.writeFileSync(marker, JSON.stringify(msg));
    send({ type: "response_start", response_id: "response-1" });
    send({ type: "response_chunk", response_id: "response-1", content: "approved output", is_thinking: false });
    send({
      type: "response_end",
      response_id: "response-1",
      usage: {
        input_tokens: 5,
        output_tokens: 2,
        cache_read_tokens: 1,
        cache_write_tokens: 0,
        total_tokens: 8,
        total_cost_usd: 0.01,
        model_id: "gpt-5.5",
        provider: "openai-codex"
      },
      tools_summary: { tools_used: ["write"], calls_succeeded: 1, calls_failed: 0 },
      duration_ms: 1
    });
    send({ type: "turn_completed", response_id: "response-1" });
  } else if (msg.type === "shutdown") {
    process.exit(0);
  }
});
"#,
        )
        .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_marker = env::var_os("MAESTRO_APPROVAL_RESPONSE_MARKER");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_APPROVAL_RESPONSE_MARKER", &marker_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "5000");

    let state = test_app_state_with_sessions(HashMap::new());
    let (_client, server) = tcp_stream_pair().await;
    let cwd = root.path().to_path_buf();
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        let mut server = server;
        run_codex_app_server_headless_cli(
            &mut server,
            CodexBridgeTransport::Sse,
            &state_for_run,
            Some("session-1"),
            &cwd,
            "gpt-5.5",
            "hello",
            &[],
        )
        .await
    });

    // Registration can take several seconds under loaded CI runners (cold node).
    let deadline = Instant::now() + Duration::from_secs(15);
    let (external_request_id, sender) = loop {
        let mut pending = state.pending_tool_responses.lock().await;
        let pending_id = pending
            .keys()
            .find(|id| id.starts_with("codex:session-1:") && id.ends_with(":approval-1"))
            .cloned();
        if let Some(pending_id) = pending_id {
            let sender = pending
                .remove(&pending_id)
                .expect("pending approval sender should exist");
            break (pending_id, sender);
        }
        drop(pending);
        assert!(
            Instant::now() < deadline,
            "headless approval request should be registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    sender
        .send((
            external_request_id,
            true,
            None,
            ExecutionSource::RemoteClient,
            None,
        ))
        .expect("approval response should send");
    let result = run.await.expect("headless run should join");

    if let Some(previous) = previous_cli {
        env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_CLI");
    }
    if let Some(previous) = previous_marker {
        env::set_var("MAESTRO_APPROVAL_RESPONSE_MARKER", previous);
    } else {
        env::remove_var("MAESTRO_APPROVAL_RESPONSE_MARKER");
    }
    if let Some(previous) = previous_timeout {
        env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", previous);
    } else {
        env::remove_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    }

    let output = result.expect("headless bridge should complete");
    let approval_response: Value = serde_json::from_str(
        &fs::read_to_string(&marker_path).expect("approval response should be recorded"),
    )
    .expect("approval response should be json");

    assert_eq!(output.text, "approved output");
    assert_eq!(output.usage.expect("usage").input_tokens, 5);
    assert_eq!(approval_response["type"], "server_request_response");
    assert_eq!(approval_response["request_id"], "approval-1");
    assert_eq!(approval_response["approved"], true);
}

#[tokio::test]
pub(crate) async fn codex_headless_approval_wait_does_not_consume_request_timeout() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-headless-approval-timeout");
    let cli_path = root
        .path()
        .join(if cfg!(unix) { "cli.sh" } else { "cli.js" });
    // Keep cold Node startup outside the approval timeout assertion on Unix.
    // This fixture only exchanges fixed JSON lines; shell builtins suffice.
    let fixture = if cfg!(unix) {
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"server_request","request_id":"approval-wait","request_type":"approval","call_id":"approval-wait","tool":"write","args":{"path":"file.txt"},"reason":"Need approval"}' ;;
    *'"type":"server_request_response"'*)
      printf '%s\n' '{"type":"response_start","response_id":"response-1"}' '{"type":"response_chunk","response_id":"response-1","content":"waited output","is_thinking":false}' '{"type":"response_end","response_id":"response-1","duration_ms":1}' '{"type":"turn_completed","response_id":"response-1"}' ;;
    *'"type":"shutdown"'*) exit 0 ;;
  esac
done
"#
    } else {
        r#"const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, terminal: false });
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    send({
      type: "server_request",
      request_id: "approval-wait",
      request_type: "approval",
      call_id: "approval-wait",
      tool: "write",
      args: { path: "file.txt" },
      reason: "Need approval"
    });
  } else if (msg.type === "server_request_response") {
    send({ type: "response_start", response_id: "response-1" });
    send({ type: "response_chunk", response_id: "response-1", content: "waited output", is_thinking: false });
    send({ type: "response_end", response_id: "response-1", duration_ms: 1 });
    send({ type: "turn_completed", response_id: "response-1" });
  } else if (msg.type === "shutdown") {
    process.exit(0);
  }
});
"#
    };
    fs::write(&cli_path, fixture).expect("cli fixture should be written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cli_path, fs::Permissions::from_mode(0o700))
            .expect("CLI fixture should be executable");
    }

    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "2000");

    let state = test_app_state_with_sessions(HashMap::new());
    let (_client, server) = tcp_stream_pair().await;
    let cwd = root.path().to_path_buf();
    let state_for_run = state.clone();
    let run = tokio::spawn(async move {
        let mut server = server;
        run_codex_app_server_headless_cli(
            &mut server,
            CodexBridgeTransport::Sse,
            &state_for_run,
            Some("session-wait"),
            &cwd,
            "gpt-5.5",
            "hello",
            &[],
        )
        .await
    });

    let expected_prefix = "codex:session-wait:";
    let expected_suffix = ":approval-wait";
    // Registration can take several seconds under loaded CI runners (cold
    // Node plus concurrent Rust tests). This is an outer scheduling bound;
    // the two-second request timeout below remains the behavior under test.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if state
            .pending_tool_responses
            .lock()
            .await
            .keys()
            .any(|id| id.starts_with(expected_prefix) && id.ends_with(expected_suffix))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "headless approval request should be registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    tokio::time::sleep(Duration::from_millis(2_150)).await;
    let mut pending = state.pending_tool_responses.lock().await;
    let external_request_id = pending
        .keys()
        .find(|id| id.starts_with(expected_prefix) && id.ends_with(expected_suffix))
        .cloned()
        .expect("approval wait should remain pending after request timeout window");
    let sender = pending
        .remove(&external_request_id)
        .expect("approval wait should have a pending sender");
    drop(pending);
    sender
        .send((
            external_request_id,
            true,
            None,
            ExecutionSource::RemoteClient,
            None,
        ))
        .expect("approval response should send");
    let result = run.await.expect("headless run should join");

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

    assert_eq!(
        result
            .expect("manual approval wait should not time out")
            .text,
        "waited output"
    );
}

#[tokio::test]
pub(crate) async fn codex_headless_bridge_keeps_valid_output_after_nonzero_shutdown() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-headless-nonzero-shutdown");
    let cli_path = root.path().join("cli.js");
    fs::write(
            &cli_path,
            r#"const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, terminal: false });
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    send({ type: "response_start", response_id: "response-1" });
    send({ type: "response_chunk", response_id: "response-1", content: "kept output", is_thinking: false });
    send({
      type: "response_end",
      response_id: "response-1",
      usage: {
        input_tokens: 1,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        total_tokens: 2,
        total_cost_usd: 0.01,
        model_id: "gpt-5.5",
        provider: "openai-codex"
      },
      tools_summary: { tools_used: [], calls_succeeded: 0, calls_failed: 0 },
      duration_ms: 1
    });
    send({ type: "turn_completed", response_id: "response-1" });
  } else if (msg.type === "shutdown") {
    process.exit(7);
  }
});
"#,
        )
        .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "5000");

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

    assert_eq!(
        output.expect("valid protocol output should win").text,
        "kept output"
    );
}

#[tokio::test]
pub(crate) async fn codex_headless_bridge_keeps_valid_output_after_broken_pipe_shutdown() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-headless-broken-pipe-shutdown");
    let cli_path = root.path().join("cli.js");
    fs::write(
            &cli_path,
            r#"const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, terminal: false });
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.type === "prompt") {
    send({ type: "response_start", response_id: "response-1" });
    send({ type: "response_chunk", response_id: "response-1", content: "kept after broken pipe", is_thinking: false });
    send({ type: "response_end", response_id: "response-1", duration_ms: 1 });
    send({ type: "turn_completed", response_id: "response-1" });
    process.stdin.destroy();
    setTimeout(() => process.exit(0), 10);
  } else if (msg.type === "shutdown") {
    process.exit(0);
  }
});
"#,
        )
        .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "5000");

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

    assert_eq!(
        output
            .expect("valid output should survive a broken shutdown pipe")
            .text,
        "kept after broken pipe"
    );
}

#[tokio::test]
pub(crate) async fn codex_app_server_timeout_kills_child_process() {
    let _guard = ENV_LOCK.lock().await;
    let root = TestDir::new("codex-timeout");
    let cli_path = root.path().join("cli.js");
    let marker_path = root.path().join("still-running.txt");
    fs::write(
        &cli_path,
        r#"const fs = require("fs");
setTimeout(() => {
  fs.writeFileSync(process.env.MAESTRO_TIMEOUT_MARKER, "still running");
}, 150);
"#,
    )
    .expect("cli fixture should be written");
    let previous_cli = env::var_os("MAESTRO_CODEX_APP_SERVER_CLI");
    let previous_timeout = env::var_os("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS");
    let previous_marker = env::var_os("MAESTRO_TIMEOUT_MARKER");
    env::set_var("MAESTRO_CODEX_APP_SERVER_CLI", &cli_path);
    env::set_var("MAESTRO_CODEX_APP_SERVER_TIMEOUT_MS", "50");
    env::set_var("MAESTRO_TIMEOUT_MARKER", &marker_path);

    let result = run_codex_app_server_cli(root.path(), "gpt-5.5", "fail", "hello", &[]).await;

    tokio::time::sleep(Duration::from_millis(250)).await;
    let marker_exists = marker_path.exists();

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
    if let Some(previous) = previous_marker {
        env::set_var("MAESTRO_TIMEOUT_MARKER", previous);
    } else {
        env::remove_var("MAESTRO_TIMEOUT_MARKER");
    }

    assert!(matches!(
        result,
        Err(ref error) if error == "Codex app-server request timed out"
    ));
    assert!(
        !marker_exists,
        "timed out child process should be terminated before it can keep running"
    );
}
