//! Runtime-derived presentation facts; cosmetics live in the shared product widgets.
use crate::components::dex_companion::DexCompanionState;
use crate::state::{AppState, MessageRole, ToolCallStatus};
pub use maestro_presentation::dex_delight::*;

pub fn observed_activity(state: &AppState) -> DexActivity {
    if !state.busy {
        return DexActivity::Thinking;
    }
    state
        .messages
        .iter()
        .rev()
        .flat_map(|m| m.tool_calls.iter().rev())
        .find(|call| call.status == ToolCallStatus::Running)
        .map_or(DexActivity::Thinking, |call| {
            DexActivity::from_tool(&call.tool)
        })
}

/// Suggest an explicit next prompt from the current turn's recorded tool outcomes.
/// A shell exit code is deliberately never interpreted as tests passing.
pub fn next_prompt(state: &AppState, terminal: Option<DexCompanionState>) -> Option<&'static str> {
    if state.busy || terminal != Some(DexCompanionState::Finished) {
        return None;
    }
    let start = state
        .messages
        .iter()
        .rposition(|m| m.role == MessageRole::User)?;
    let calls = || state.messages[start..].iter().flat_map(|m| &m.tool_calls);
    if calls().any(|c| matches!(c.status, ToolCallStatus::Failed | ToolCallStatus::Blocked)) {
        Some("Explain the tool failures and propose a fix.")
    } else if calls().any(|c| {
        c.status == ToolCallStatus::Completed
            && DexActivity::from_tool(&c.tool) == DexActivity::Editing
    }) {
        Some("/diff")
    } else if calls().any(|c| {
        c.status == ToolCallStatus::Completed
            && matches!(
                DexActivity::from_tool(&c.tool),
                DexActivity::Reading | DexActivity::Searching
            )
    }) {
        Some("Summarize what you found and recommend the next step.")
    } else {
        None
    }
}

/// Short factual recap of the latest observed turn, with no invented test/PR status.
pub fn recap(state: &AppState, terminal: Option<DexCompanionState>) -> String {
    let start = state
        .messages
        .iter()
        .rposition(|m| m.role == MessageRole::User)
        .unwrap_or(state.messages.len());
    let calls = || state.messages[start..].iter().flat_map(|m| &m.tool_calls);
    let completed = calls()
        .filter(|c| c.status == ToolCallStatus::Completed)
        .count();
    let attention = calls()
        .filter(|c| {
            matches!(
                c.status,
                ToolCallStatus::Failed | ToolCallStatus::Blocked | ToolCallStatus::Cancelled
            )
        })
        .count();
    let status = if terminal == Some(DexCompanionState::NeedsInput) {
        "Your answer is needed"
    } else if terminal == Some(DexCompanionState::Failed) {
        "Last request failed"
    } else if state.busy {
        "Still working"
    } else {
        match terminal {
            Some(DexCompanionState::Finished) => "Last request completed",
            Some(DexCompanionState::Failed) => "Last request failed",
            Some(DexCompanionState::NeedsInput) => "Your answer is needed",
            Some(DexCompanionState::Waiting) => "Waiting for a prerequisite",
            _ => "Ready for your next request",
        }
    };
    format!("{status} · {completed} tools completed · {attention} need attention")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_does_not_claim_outcomes_and_pet_is_bounded() {
        let look = DexLook {
            pet_frame: Some(0),
            ..Default::default()
        };
        assert_eq!(look.eyes(DexCompanionState::Ready, true), "− −");
        assert_eq!(look.eyes(DexCompanionState::Failed, false), "⠂ ⠕");
        assert_eq!(look.eyes(DexCompanionState::Failed, true), "⠂ ⠕");
        assert_eq!(look.eyes(DexCompanionState::NeedsInput, true), "• ?");
        let glasses = DexLook {
            accessory: DexAccessory::Glasses,
            ..look
        };
        assert_eq!(glasses.eyes(DexCompanionState::Failed, true), "⠂ ⠕");
        assert_eq!(
            DexLook {
                pet_frame: Some(8),
                ..look
            }
            .eyes(DexCompanionState::Ready, true),
            "• •"
        );
        for (frame, expected) in [(0, "− −"), (4, "^ ^"), (6, "• •"), (8, "• •")] {
            let reacting = DexLook {
                pet_frame: Some(frame),
                ..look
            };
            assert_eq!(reacting.eyes(DexCompanionState::Ready, true), expected);
            assert_eq!(reacting.eyes(DexCompanionState::Ready, false), "• •");
            assert_eq!(reacting.eyes(DexCompanionState::Failed, true), "⠂ ⠕");
            assert_eq!(reacting.eyes(DexCompanionState::NeedsInput, true), "• ?");
        }
        assert_eq!(DexActivity::from_tool("bash"), DexActivity::Running);
        assert_eq!(
            DexActivity::from_tool("tests passed"),
            DexActivity::Thinking
        );
        assert_eq!(
            next_prompt(&AppState::new(), Some(DexCompanionState::Finished)),
            None
        );
        assert!(!recap(&AppState::new(), None).contains("passed"));
    }
    #[test]
    fn whimsical_accessories_fit_the_portrait() {
        for accessory in [
            DexAccessory::Sprout,
            DexAccessory::CatEars,
            DexAccessory::Crown,
            DexAccessory::Bow,
        ] {
            let cap = DexLook {
                accessory,
                ..Default::default()
            }
            .cap();
            assert_eq!(unicode_width::UnicodeWidthStr::width(cap), 5);
        }
    }

    #[test]
    fn suggestions_use_only_current_turn_and_never_claim_tests_passed() {
        use crate::state::ToolCallState;
        let mut state = AppState::new();
        state.add_user_message("Change it".into());
        state
            .messages
            .last_mut()
            .unwrap()
            .tool_calls
            .push(ToolCallState {
                parent_call_id: None,
                duration_ms: None,
                call_id: "edit-1".into(),
                tool: "edit".into(),
                args: serde_json::json!({}),
                status: ToolCallStatus::Completed,
                output: "tests passed".into(),
            });
        state.busy = false; // Explicitly completed fixture turn.
        assert_eq!(
            next_prompt(&state, Some(DexCompanionState::Finished)),
            Some("/diff")
        );
        assert!(!recap(&state, Some(DexCompanionState::Finished)).contains("tests passed"));
        state.busy = true;
        assert_eq!(next_prompt(&state, Some(DexCompanionState::Finished)), None);
        state.busy = false;
        state.add_user_message("Unrelated new task".into());
        state.busy = false;
        assert_eq!(next_prompt(&state, Some(DexCompanionState::Finished)), None);
    }
}
