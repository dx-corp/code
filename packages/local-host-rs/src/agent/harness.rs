//! Test harness for driving [`NativeAgent`] with a scripted model and
//! observing hook wiring.
//!
//! Production code cannot construct a fully instrumented runner with injected
//! hooks without going through config files. This module writes a minimal
//! `.composer/hooks.toml` into a temporary workspace so session and
//! recovery-path dispatch becomes assertable.

#![cfg(test)]

use super::protocol::FromAgent;
use super::{NativeAgent, NativeAgentConfig};
use crate::ai::{
    ProviderStreamErrorKind, ScriptedBlock, ScriptedClient, ScriptedResponse, StopReason,
    UnifiedClient,
};
use crate::hooks::HookEventType;
use crate::state::ApprovalMode;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Workspace + agent pair for wiring tests.
pub struct AgentHarness {
    pub workspace: TempDir,
    pub agent: NativeAgent,
    pub events: mpsc::UnboundedReceiver<FromAgent>,
    hook_log: PathBuf,
}

/// Upper bound for an event a test expects to arrive. `wait_for_event` returns
/// as soon as the event is observed, so this only bounds a failing test. The
/// previous 2-12 s limits expired on a loaded host during the full
/// maestro-local-host suite (9 of 3232 tests on dev-desktop, 2026-09-25) while
/// the same tests passed when run alone.
const EVENT_WAIT: Duration = Duration::from_secs(30);

impl AgentHarness {
    /// Create a harness with a scripted client and hook logging enabled.
    pub fn with_scripted(responses: Vec<ScriptedResponse>) -> anyhow::Result<Self> {
        let workspace = TempDir::new()?;
        let hook_log = workspace.path().join("hook-events.log");

        let config = NativeAgentConfig {
            model: "scripted-replay/maestro-replay-v1".to_owned(),
            cwd: workspace.path().display().to_string(),
            approval_mode: ApprovalMode::Yolo,
            ..NativeAgentConfig::default()
        };
        let client = UnifiedClient::Scripted(ScriptedClient::new(
            "scripted-replay/maestro-replay-v1",
            responses,
        ));
        let (agent, events) = NativeAgent::new_with_test_client(config, client)?;
        // Point the runner's hook logger at our file without requiring the
        // temp workspace to be marked trusted in global config.
        agent.set_hook_log_file(hook_log.display().to_string())?;
        Ok(Self {
            workspace,
            agent,
            events,
            hook_log,
        })
    }

    /// Drain events until `predicate` matches or the timeout elapses.
    pub async fn wait_for_event(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&FromAgent) -> bool,
    ) -> Option<FromAgent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.events.recv()).await {
                Ok(Some(event)) => {
                    if predicate(&event) {
                        return Some(event);
                    }
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }

    /// Lines written by the recording hook logger (event name prefixes).
    pub fn hook_log_lines(&self) -> Vec<String> {
        fs::read_to_string(&self.hook_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .filter(|line| !line.trim().is_empty())
            .collect()
    }

    pub fn hook_log_contains(&self, needle: &str) -> bool {
        self.hook_log_lines()
            .iter()
            .any(|line| line.contains(needle))
    }
}

/// Production hook events that must have a dispatch site in the local host or terminal application.
///
/// Kept in sync with scripts/check-hook-dispatch-coverage.mjs. Events that are
/// registered for external config but intentionally not yet wired stay listed
/// in [`UNWIRED_HOOK_EVENTS`].
pub const WIRED_HOOK_EVENTS: &[HookEventType] = &[
    HookEventType::PreToolUse,
    HookEventType::PostToolUse,
    // Shares `execute_post_tool_use` with PostToolUse; selected when `is_error`
    // is true and logged as hookEventName=PostToolUseFailure.
    HookEventType::PostToolUseFailure,
    HookEventType::SessionStart,
    HookEventType::SessionEnd,
    HookEventType::UserPromptSubmit,
    HookEventType::Overflow,
    HookEventType::StopFailure,
    HookEventType::PreMessage,
    HookEventType::PostMessage,
    HookEventType::OnError,
    HookEventType::EvalGate,
    HookEventType::SubagentStart,
    HookEventType::SubagentStop,
    HookEventType::PermissionRequest,
];

/// Events that may be registered in config but are not required to have a
/// production call site yet (tracked separately).
pub const UNWIRED_HOOK_EVENTS: &[HookEventType] = &[
    HookEventType::SessionSwitch,
    HookEventType::SessionBeforeTree,
    HookEventType::SessionTree,
    HookEventType::PreCompact,
    HookEventType::PostCompact,
    HookEventType::Notification,
    HookEventType::Branch,
];

/// Names used by the dispatch coverage script / docs.
pub fn wired_hook_event_names() -> HashSet<&'static str> {
    WIRED_HOOK_EVENTS
        .iter()
        .map(|event| match event {
            HookEventType::PreToolUse => "PreToolUse",
            HookEventType::PostToolUse => "PostToolUse",
            HookEventType::PostToolUseFailure => "PostToolUseFailure",
            HookEventType::SessionStart => "SessionStart",
            HookEventType::SessionEnd => "SessionEnd",
            HookEventType::SessionSwitch => "SessionSwitch",
            HookEventType::SessionBeforeTree => "SessionBeforeTree",
            HookEventType::SessionTree => "SessionTree",
            HookEventType::UserPromptSubmit => "UserPromptSubmit",
            HookEventType::PreCompact => "PreCompact",
            HookEventType::PostCompact => "PostCompact",
            HookEventType::Notification => "Notification",
            HookEventType::Overflow => "Overflow",
            HookEventType::StopFailure => "StopFailure",
            HookEventType::PreMessage => "PreMessage",
            HookEventType::PostMessage => "PostMessage",
            HookEventType::OnError => "OnError",
            HookEventType::EvalGate => "EvalGate",
            HookEventType::SubagentStart => "SubagentStart",
            HookEventType::SubagentStop => "SubagentStop",
            HookEventType::PermissionRequest => "PermissionRequest",
            HookEventType::Branch => "Branch",
        })
        .collect()
}

#[tokio::test]
async fn session_context_transition_logs_session_start_and_end() {
    let harness = AgentHarness::with_scripted(vec![ScriptedResponse::text("hi")])
        .expect("harness should construct");

    harness
        .agent
        .set_session_context(Some("sess-a".to_owned()), "new", false)
        .expect("set session a");
    // Give the runner a moment to process the command and write the hook log.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        harness.hook_log_contains("SessionStart"),
        "expected SessionStart in {:?}",
        harness.hook_log_lines()
    );

    harness
        .agent
        .set_session_context(Some("sess-b".to_owned()), "resume", false)
        .expect("set session b");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let lines = harness.hook_log_lines();
    assert!(
        lines.iter().any(|line| line.contains("SessionEnd")),
        "expected SessionEnd when leaving sess-a: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .filter(|line| line.contains("SessionStart"))
            .count()
            >= 2,
        "expected a second SessionStart for sess-b: {lines:?}"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_stream_error_dispatches_stop_failure() {
    let mut harness =
        AgentHarness::with_scripted(vec![ScriptedResponse::stream_error("provider went away")])
            .expect("harness should construct");

    // Ensure the log path is installed before the turn that should fire StopFailure.
    tokio::time::sleep(Duration::from_millis(50)).await;

    harness
        .agent
        .set_session_context(Some("sess-stop".to_owned()), "new", false)
        .expect("session");
    tokio::time::sleep(Duration::from_millis(50)).await;

    harness
        .agent
        .prompt("trigger error".to_owned(), vec![])
        .await
        .expect("prompt");

    let saw_error = harness
        .wait_for_event(EVENT_WAIT, |event| {
            matches!(event, FromAgent::Error { terminal: true, .. })
        })
        .await;
    assert!(
        saw_error.is_some(),
        "expected a terminal stream Error event; log={:?}",
        harness.hook_log_lines()
    );
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| {
                matches!(event, FromAgent::ResponseEnd { .. })
            })
            .await
            .is_none(),
        "terminal stream errors must not be followed by ResponseEnd"
    );

    // Poll specifically for StopFailure — PostMessage may also log on the same
    // path, but this regression is only about the recovery-failure hook.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if harness.hook_log_contains("StopFailure") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        harness.hook_log_contains("StopFailure"),
        "expected StopFailure in hook log, got {:?}",
        harness.hook_log_lines()
    );
    assert!(
        harness.hook_log_contains("OnError"),
        "expected OnError in hook log, got {:?}",
        harness.hook_log_lines()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_provider_error_preserves_kind_and_never_completes_turn() {
    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse {
        blocks: vec![ScriptedBlock::ProviderError {
            kind: ProviderStreamErrorKind::OutputTokenExhaustion,
            message: "output budget exhausted".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .expect("harness should construct");

    harness
        .agent
        .prompt("exhaust the output budget".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(matches!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::ProviderError { .. }
            ))
            .await,
        Some(FromAgent::ProviderError {
            kind: ProviderStreamErrorKind::OutputTokenExhaustion,
            message,
        }) if message.contains("output budget exhausted")
    ));
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_none()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_partial_text_eof_is_transient_protocol_error() {
    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse {
        blocks: vec![
            ScriptedBlock::Text("partial answer".to_string()),
            ScriptedBlock::Eof,
        ],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .expect("harness should construct");

    harness
        .agent
        .prompt("cut the stream".to_owned(), vec![])
        .await
        .expect("prompt");

    let snapshot = harness
        .wait_for_event(EVENT_WAIT, |event| {
            matches!(event, FromAgent::ConversationSnapshot { .. })
        })
        .await
        .expect("terminal provider failure should publish a snapshot");
    let FromAgent::ConversationSnapshot { messages, .. } = snapshot else {
        unreachable!();
    };
    assert!(
        messages
            .iter()
            .all(|message| message.role != crate::ai::Role::Assistant),
        "partial provider text must not become authoritative assistant history: {messages:?}"
    );
    assert!(matches!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::ProviderError { .. }
            ))
            .await,
        Some(FromAgent::ProviderError {
            kind: ProviderStreamErrorKind::TransientProtocol,
            ..
        })
    ));
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_none()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_completed_tool_block_eof_is_transient_protocol_error() {
    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse {
        blocks: vec![
            ScriptedBlock::ToolUse {
                id: "call-cut".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({ "path": "Cargo.toml" }),
            },
            ScriptedBlock::Eof,
        ],
        stop_reason: StopReason::ToolUse,
        error: None,
    }])
    .expect("harness should construct");

    harness
        .agent
        .prompt("cut after a tool block".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(matches!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::ProviderError { .. }
            ))
            .await,
        Some(FromAgent::ProviderError {
            kind: ProviderStreamErrorKind::TransientProtocol,
            ..
        })
    ));
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_none()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn successful_native_loop_emits_explicit_turn_completed() {
    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse::text("complete")])
        .expect("harness should construct");

    harness
        .agent
        .prompt("complete the turn".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_some()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn continue_preserves_provider_error_kind_and_does_not_complete() {
    let mut harness = AgentHarness::with_scripted(vec![
        ScriptedResponse::text("first turn"),
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ProviderError {
                kind: ProviderStreamErrorKind::ProviderDeclaredFailure,
                message: "declared continue failure".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            error: None,
        },
    ])
    .expect("harness should construct");

    harness
        .agent
        .prompt("first prompt".to_owned(), vec![])
        .await
        .expect("prompt");
    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_some()
    );

    harness.agent.continue_execution().expect("continue");
    assert!(matches!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::ProviderError { .. }
            ))
            .await,
        Some(FromAgent::ProviderError {
            kind: ProviderStreamErrorKind::ProviderDeclaredFailure,
            message,
        }) if message == "declared continue failure"
    ));
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_none()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn cancelled_native_loop_emits_explicit_turn_interrupted() {
    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse {
        blocks: vec![ScriptedBlock::Pending],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .expect("harness should construct");

    harness
        .agent
        .prompt("wait for cancellation".to_owned(), vec![])
        .await
        .expect("prompt");
    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::ResponseStart { .. }
            ))
            .await
            .is_some()
    );
    harness.agent.cancel();

    assert!(matches!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::TurnInterrupted { .. }
            ))
            .await,
        Some(FromAgent::TurnInterrupted { reason, .. }) if reason == "cancelled"
    ));
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| matches!(
                event,
                FromAgent::TurnCompleted { .. }
            ))
            .await
            .is_none()
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn side_question_eof_reports_structured_transient_protocol_error() {
    use super::message_queue::PromptKind;

    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse {
        blocks: vec![
            ScriptedBlock::Text("partial side answer".to_string()),
            ScriptedBlock::Eof,
        ],
        stop_reason: StopReason::EndTurn,
        error: None,
    }])
    .expect("harness should construct");

    harness
        .agent
        .prompt_with_kind(
            "side question".to_owned(),
            vec![],
            PromptKind::SideQuestion,
            None,
        )
        .await
        .expect("side question");

    assert!(matches!(
        harness
            .wait_for_event(EVENT_WAIT, |event| matches!(
                event,
                FromAgent::SideQuestionEnd { .. }
            ))
            .await,
        Some(FromAgent::SideQuestionEnd {
            provider_error_kind: Some(ProviderStreamErrorKind::TransientProtocol),
            ..
        })
    ));

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_empty_response_is_terminal_error_without_response_end() {
    let empty = ScriptedResponse {
        blocks: Vec::new(),
        stop_reason: StopReason::EndTurn,
        error: None,
    };
    let mut harness =
        AgentHarness::with_scripted(vec![empty.clone(), empty.clone(), empty.clone(), empty])
            .expect("harness should construct");

    harness
        .agent
        .prompt("return nothing".to_owned(), vec![])
        .await
        .expect("prompt");

    let error = harness
        .wait_for_event(EVENT_WAIT, |event| {
            matches!(event, FromAgent::Error { terminal: true, .. })
        })
        .await
        .expect("empty response should produce terminal error");
    let FromAgent::Error { message, .. } = error else {
        unreachable!();
    };
    assert!(message.contains("empty_assistant_response"), "{message}");
    assert!(
        harness
            .wait_for_event(Duration::from_millis(200), |event| {
                matches!(event, FromAgent::ResponseEnd { .. })
            })
            .await
            .is_none(),
        "empty response must not be followed by ResponseEnd"
    );
    assert_eq!(
        harness
            .hook_log_lines()
            .iter()
            .filter(|line| line.contains("StopFailure"))
            .count(),
        1,
        "retry exhaustion should dispatch StopFailure exactly once"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_empty_response_retries_before_terminal_success() {
    let mut harness = AgentHarness::with_scripted(vec![
        ScriptedResponse {
            blocks: Vec::new(),
            stop_reason: StopReason::EndTurn,
            error: None,
        },
        ScriptedResponse::text("recovered"),
    ])
    .expect("harness should construct");

    harness
        .agent
        .prompt("recover from an empty provider stream".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(event, FromAgent::Status { message } if message.contains("Retrying"))
            })
            .await
            .is_some(),
        "empty response should enter bounded retry"
    );
    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(event, FromAgent::ResponseEnd { response_id, .. } if response_id == "done")
            })
            .await
            .is_some(),
        "a later provider response should complete the original turn"
    );
    assert!(
        !harness.hook_log_contains("StopFailure"),
        "recovered empty response must not dispatch StopFailure"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_billed_empty_completion_steers_once_then_completes() {
    let mut harness = AgentHarness::with_scripted(vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::BilledSilence { output_tokens: 390 }],
            stop_reason: StopReason::EndTurn,
            error: None,
        },
        ScriptedResponse::text("{\"escalate_to\":\"none\",\"findings\":[]}"),
    ])
    .expect("harness should construct");

    harness
        .agent
        .prompt("review this change".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(
                    event,
                    FromAgent::Status { message }
                        if message.contains("billed tokens with no assistant text")
                )
            })
            .await
            .is_some(),
        "billed empty completion should steer once instead of retrying identically"
    );
    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(event, FromAgent::ResponseEnd { response_id, .. } if response_id == "done")
            })
            .await
            .is_some(),
        "the steered follow-up should complete the original turn"
    );
    assert!(
        !harness.hook_log_contains("StopFailure"),
        "recovered billed-empty completion must not dispatch StopFailure"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_tool_side_effect_is_not_repeated_by_later_empty_retry() {
    let mut harness = AgentHarness::with_scripted(vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "call-side-effect".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "printf x >> side-effect.log"}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse {
            blocks: Vec::new(),
            stop_reason: StopReason::EndTurn,
            error: None,
        },
        ScriptedResponse::text("recovered after tool"),
    ])
    .expect("harness should construct");

    harness
        .agent
        .prompt("perform one side effect".to_owned(), vec![])
        .await
        .expect("prompt");
    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(event, FromAgent::ResponseEnd { response_id, .. } if response_id == "done")
            })
            .await
            .is_some(),
        "turn should recover after the empty provider attempt"
    );
    assert_eq!(
        fs::read_to_string(harness.workspace.path().join("side-effect.log"))
            .expect("side effect file"),
        "x",
        "retry must not execute a completed tool call twice"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_text_response_still_completes() {
    let mut harness = AgentHarness::with_scripted(vec![ScriptedResponse::text("hello")])
        .expect("harness should construct");

    harness
        .agent
        .prompt("say hello".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(event, FromAgent::ResponseEnd { response_id, .. } if response_id == "done")
            })
            .await
            .is_some(),
        "text response should complete"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn scripted_tool_only_turn_is_not_rejected_as_empty() {
    let mut harness = AgentHarness::with_scripted(vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "call-glob".to_owned(),
                name: "glob".to_owned(),
                input: json!({"pattern": "*"}),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse::text("tool completed"),
    ])
    .expect("harness should construct");

    harness
        .agent
        .prompt("list files".to_owned(), vec![])
        .await
        .expect("prompt");

    assert!(
        harness
            .wait_for_event(EVENT_WAIT, |event| {
                matches!(event, FromAgent::ResponseEnd { response_id, .. } if response_id == "done")
            })
            .await
            .is_some(),
        "tool-only turn should continue to the final response"
    );

    harness.agent.shutdown().await;
}

#[tokio::test]
async fn failed_tool_dispatches_post_tool_use_failure() {
    // A tool call that exits non-zero must log PostToolUseFailure (not only
    // PostToolUse). The two events share execute_post_tool_use; this asserts
    // the is_error branch is still selected in production.
    let harness = AgentHarness::with_scripted(vec![
        ScriptedResponse {
            blocks: vec![ScriptedBlock::ToolUse {
                id: "call-fail".to_owned(),
                name: "bash".to_owned(),
                input: json!({ "command": "exit 1" }),
            }],
            stop_reason: StopReason::ToolUse,
            error: None,
        },
        ScriptedResponse::text("acknowledged failure"),
    ])
    .expect("harness should construct");

    tokio::time::sleep(Duration::from_millis(50)).await;
    harness
        .agent
        .set_session_context(Some("sess-fail".to_owned()), "new", false)
        .expect("session");
    tokio::time::sleep(Duration::from_millis(50)).await;

    harness
        .agent
        .prompt("run a failing command".to_owned(), vec![])
        .await
        .expect("prompt");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if harness.hook_log_contains("PostToolUseFailure") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        harness.hook_log_contains("PostToolUseFailure"),
        "expected PostToolUseFailure after a failed tool; log={:?}",
        harness.hook_log_lines()
    );

    harness.agent.shutdown().await;
}

#[test]
fn wired_and_unwired_hook_events_partition_the_enum() {
    use std::mem;

    let wired: HashSet<_> = WIRED_HOOK_EVENTS.iter().copied().collect();
    let unwired: HashSet<_> = UNWIRED_HOOK_EVENTS.iter().copied().collect();
    assert!(wired.is_disjoint(&unwired));
    // Exhaustiveness: every HookEventType appears in exactly one set.
    // Adding a variant to HookEventType will fail this until it is classified.
    let total = wired.len() + unwired.len();
    assert_eq!(
        total, 22,
        "update WIRED_HOOK_EVENTS / UNWIRED_HOOK_EVENTS when HookEventType changes (count={total})"
    );
    let _ = mem::size_of::<HookEventType>();
}

#[tokio::test]
async fn bash_capture_survives_native_compaction_and_continuation_replay() {
    let workspace = TempDir::new().expect("workspace");
    let evidence = format!(
        "{}\nUNIQUE_CAPTURE_MIDDLE_EVIDENCE\n{}",
        "a".repeat(40_000),
        "z".repeat(40_000)
    );
    fs::write(workspace.path().join("evidence.txt"), &evidence).expect("evidence fixture");
    let client = UnifiedClient::Scripted(ScriptedClient::new(
        "scripted-replay/maestro-replay-v1",
        vec![
            ScriptedResponse {
                blocks: vec![ScriptedBlock::ToolUse {
                    id: "capture-bash".into(),
                    name: "bash".into(),
                    input: json!({"command": "cat evidence.txt"}),
                }],
                stop_reason: StopReason::ToolUse,
                error: None,
            },
            ScriptedResponse::text("Finished reading."),
            ScriptedResponse::text("Continued."),
        ],
    ));
    let config = NativeAgentConfig {
        model: "scripted-replay/maestro-replay-v1".into(),
        cwd: workspace.path().display().to_string(),
        approval_mode: ApprovalMode::Yolo,
        context_window: Some(4_096),
        ..NativeAgentConfig::default()
    };
    let (agent, mut events) = NativeAgent::new_with_test_client(config, client).expect("agent");
    agent
        .prompt("Read the evidence file.".into(), vec![])
        .await
        .expect("prompt");
    let continuation = tokio::time::timeout(Duration::from_secs(15), async {
        let mut continued = false;
        loop {
            match events.recv().await {
                Some(FromAgent::TurnCompleted { .. }) if !continued => {
                    continued = true;
                    agent
                        .prompt(
                            "Continue with the captured evidence. ".repeat(1_000),
                            vec![],
                        )
                        .await
                        .expect("continuation prompt");
                }
                Some(FromAgent::Compaction {
                    continuation: Some(record),
                    ..
                }) => break record,
                Some(FromAgent::Error { message, .. }) => panic!("agent error: {message}"),
                Some(_) => {}
                None => panic!("agent closed before compaction"),
            }
        }
    })
    .await
    .expect("compaction timeout");
    agent.shutdown().await;
    let serialized = serde_json::to_vec(&continuation).expect("serialize continuation");
    let replay: maestro_runtime::agent::compaction::ContinuationRecord =
        serde_json::from_slice(&serialized).expect("replay continuation");
    let output = replay
        .tool_outputs
        .iter()
        .find(|output| output.tool_call_id == "capture-bash")
        .expect("native Bash capture must be retained without an outer spill");
    assert_eq!(
        fs::read_to_string(&output.path).expect("read original capture"),
        evidence
    );
}
