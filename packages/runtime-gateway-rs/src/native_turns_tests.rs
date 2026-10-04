use super::*;

fn binding() -> TurnBinding {
    TurnBinding {
        session_id: "session-1".into(),
        session_created_at: "created-1".into(),
        subject: Some("user-1".into()),
        organization_id: Some("org-1".into()),
        workspace_id: Some("workspace-1".into()),
        source: AuthSource::IdentityJwt,
        cwd: PathBuf::from("/workspace"),
    }
}
fn request(content: &str) -> ChatRequest {
    serde_json::from_value(
        serde_json::json!({"messages":[{"role":"user","content":content}],"sessionId":"session-1"}),
    )
    .unwrap()
}
fn accept(runtime: &NativeTurnRuntime, id: &str, content: &str) -> AcceptedTurn {
    runtime
        .accept(
            binding(),
            id.into(),
            request(content),
            AuthContext::default(),
            true,
            None,
        )
        .unwrap()
}

#[test]
fn accepted_retry_joins_one_owner_and_changed_payload_conflicts() {
    let runtime = NativeTurnRuntime::default();
    let first = accept(&runtime, "turn-1", "hello");
    let second = accept(&runtime, "turn-1", "hello");
    assert!(Arc::ptr_eq(&first.turn, &second.turn));
    assert!(first.start_lane);
    assert!(!second.start_lane);
    assert!(
        runtime
            .accept(
                binding(),
                "turn-1".into(),
                request("other"),
                AuthContext::default(),
                true,
                None
            )
            .is_err()
    );
    assert_eq!(runtime.entries.lock().unwrap().turns.len(), 1);
}

#[test]
fn principal_tenant_workspace_and_session_generation_are_exact() {
    let runtime = NativeTurnRuntime::default();
    accept(&runtime, "turn-1", "hello");
    for changed in 0..5 {
        let mut other = binding();
        match changed {
            0 => other.subject = Some("other".into()),
            1 => other.organization_id = Some("other".into()),
            2 => other.workspace_id = Some("other".into()),
            3 => other.session_created_at = "new-generation".into(),
            _ => other.cwd = PathBuf::from("/other-workspace"),
        }
        assert!(runtime.find(&other, "turn-1").is_none());
        assert!(
            runtime
                .accept(
                    other,
                    "turn-1".into(),
                    request("hello"),
                    AuthContext::default(),
                    true,
                    None
                )
                .is_err()
        );
    }
}

#[tokio::test]
async fn disconnect_during_tool_or_approval_does_not_reexecute_or_change_owner() {
    use crate::chat_output::{ChatEventWriter, NativeTurnOutput};
    let runtime = NativeTurnRuntime::default();
    let first = accept(&runtime, "turn-1", "hello");
    let mut output = NativeTurnOutput(first.turn.clone());
    let approval = serde_json::json!({"type":"action_approval_required","request":{"id":"approval-1","toolName":"shell"}});
    output
        .send_event(
            CodexBridgeTransport::WebSocket,
            &serde_json::json!({"type":"tool_execution_start","toolCallId":"tool-1"}),
        )
        .await
        .unwrap();
    output
        .send_event(CodexBridgeTransport::WebSocket, &approval)
        .await
        .unwrap();
    output.close().await.unwrap();
    assert!(!first.turn.cancel.is_cancelled());
    let retry = accept(&runtime, "turn-1", "hello");
    assert!(!retry.start_lane);
    assert!(Arc::ptr_eq(&first.turn, &retry.turn));
    let snapshot = retry.turn.snapshot(&runtime.epoch, 1, Vec::new());
    assert_eq!(snapshot["pendingApprovals"][0], approval);
    assert_eq!(snapshot["events"].as_array().unwrap().len(), 1);
    assert_eq!(retry.turn.binding, binding());
}

#[test]
fn queue_edits_removals_stale_generations_and_stop_are_owner_transitions() {
    let runtime = NativeTurnRuntime::default();
    let first = accept(&runtime, "turn-1", "one");
    let second = accept(&runtime, "turn-2", "two");
    let active = runtime.next(&binding().lane()).unwrap();
    assert_eq!(active.id, first.turn.id);
    assert!(!second.start_lane);
    let generation = second.turn.data.lock().unwrap().generation;
    second.turn.edit(generation, request("edited")).unwrap();
    assert!(second.turn.remove(generation).is_err());
    second.turn.remove(generation + 1).unwrap();
    assert_eq!(second.turn.data.lock().unwrap().state, TurnState::Cancelled);
    let generation = active.data.lock().unwrap().generation;
    active.stop(generation).unwrap();
    assert!(active.cancel.is_cancelled());
    assert_eq!(active.data.lock().unwrap().state, TurnState::Stopping);
    active.publish(serde_json::json!({"type":"done"}));
    assert_eq!(active.data.lock().unwrap().state, TurnState::Cancelled);
    assert!(runtime.next(&binding().lane()).is_none());
}

#[test]
fn recoverable_actor_error_preserves_successful_native_completion() {
    let runtime = NativeTurnRuntime::default();
    let turn = accept(&runtime, "turn-1", "one").turn;
    runtime.next(&binding().lane()).unwrap();
    let warning = serde_json::json!({
        "type":"error", "message":"Tool arguments were invalid",
        "fatal":false, "terminal":false
    });
    turn.publish(warning.clone());
    let running = turn.snapshot(&runtime.epoch, 0, Vec::new());
    assert_eq!(running["state"], "running");
    assert!(running["error"].is_null());
    assert_eq!(running["events"][0]["event"], warning);
    turn.publish(serde_json::json!({
        "type":"message_end", "message":{"role":"assistant","content":"Recovered answer"}
    }));
    turn.publish(serde_json::json!({"type":"done"}));
    let completed = turn.snapshot(&runtime.epoch, 0, Vec::new());
    assert_eq!(completed["state"], "completed");
    assert!(completed["error"].is_null());
    assert_eq!(completed["message"]["content"], "Recovered answer");
}

#[test]
fn terminal_and_unclassified_errors_keep_native_completion_failed() {
    for classification in [
        serde_json::json!({"fatal":true,"terminal":false}),
        serde_json::json!({"fatal":false,"terminal":true}),
        serde_json::json!({"fatal":true,"terminal":true}),
        serde_json::json!({}),
        serde_json::json!({"fatal":false}),
        serde_json::json!({"terminal":false}),
        serde_json::json!({"fatal":"false","terminal":false}),
    ] {
        let runtime = NativeTurnRuntime::default();
        let turn = accept(&runtime, "turn-1", "one").turn;
        runtime.next(&binding().lane()).unwrap();
        let mut failure = classification;
        failure["type"] = serde_json::json!("error");
        failure["message"] = serde_json::json!("Turn failed");
        turn.publish(failure.clone());
        turn.publish(serde_json::json!({"type":"done"}));
        let completed = turn.snapshot(&runtime.epoch, 0, Vec::new());
        assert_eq!(completed["state"], "failed", "{failure}");
        assert_eq!(completed["error"], "Turn failed");
    }
}

#[test]
fn bounded_replay_requires_snapshot_and_preserves_approval_projection() {
    let runtime = NativeTurnRuntime::default();
    let turn = accept(&runtime, "turn-1", "one").turn;
    turn.publish(
        serde_json::json!({"type":"action_approval_required","request":{"id":"approval-1"}}),
    );
    for index in 0..(MAX_REPLAY_EVENTS + 5) {
        turn.publish(serde_json::json!({"type":"status","status":format!("event-{index}")}));
    }
    let snapshot = turn.snapshot(&runtime.epoch, 1, Vec::new());
    assert_eq!(snapshot["resetRequired"], true);
    assert!(snapshot["events"].as_array().unwrap().len() <= MAX_REPLAY_EVENTS);
    assert_eq!(
        snapshot["pendingApprovals"][0]["request"]["id"],
        "approval-1"
    );
    assert!(turn.data.lock().unwrap().replay_bytes <= MAX_REPLAY_BYTES);
}

#[test]
fn restarted_owner_does_not_recover_or_resubmit_an_old_turn() {
    let first = NativeTurnRuntime::default();
    accept(&first, "turn-1", "one");
    let restarted = NativeTurnRuntime::default();
    assert_ne!(first.epoch, restarted.epoch);
    assert!(restarted.find(&binding(), "turn-1").is_none());
    assert!(restarted.next(&binding().lane()).is_none());
}

#[tokio::test]
async fn fork_prefix_reaches_the_native_actor_once_and_stale_generation_is_denied() {
    use maestro_local_host::ai::ScriptedResponse;
    use maestro_local_host::embedding::test_kit::ScriptedEmbeddingBuilder;
    let mut session = crate::tests::test_session_record("session-1");
    session.created_at = "created-1".into();
    session.messages = vec![
        serde_json::json!({"role":"user","content":"unique completed fork question"}),
        serde_json::json!({"role":"assistant","content":"unique completed fork answer","turnCompleted":true,"turnIndex":0}),
    ];
    let state =
        crate::tests::test_app_state_with_sessions(HashMap::from([("session-1".into(), session)]));
    let auth = AuthContext {
        unrestricted: true,
        source: AuthSource::StaticGatewayKey,
        ..Default::default()
    };
    let runtime = NativeTurnRuntime::default();
    let accepted = runtime
        .accept(
            binding(),
            "fork-next".into(),
            request("unique new prompt"),
            auth,
            true,
            None,
        )
        .unwrap();
    let hydrated = execution_request(&state, &accepted.turn).await.unwrap();
    assert_eq!(hydrated.messages.len(), 3);
    let workspace = tempfile::tempdir().unwrap();
    let (agent, mut events) =
        ScriptedEmbeddingBuilder::new(vec![ScriptedResponse::text("continued fork")])
            .working_directory(workspace.path())
            .start()
            .unwrap()
            .into_parts();
    agent
        .prompt(crate::chat::build_prompt_from_chat(&hydrated))
        .await
        .unwrap();
    let mut observed = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            FromAgent::ConversationSnapshot { messages, .. } => {
                observed = Some(serde_json::to_string(&messages).unwrap())
            }
            FromAgent::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    agent.shutdown().await;
    let observed = observed.expect("real actor persisted its provider conversation");
    assert_eq!(
        observed.matches("unique completed fork question").count(),
        1
    );
    assert_eq!(observed.matches("unique completed fork answer").count(), 1);
    assert_eq!(observed.matches("unique new prompt").count(), 1);
    state
        .sessions
        .lock()
        .await
        .sessions
        .get_mut("session-1")
        .unwrap()
        .created_at = "replaced-generation".into();
    assert!(execution_request(&state, &accepted.turn).await.is_err());
}

#[tokio::test]
async fn one_native_actor_retains_tool_approval_when_its_observer_rejoins() {
    use maestro_local_host::ai::{ScriptedBlock, ScriptedResponse, StopReason, Tool};
    use maestro_local_host::embedding::{EmbeddedRunProgress, test_kit::ScriptedEmbeddingBuilder};
    let workspace = tempfile::tempdir().unwrap();
    let runtime = NativeTurnRuntime::default();
    let accepted = accept(&runtime, "turn-tool", "Use caller tool");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "call-owned".into(),
                name: "lookup_status".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse::text("finished once after the retained approval"),
    ])
    .working_directory(workspace.path())
    .external_tools([ToolDefinition {
        tool: Tool::new("lookup_status", "Caller-owned lookup")
            .with_schema(serde_json::json!({"type":"object","properties":{}})),
        requires_approval: true,
    }])
    .start_runner()
    .unwrap();
    let pending = match runner.run("Use caller tool").await.unwrap() {
        EmbeddedRunProgress::AwaitingTool(pending) => pending,
        EmbeddedRunProgress::Completed(_) => panic!("must await actual native tool approval"),
    };
    accepted.turn.publish(serde_json::json!({"type":"action_approval_required","request":{"id":"call-owned","toolName":"lookup_status"}}));
    let retry = accept(&runtime, "turn-tool", "Use caller tool");
    assert!(!retry.start_lane);
    assert!(Arc::ptr_eq(&retry.turn, &accepted.turn));
    runner
        .external_result(&pending, ToolResult::success("owned response"))
        .unwrap();
    let completed = match runner.resume().await.unwrap() {
        EmbeddedRunProgress::Completed(completed) => completed,
        EmbeddedRunProgress::AwaitingTool(_) => {
            panic!("rejoin must not cause another tool execution")
        }
    };
    assert_eq!(
        completed.output(),
        "finished once after the retained approval"
    );
    runner.shutdown().await;
}

#[test]
fn session_generation_orders_principals_without_disclosing_another_owner() {
    let runtime = NativeTurnRuntime::default();
    let first = accept(&runtime, "turn-1", "first");
    let mut second_binding = binding();
    second_binding.source = AuthSource::StaticGatewayKey;
    second_binding.subject = None;
    let second = runtime
        .accept(
            second_binding.clone(),
            "turn-2".into(),
            request("second"),
            AuthContext::default(),
            true,
            None,
        )
        .unwrap();
    assert!(!second.start_lane);
    assert_eq!(runtime.next(&binding().lane()).unwrap().id, first.turn.id);
    first.turn.publish(serde_json::json!({"type":"done"}));
    assert_eq!(runtime.next(&binding().lane()).unwrap().id, second.turn.id);
    assert!(runtime.find(&binding(), &second.turn.id).is_none());
    assert!(runtime.find(&second_binding, &first.turn.id).is_none());
}

#[test]
fn selected_model_is_pinned_atomically_but_retries_keep_original_identity() {
    let runtime = NativeTurnRuntime::default();
    let accepted = runtime
        .accept(
            binding(),
            "pinned".into(),
            request("hello"),
            AuthContext::default(),
            true,
            Some("provider/original-model".into()),
        )
        .unwrap();
    assert_eq!(
        runtime
            .next(&binding().lane())
            .unwrap()
            .data
            .lock()
            .unwrap()
            .request
            .model
            .as_deref(),
        Some("provider/original-model")
    );
    let retry = runtime
        .accept(
            binding(),
            "pinned".into(),
            request("hello"),
            AuthContext::default(),
            false,
            Some("provider/new-model".into()),
        )
        .unwrap();
    assert!(Arc::ptr_eq(&accepted.turn, &retry.turn));
    assert_eq!(
        retry.turn.data.lock().unwrap().request.model.as_deref(),
        Some("provider/original-model")
    );
}

#[test]
fn oversized_current_message_keeps_latest_excerpt_and_expired_identity_cannot_rerun() {
    let runtime = NativeTurnRuntime::default();
    let turn = accept(&runtime, "turn-1", "one").turn;
    turn.publish(serde_json::json!({"type":"message_update","message":{"role":"assistant","content":"old prefix"}}));
    turn.publish(serde_json::json!({"type":"message_update","message":{"role":"assistant","content":"x".repeat(MAX_PROJECTION_BYTES+1)}}));
    let snapshot = turn.snapshot(&runtime.epoch, 0, Vec::new());
    assert_eq!(snapshot["projectionTruncated"], true);
    assert_eq!(snapshot["message"]["contentTruncated"], true);
    assert!(
        snapshot["message"]["content"]
            .as_str()
            .unwrap()
            .starts_with("xxx")
    );
    assert!(
        !snapshot["projectionEvents"]
            .to_string()
            .contains("old prefix")
    );
    turn.publish(serde_json::json!({"type":"done"}));
    turn.data.lock().unwrap().finished_at =
        Some(Instant::now() - RETENTION - Duration::from_secs(1));
    accept(&runtime, "turn-prune", "prune");
    assert!(runtime.find(&binding(), "turn-1").is_none());
    assert!(
        runtime
            .accept(
                binding(),
                "turn-1".into(),
                request("one"),
                AuthContext::default(),
                true,
                None
            )
            .is_err()
    );
}

#[test]
fn roster_contains_one_foreground_and_bounded_queue_excerpts() {
    let runtime = NativeTurnRuntime::default();
    let first = accept(&runtime, "turn-1", "first");
    runtime.next(&binding().lane()).unwrap();
    let full_prompt = "a".repeat(64 * 1024);
    for index in 0..MAX_QUEUED {
        accept(&runtime, &format!("queued-{index}"), &full_prompt);
    }
    let roster = runtime.snapshots(&binding());
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["turnId"], first.turn.id);
    assert_eq!(roster[0]["queued"].as_array().unwrap().len(), MAX_QUEUED);
    assert_eq!(roster[0]["queued"][0]["promptTruncated"], true);
    assert_eq!(
        roster[0]["queued"][0]["prompt"].as_str().unwrap().len(),
        1024
    );
    let queued = runtime.find(&binding(), "queued-0").unwrap();
    assert_eq!(
        queued.snapshot(&runtime.epoch, 0, Vec::new())["prompt"],
        full_prompt
    );
    assert!(serde_json::to_vec(&roster).unwrap().len() < 64 * 1024);
}

#[tokio::test]
async fn native_attachment_bytes_use_files_and_never_duplicate_into_prompt_text() {
    use base64::Engine;
    let content = BASE64_STANDARD.encode("original attachment bytes");
    let chat:ChatRequest=serde_json::from_value(serde_json::json!({"messages":[{"role":"user","content":"inspect the file","attachments":[{"fileName":"sample.txt","mimeType":"text/plain","content":content,"extractedText":"unique extracted document text"}]}]})).unwrap();
    let native_prompt = crate::chat::build_native_prompt_from_chat(&chat);
    assert!(!native_prompt.contains(&content));
    assert!(native_prompt.contains("unique extracted document text"));
    assert!(native_prompt.contains("sample.txt"));
    assert!(
        crate::chat::build_prompt_from_chat(&chat).contains(&content),
        "legacy projection contract unchanged"
    );
    assert_eq!(
        chat.messages[0].attachments[0].content.as_deref(),
        Some(content.as_str())
    );
    let workspace = tempfile::tempdir().unwrap();
    let prepared = crate::chat::prepare_chat_attachments(&chat, workspace.path())
        .await
        .unwrap();
    assert_eq!(prepared.paths.len(), 1);
    assert_eq!(
        tokio::fs::read(&prepared.paths[0]).await.unwrap(),
        b"original attachment bytes"
    );
}
