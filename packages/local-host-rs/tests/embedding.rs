use std::{fs, time::Duration};

use maestro_local_host::agent::{FromAgent, NativeAgentConfig, ToolDefinition, ToolResult};
use maestro_local_host::ai::{
    ContentBlock, MessageContent, ProviderStreamErrorKind, ScriptedBlock, ScriptedResponse,
    StopReason, Tool,
};
use maestro_local_host::embedding::{
    EmbeddedAgentRunner, EmbeddedAgentSession, EmbeddedPendingTool, EmbeddedRunError,
    EmbeddedRunEvent, EmbeddedRunProgress, EmbeddedToolResponse,
    test_kit::ScriptedEmbeddingBuilder,
};
use maestro_local_host::state::ApprovalMode;

async fn next_event_matching(
    session: &mut EmbeddedAgentSession,
    predicate: impl Fn(&FromAgent) -> bool,
) -> FromAgent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = session
                .events()
                .recv()
                .await
                .expect("embedding event stream should stay open");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("expected embedding event before timeout")
}

async fn events_through_completion(session: &mut EmbeddedAgentSession) -> Vec<FromAgent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        loop {
            let event = session
                .events()
                .recv()
                .await
                .expect("embedding event stream should stay open");
            let completed = matches!(event, FromAgent::TurnCompleted { .. });
            events.push(event);
            if completed {
                return events;
            }
        }
    })
    .await
    .expect("expected embedding completion before timeout")
}

fn caller_owned_tool() -> ToolDefinition {
    ToolDefinition {
        tool: Tool::new("lookup_status", "Return the current project status").with_schema(
            serde_json::json!({
                "type": "object",
                "additionalProperties": false
            }),
        ),
        requires_approval: true,
    }
}

async fn expect_cancellation_error(runner: &mut EmbeddedAgentRunner) {
    loop {
        match next_runner_event(runner).await {
            Ok(EmbeddedRunEvent::Event(_)) => {}
            Ok(EmbeddedRunEvent::AwaitingTool(_)) => {
                panic!("cancelled runner must not request another tool decision")
            }
            Ok(EmbeddedRunEvent::Completed(completed)) => {
                panic!(
                    "cancelled runner must not complete successfully: {}",
                    completed.output()
                )
            }
            Err(
                EmbeddedRunError::Interrupted { .. }
                | EmbeddedRunError::Runtime { terminal: true, .. },
            ) => return,
            Err(error) => panic!("unexpected cancellation error: {error:?}"),
        }
    }
}

#[test]
fn raw_embedding_api_does_not_expose_result_injection() {
    let source = include_str!("../src/embedding.rs");
    assert!(
        !source.contains("pub fn external_result(call_id"),
        "raw EmbeddedToolResponse must not construct caller results by call ID"
    );

    assert!(
        source.contains("#[cfg(feature = \"runtime-gateway-bridge\")]"),
        "legacy native response channel must require the runtime-gateway bridge feature"
    );
}

#[test]
fn start_runner_rejects_unbounded_turns_from_legacy_config() {
    let config = NativeAgentConfig {
        allow_unbounded_turn: true,
        max_turn_steps: 0,
        ..NativeAgentConfig::default()
    };

    let error = match ScriptedEmbeddingBuilder::from_config(config).start_runner() {
        Ok(_) => panic!("runner must reject raw unbounded-turn configuration"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("at least one turn step"),
        "unexpected error: {error}"
    );
}

fn scripted_tool_use(call_id: &str, tool: &str, args: serde_json::Value) -> ScriptedResponse {
    ScriptedResponse {
        blocks: vec![ScriptedBlock::ToolUse {
            id: call_id.to_owned(),
            name: tool.to_owned(),
            input: args,
        }],
        stop_reason: StopReason::ToolUse,
        error: None,
    }
}

async fn next_runner_event(
    runner: &mut EmbeddedAgentRunner,
) -> Result<EmbeddedRunEvent, EmbeddedRunError> {
    tokio::time::timeout(Duration::from_secs(5), runner.next_event())
        .await
        .expect("expected runner event before timeout")
}

fn pending_tool(progress: EmbeddedRunProgress) -> EmbeddedPendingTool {
    match progress {
        EmbeddedRunProgress::AwaitingTool(pending) => pending,
        EmbeddedRunProgress::Completed(completed) => {
            panic!(
                "expected pending tool, completed with {:?}",
                completed.output()
            )
        }
    }
}

#[tokio::test]
async fn scripted_embedding_raw_session_denies_a_caller_owned_tool_without_local_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let script = vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "lookup-status-1".to_owned(),
                name: "lookup_status".to_owned(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse::text("The tool call was denied."),
    ];
    let mut session = ScriptedEmbeddingBuilder::new(script)
        .working_directory(workspace.path())
        .external_tools([caller_owned_tool()])
        .start()
        .expect("scripted embedding starts");

    session
        .agent()
        .prompt("Check the project status.")
        .await
        .expect("prompt is queued");
    let event = next_event_matching(&mut session, |event| {
        matches!(event, FromAgent::ToolCall { .. })
    })
    .await;
    let FromAgent::ToolCall {
        call_id,
        tool,
        requires_approval,
        ..
    } = event
    else {
        unreachable!("the predicate only returns tool calls");
    };
    assert_eq!(tool, "lookup_status");
    assert!(requires_approval);
    session
        .agent()
        .send_tool_response(EmbeddedToolResponse::deny(call_id.clone()))
        .expect("caller denial reaches the actor");

    let events = events_through_completion(&mut session).await;
    session.shutdown().await;

    assert!(
        !events.iter().any(|event| match event {
            FromAgent::ToolStart {
                call_id: event_call_id,
            }
            | FromAgent::ToolOutput {
                call_id: event_call_id,
                ..
            }
            | FromAgent::ToolEnd {
                call_id: event_call_id,
                success: true,
                ..
            } => event_call_id == &call_id,
            _ => false,
        }),
        "raw external denial must not execute through the local host"
    );
}

#[tokio::test]
async fn scripted_embedding_denial_reaches_the_next_turn_without_local_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let script = vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "lookup-status-denied".to_owned(),
                name: "lookup_status".to_owned(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse::text("The tool call was denied."),
    ];
    let mut session = ScriptedEmbeddingBuilder::new(script)
        .working_directory(workspace.path())
        .external_tools([caller_owned_tool()])
        .start()
        .expect("scripted embedding starts");

    session
        .agent()
        .prompt("Check the project status.")
        .await
        .expect("prompt is queued");
    let event = next_event_matching(&mut session, |event| {
        matches!(event, FromAgent::ToolCall { .. })
    })
    .await;
    let FromAgent::ToolCall { call_id, .. } = event else {
        unreachable!("the predicate only returns tool calls");
    };
    session
        .agent()
        .send_tool_response(EmbeddedToolResponse::deny(call_id.clone()))
        .expect("caller denial reaches the actor");

    let events = events_through_completion(&mut session).await;
    session.shutdown().await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            FromAgent::ResponseChunk { content, is_thinking: false, .. }
                if content == "The tool call was denied."
        )),
        "the scripted second provider round must complete after the denial"
    );
    assert!(
        !events.iter().any(|event| match event {
            FromAgent::ToolStart {
                call_id: event_call_id,
            }
            | FromAgent::ToolOutput {
                call_id: event_call_id,
                ..
            } => event_call_id == &call_id,
            FromAgent::ToolEnd {
                call_id: event_call_id,
                success: true,
                ..
            } => event_call_id == &call_id,
            _ => false,
        }),
        "a denied caller-owned tool must not execute locally or claim success"
    );
    let snapshot = events
        .iter()
        .rev()
        .find_map(|event| match event {
            FromAgent::ConversationSnapshot { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("completed embedding turn must publish a conversation snapshot");
    assert!(
        snapshot.iter().any(|message| {
            matches!(
                &message.content,
                MessageContent::Blocks(blocks)
                    if blocks.iter().any(|block| matches!(
                        block,
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error: Some(true),
                        } if tool_use_id == &call_id && content == "[tool result omitted from checkpoint]"
                    ))
            )
        }),
        "denial must become an error tool result before the next provider turn"
    );
}

#[tokio::test]
async fn shutdown_cancels_a_pending_embedded_turn_before_returning() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut session = ScriptedEmbeddingBuilder::new(vec![ScriptedResponse {
        blocks: vec![ScriptedBlock::Pending],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .working_directory(workspace.path())
    .start()
    .expect("scripted embedding starts");

    session
        .agent()
        .prompt("Wait for cancellation.")
        .await
        .expect("prompt is queued");
    let _ = next_event_matching(&mut session, |event| {
        matches!(event, FromAgent::ResponseStart { .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(3), session.shutdown())
        .await
        .expect("embedded shutdown waits for cancellation cleanup");
}

#[tokio::test]
async fn runner_collects_only_the_final_response_after_external_tool_rounds() {
    let workspace = tempfile::tempdir().expect("workspace");
    let script = vec![
        ScriptedResponse {
            blocks: vec![
                ScriptedBlock::Text("first draft ".to_owned()),
                ScriptedBlock::ToolUse {
                    id: "round-one".to_owned(),
                    name: "lookup_status".to_owned(),
                    input: serde_json::json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse {
            blocks: vec![
                ScriptedBlock::Text("second draft ".to_owned()),
                ScriptedBlock::ToolUse {
                    id: "round-two".to_owned(),
                    name: "lookup_status".to_owned(),
                    input: serde_json::json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse {
            blocks: vec![
                ScriptedBlock::Text("final answer".to_owned()),
                ScriptedBlock::BilledSilence { output_tokens: 7 },
            ],
            stop_reason: StopReason::EndTurn,
            error: None,
        },
    ];
    let mut runner = ScriptedEmbeddingBuilder::new(script)
        .working_directory(workspace.path())
        .external_tools([caller_owned_tool()])
        .start_runner()
        .expect("scripted runner starts");

    let first = pending_tool(
        runner
            .run("Resolve the project status.")
            .await
            .expect("first tool pause"),
    );
    runner
        .external_result(&first, ToolResult::success("first result"))
        .expect("first external result is accepted");
    let second = pending_tool(runner.resume().await.expect("second tool pause"));
    runner
        .external_result(&second, ToolResult::success("second result"))
        .expect("second external result is accepted");
    let completed = match runner.resume().await.expect("terminal completion") {
        EmbeddedRunProgress::Completed(completed) => completed,
        EmbeddedRunProgress::AwaitingTool(pending) => {
            panic!("unexpected third tool call: {}", pending.tool_call().tool())
        }
    };

    assert_eq!(completed.output(), "final answer");
    assert!(!completed.output().contains("first draft"));
    assert!(!completed.output().contains("second draft"));
    assert!(
        !completed.response_id().is_empty(),
        "the collector returns a nonempty final response ID"
    );
    assert_eq!(
        completed
            .final_response_usage()
            .expect("scripted final response reports usage")
            .output_tokens,
        7
    );
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_keeps_response_end_nonterminal_until_the_gated_tool_is_resolved() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        scripted_tool_use("response-end-tool", "lookup_status", serde_json::json!({})),
        ScriptedResponse::text("completed after approval"),
    ])
    .working_directory(workspace.path())
    .external_tools([caller_owned_tool()])
    .start_runner()
    .expect("scripted runner starts");

    runner.start("Use the tool.").await.expect("runner starts");
    let mut saw_response_end = false;
    let pending = loop {
        match next_runner_event(&mut runner).await.expect("runner event") {
            EmbeddedRunEvent::Event(event)
                if matches!(event.as_ref(), FromAgent::ResponseEnd { .. }) =>
            {
                saw_response_end = true;
            }
            EmbeddedRunEvent::AwaitingTool(pending) => break pending,
            EmbeddedRunEvent::Event(_) => {}
            EmbeddedRunEvent::Completed(_) => panic!("ResponseEnd must not complete the turn"),
        }
    };
    assert!(
        saw_response_end,
        "the first provider response must end before approval"
    );
    runner
        .external_result(&pending, ToolResult::success("approved"))
        .expect("external result is accepted");
    let completed = match runner.resume().await.expect("turn resumes") {
        EmbeddedRunProgress::Completed(completed) => completed,
        EmbeddedRunProgress::AwaitingTool(_) => panic!("unexpected second tool call"),
    };
    assert_eq!(completed.output(), "completed after approval");
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_external_result_and_denial_never_dispatch_the_local_host() {
    async fn resolve_external(approved: bool) -> (String, String, Vec<FromAgent>) {
        let workspace = tempfile::tempdir().expect("workspace");
        let response = if approved {
            "external result accepted"
        } else {
            "external result denied"
        };
        let mut runner = ScriptedEmbeddingBuilder::new(vec![
            scripted_tool_use("external-only", "lookup_status", serde_json::json!({})),
            ScriptedResponse::text(response),
        ])
        .working_directory(workspace.path())
        .external_tools([caller_owned_tool()])
        .start_runner()
        .expect("scripted runner starts");

        let pending = pending_tool(runner.run("Use caller tool.").await.expect("tool pause"));
        let call_id = pending.tool_call().call_id().to_owned();
        if approved {
            runner
                .external_result(&pending, ToolResult::success("caller result"))
                .expect("external result is accepted");
        } else {
            runner
                .deny_tool(&pending)
                .expect("external denial is accepted");
        }

        let mut events = Vec::new();
        let completed = loop {
            match next_runner_event(&mut runner).await.expect("runner event") {
                EmbeddedRunEvent::Event(event) => events.push(*event),
                EmbeddedRunEvent::Completed(completed) => break completed,
                EmbeddedRunEvent::AwaitingTool(_) => panic!("unexpected extra tool call"),
            }
        };
        let output = completed.output().to_owned();
        runner.shutdown().await;
        (call_id, output, events)
    }

    for approved in [true, false] {
        let (call_id, output, events) = resolve_external(approved).await;
        assert_eq!(
            output,
            if approved {
                "external result accepted"
            } else {
                "external result denied"
            }
        );
        assert!(
            !events.iter().any(|event| match event {
                FromAgent::ToolStart {
                    call_id: event_call_id,
                }
                | FromAgent::ToolOutput {
                    call_id: event_call_id,
                    ..
                }
                | FromAgent::ToolEnd {
                    call_id: event_call_id,
                    success: true,
                    ..
                } => event_call_id == &call_id,
                _ => false,
            }),
            "caller-owned external tool {call_id} must not execute through the local host"
        );
    }
}

#[tokio::test]
async fn runner_rejects_external_results_for_host_tools_and_revalidates_host_approval() {
    let workspace = tempfile::tempdir().expect("workspace");
    let effect = workspace.path().join("host-approved.txt");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        scripted_tool_use(
            "host-bash",
            "bash",
            serde_json::json!({"command": "printf host-approved > host-approved.txt"}),
        ),
        ScriptedResponse::text("host tool finished"),
    ])
    .working_directory(workspace.path())
    .approval_mode(ApprovalMode::Safe)
    .start_runner()
    .expect("scripted runner starts");

    let pending = pending_tool(
        runner
            .run("Run the local host tool.")
            .await
            .expect("tool pause"),
    );
    assert!(!pending.is_external());
    assert!(matches!(
        runner.external_result(&pending, ToolResult::success("forged external result")),
        Err(EmbeddedRunError::ToolResponse { .. })
    ));
    runner
        .approve_host_tool(&pending)
        .expect("host approval is accepted");

    let mut saw_start = false;
    let mut saw_success = false;
    let completed = loop {
        match next_runner_event(&mut runner).await.expect("runner event") {
            EmbeddedRunEvent::Event(event)
                if matches!(
                    event.as_ref(),
                    FromAgent::ToolStart { call_id }
                        if call_id == pending.tool_call().call_id()
                ) =>
            {
                saw_start = true;
            }
            EmbeddedRunEvent::Event(event) => {
                if let FromAgent::ToolEnd {
                    call_id, success, ..
                } = event.as_ref()
                {
                    if call_id == pending.tool_call().call_id() {
                        saw_success = *success;
                    }
                }
            }
            EmbeddedRunEvent::Completed(completed) => break completed,
            EmbeddedRunEvent::AwaitingTool(_) => panic!("unexpected second tool call"),
        }
    };

    assert!(
        saw_start,
        "the approved host tool must enter local execution"
    );
    assert!(
        saw_success,
        "the approved host tool must report successful execution"
    );
    assert_eq!(
        fs::read_to_string(effect).expect("host tool side effect"),
        "host-approved"
    );
    assert_eq!(completed.output(), "host tool finished");
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_rejects_pending_tools_from_another_runner_even_when_call_ids_match() {
    let workspace_a = tempfile::tempdir().expect("first workspace");
    let workspace_b = tempfile::tempdir().expect("second workspace");
    let script = || {
        vec![
            scripted_tool_use("shared-call-id", "lookup_status", serde_json::json!({})),
            ScriptedResponse::text("completed"),
        ]
    };
    let mut first = ScriptedEmbeddingBuilder::new(script())
        .working_directory(workspace_a.path())
        .external_tools([caller_owned_tool()])
        .start_runner()
        .expect("first scripted runner starts");
    let mut second = ScriptedEmbeddingBuilder::new(script())
        .working_directory(workspace_b.path())
        .external_tools([caller_owned_tool()])
        .start_runner()
        .expect("second scripted runner starts");

    let first_pending = pending_tool(first.run("First run.").await.expect("first pause"));
    let second_pending = pending_tool(second.run("Second run.").await.expect("second pause"));
    assert_eq!(
        first_pending.tool_call().call_id(),
        second_pending.tool_call().call_id(),
        "the model-controlled ID deliberately collides across runners"
    );
    assert!(matches!(
        second.external_result(&first_pending, ToolResult::success("wrong runner")),
        Err(EmbeddedRunError::ToolResponse { .. })
    ));

    first
        .external_result(&first_pending, ToolResult::success("first result"))
        .expect("first pending response is accepted");
    second
        .external_result(&second_pending, ToolResult::success("second result"))
        .expect("second pending response is accepted");
    assert!(matches!(
        first.resume().await.expect("first completes"),
        EmbeddedRunProgress::Completed(_)
    ));
    assert!(matches!(
        second.resume().await.expect("second completes"),
        EmbeddedRunProgress::Completed(_)
    ));
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn runner_rejects_an_earlier_pending_tool_when_a_later_round_reuses_its_call_id() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        scripted_tool_use(
            "reused-call-id",
            "lookup_status",
            serde_json::json!({"round": 1}),
        ),
        scripted_tool_use(
            "reused-call-id",
            "lookup_status",
            serde_json::json!({"round": 2}),
        ),
        ScriptedResponse::text("completed after the second response"),
    ])
    .working_directory(workspace.path())
    .external_tools([caller_owned_tool()])
    .start_runner()
    .expect("scripted runner starts");

    let first_pending = pending_tool(
        runner
            .run("Use the tool twice.")
            .await
            .expect("first pause"),
    );
    runner
        .external_result(&first_pending, ToolResult::success("first result"))
        .expect("first pending response is accepted");
    let second_pending = pending_tool(runner.resume().await.expect("second pause"));
    assert_eq!(
        first_pending.tool_call().call_id(),
        second_pending.tool_call().call_id(),
        "the model deliberately reuses the same call ID in a later round"
    );
    assert!(matches!(
        runner.external_result(&first_pending, ToolResult::success("stale result")),
        Err(EmbeddedRunError::ToolResponse { .. })
    ));
    runner
        .external_result(&second_pending, ToolResult::success("second result"))
        .expect("current pending response is accepted");
    assert!(matches!(
        runner.resume().await.expect("turn completes"),
        EmbeddedRunProgress::Completed(_)
    ));
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_accepts_only_one_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        ScriptedResponse::text("first completion"),
        ScriptedResponse::text("must remain unconsumed"),
    ])
    .working_directory(workspace.path())
    .start_runner()
    .expect("scripted runner starts");

    assert!(matches!(
        runner.run("First prompt.").await.expect("first completion"),
        EmbeddedRunProgress::Completed(_)
    ));
    assert!(matches!(
        runner.run("Second prompt.").await,
        Err(EmbeddedRunError::AlreadyStarted)
    ));
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_keeps_nonterminal_errors_in_progress_and_continues_the_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "truncated-tool".to_owned(),
                name: "lookup_status".to_owned(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::MaxTokens,
            error: None,
        },
        ScriptedResponse::text("continued after nonterminal error"),
    ])
    .working_directory(workspace.path())
    .external_tools([caller_owned_tool()])
    .start_runner()
    .expect("scripted runner starts");

    runner
        .start("Produce a truncated tool call.")
        .await
        .expect("runner starts");
    let mut saw_nonterminal_error = false;
    let completed = loop {
        match next_runner_event(&mut runner).await.expect("runner event") {
            EmbeddedRunEvent::Event(event)
                if matches!(
                    event.as_ref(),
                    FromAgent::Error {
                        fatal: false,
                        terminal: false,
                        ..
                    }
                ) =>
            {
                saw_nonterminal_error = true;
            }
            EmbeddedRunEvent::Event(_) => {}
            EmbeddedRunEvent::AwaitingTool(_) => {
                panic!("truncated tool must not request execution approval")
            }
            EmbeddedRunEvent::Completed(completed) => break completed,
        }
    };

    assert!(
        saw_nonterminal_error,
        "the nonterminal runtime error stays observable"
    );
    assert_eq!(completed.output(), "continued after nonterminal error");
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_returns_a_typed_error_for_a_nonfatal_terminal_runtime_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![ScriptedResponse::stream_error(
        "provider connection closed",
    )])
    .working_directory(workspace.path())
    .start_runner()
    .expect("scripted runner starts");

    assert!(matches!(
        runner.run("Trigger terminal failure.").await,
        Err(EmbeddedRunError::Runtime {
            fatal: false,
            terminal: true,
            ..
        })
    ));
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_retries_provider_eof_and_treats_cancellation_as_error() {
    let eof_workspace = tempfile::tempdir().expect("EOF workspace");
    let mut eof_runner = ScriptedEmbeddingBuilder::new(vec![
        ScriptedResponse {
            blocks: vec![
                ScriptedBlock::Text("partial".to_owned()),
                ScriptedBlock::Eof,
            ],
            stop_reason: StopReason::EndTurn,
            error: None,
        },
        ScriptedResponse::text("completed after retry"),
    ])
    .working_directory(eof_workspace.path())
    .start_runner()
    .expect("EOF runner starts");
    let completed = eof_runner
        .run("Read until EOF.")
        .await
        .expect("the retry completes without the partial response");
    match completed {
        EmbeddedRunProgress::Completed(completed) => {
            assert_eq!(completed.output(), "completed after retry");
        }
        EmbeddedRunProgress::AwaitingTool(_) => panic!("unexpected tool call after provider retry"),
    }
    eof_runner.shutdown().await;

    let provider_workspace = tempfile::tempdir().expect("provider workspace");
    let mut provider_runner = ScriptedEmbeddingBuilder::new(vec![ScriptedResponse {
        blocks: vec![ScriptedBlock::ProviderError {
            kind: ProviderStreamErrorKind::ProviderDeclaredFailure,
            message: "declared provider failure".to_owned(),
        }],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .working_directory(provider_workspace.path())
    .start_runner()
    .expect("provider runner starts");
    assert!(matches!(
        provider_runner.run("Trigger provider failure.").await,
        Err(EmbeddedRunError::Provider { kind, .. })
            if kind == format!("{:?}", ProviderStreamErrorKind::ProviderDeclaredFailure)
    ));
    provider_runner.shutdown().await;

    let cancel_workspace = tempfile::tempdir().expect("cancel workspace");
    let mut cancel_runner = ScriptedEmbeddingBuilder::new(vec![ScriptedResponse {
        blocks: vec![ScriptedBlock::Pending],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .working_directory(cancel_workspace.path())
    .start_runner()
    .expect("cancel runner starts");
    cancel_runner
        .start("Wait for cancellation.")
        .await
        .expect("runner starts");
    loop {
        match next_runner_event(&mut cancel_runner)
            .await
            .expect("runner event")
        {
            EmbeddedRunEvent::Event(event)
                if matches!(event.as_ref(), FromAgent::ResponseStart { .. }) =>
            {
                break;
            }
            EmbeddedRunEvent::Event(_) => {}
            EmbeddedRunEvent::AwaitingTool(_) | EmbeddedRunEvent::Completed(_) => {
                panic!("pending stream must not pause or complete before cancellation")
            }
        }
    }
    cancel_runner.cancel().expect("cancellation is requested");
    expect_cancellation_error(&mut cancel_runner).await;
    cancel_runner.shutdown().await;
}

#[tokio::test]
async fn runner_rejects_a_pending_external_result_after_cancellation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut runner = ScriptedEmbeddingBuilder::new(vec![
        scripted_tool_use("cancelled-tool", "lookup_status", serde_json::json!({})),
        ScriptedResponse::text("must not run after cancellation"),
    ])
    .working_directory(workspace.path())
    .external_tools([caller_owned_tool()])
    .start_runner()
    .expect("scripted runner starts");

    let pending = pending_tool(
        runner
            .run("Wait for a tool response.")
            .await
            .expect("tool pause"),
    );
    runner.cancel().expect("cancellation is requested");
    assert!(matches!(
        runner.external_result(&pending, ToolResult::success("stale result")),
        Err(EmbeddedRunError::ToolResponse { .. })
    ));
    expect_cancellation_error(&mut runner).await;
    runner.shutdown().await;
}

#[tokio::test]
async fn runner_drop_cancels_a_pending_tool_and_reaches_native_cleanup() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut runner, cleanup) = ScriptedEmbeddingBuilder::new(vec![
        scripted_tool_use("dropped-tool", "lookup_status", serde_json::json!({})),
        ScriptedResponse::text("must not run after drop"),
    ])
    .working_directory(workspace.path())
    .external_tools([caller_owned_tool()])
    .start_runner_with_drop_observer()
    .expect("scripted runner starts");

    let pending = pending_tool(
        runner
            .run("Wait for a tool response.")
            .await
            .expect("tool pause"),
    );
    assert_eq!(pending.tool_call().call_id(), "dropped-tool");
    drop(runner);
    tokio::time::timeout(Duration::from_secs(3), cleanup)
        .await
        .expect("runner drop schedules native shutdown")
        .expect("native shutdown barrier completes after pending-tool drop");
}
