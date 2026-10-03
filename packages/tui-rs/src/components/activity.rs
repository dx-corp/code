//! Presentation of recorded activity; never classify outcomes from transcript prose.

use super::dex_companion::DexCompanionState;
use crate::state::{AppState, Message, ToolCallStatus};

/// Derive presentation from live app-owned signals and the last explicit terminal event.
#[must_use]
pub fn dex_state(
    busy: bool,
    pending_approvals: bool,
    external_review: bool,
    failed: bool,
    terminal: Option<DexCompanionState>,
) -> DexCompanionState {
    if pending_approvals {
        DexCompanionState::NeedsInput
    } else if busy && external_review {
        DexCompanionState::Waiting
    } else if busy {
        DexCompanionState::Working
    } else if let Some(terminal) = terminal {
        terminal
    } else if failed {
        DexCompanionState::Failed
    } else {
        DexCompanionState::Ready
    }
}

/// Name the most recent active tool using its runtime status, not model narration.
#[must_use]
pub fn active_tool_label(state: &AppState) -> Option<String> {
    if !state.busy {
        return None;
    }
    let all_calls = state
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .collect::<Vec<_>>();
    for (parent, children) in &state.codemode_progress {
        if all_calls
            .iter()
            .any(|call| call.call_id == *parent && call.status == ToolCallStatus::Running)
        {
            let approvals = children
                .iter()
                .filter(|child| {
                    all_calls.iter().any(|call| {
                        call.call_id == child.call_id && call.status == ToolCallStatus::Pending
                    })
                })
                .count();
            if approvals > 0 {
                return Some(format!("Script · {approvals} awaiting approval"));
            }
            return Some(
                maestro_local_host::agent::protocol::CodeModeChildProgress::summary(children),
            );
        }
    }
    let mut calls = state
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.tool_calls.iter().rev());
    calls
        .clone()
        .find(|call| call.status == ToolCallStatus::Running)
        .map(|call| {
            maestro_ui::localization::format("Running {0}", std::slice::from_ref(&(call.tool)))
        })
        .or_else(|| {
            calls
                .find(|call| call.status == ToolCallStatus::Pending)
                .map(|call| {
                    maestro_ui::localization::format(
                        "Pending {0}",
                        std::slice::from_ref(&(call.tool)),
                    )
                })
        })
}

/// Summarize recorded tool outcomes. A completed command is not proof of a passed test.
#[must_use]
pub fn evidence_summary(messages: &[Message], changed_files: Option<usize>) -> String {
    let turn_start = messages
        .iter()
        .rposition(|message| message.role == crate::state::MessageRole::User)
        .unwrap_or(0);
    let attention = messages[turn_start..]
        .iter()
        .flat_map(|message| &message.tool_calls)
        .filter(|call| {
            matches!(
                call.status,
                ToolCallStatus::Failed | ToolCallStatus::Blocked | ToolCallStatus::Cancelled
            )
        })
        .count();
    let changes = changed_files.map_or_else(
        || maestro_ui::localization::tr("Changes: /diff").to_owned(),
        |count| {
            maestro_ui::localization::format(
                "{0} recorded file changes · /diff",
                &[(count).to_string()],
            )
        },
    );
    maestro_ui::localization::format(
        "Latest recorded checkpoint: {0}\nMost recent turn: tests not recorded · Tools needing attention: {1}",
        &[(changes).clone(), (attention).to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ToolCallState;

    #[test]
    fn lifecycle_prefers_current_work_and_explicit_events_over_old_errors() {
        use DexCompanionState::*;
        assert_eq!(dex_state(false, false, false, false, None), Ready);
        assert_eq!(dex_state(true, false, false, true, Some(Failed)), Working);
        assert_eq!(dex_state(true, true, true, false, None), NeedsInput);
        assert_eq!(dex_state(true, false, true, false, None), Waiting);
        assert_eq!(
            dex_state(false, false, true, false, Some(Finished)),
            Finished
        );
        assert_eq!(
            dex_state(false, false, false, true, Some(Finished)),
            Finished
        );
        assert_eq!(dex_state(false, false, false, false, Some(Failed)), Failed);
        assert_eq!(dex_state(false, false, false, true, None), Failed);
    }

    #[test]
    fn activity_uses_tool_status_and_disappears_when_idle() {
        let mut state = AppState::new();
        state.add_user_message("Check the code".into());
        state.messages[0].tool_calls.push(ToolCallState {
            parent_call_id: None,
            duration_ms: None,
            call_id: "call-1".into(),
            tool: "read".into(),
            args: serde_json::json!({}),
            status: ToolCallStatus::Running,
            output: String::new(),
        });
        state.busy = true;
        assert_eq!(active_tool_label(&state).as_deref(), Some("Running read"));
        state.messages[0].tool_calls[0].status = ToolCallStatus::Completed;
        assert!(active_tool_label(&state).is_none());
        state.messages[0].tool_calls[0].status = ToolCallStatus::Pending;
        assert_eq!(active_tool_label(&state).as_deref(), Some("Pending read"));
        state.busy = false;
        assert!(active_tool_label(&state).is_none());
    }

    #[test]
    fn evidence_does_not_treat_success_prose_as_test_results() {
        let mut state = AppState::new();
        state.add_user_message("All tests passed".into());
        state.messages[0].tool_calls.push(ToolCallState {
            parent_call_id: None,
            duration_ms: None,
            call_id: "call-1".into(),
            tool: "bash".into(),
            args: serde_json::json!({}),
            status: ToolCallStatus::Completed,
            output: "100 tests passed".into(),
        });
        let summary = evidence_summary(&state.messages, Some(2));
        assert!(summary.contains("2 recorded file changes"));
        assert!(summary.contains("tests not recorded"));
        assert!(summary.contains("Tools needing attention: 0"));
        state.messages[0].tool_calls[0].status = ToolCallStatus::Blocked;
        assert!(evidence_summary(&state.messages, None).contains("Tools needing attention: 1"));
    }
}
