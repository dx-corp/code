use super::*;

#[test]
fn test_supervisor_builder_restores_session_replay() {
    let replay = SessionReplay {
        system_prompt_parts: Vec::new(),
        state: AgentState {
            model: Some("claude-3-opus".to_string()),
            provider: Some("anthropic".to_string()),
            is_ready: true,
            ..AgentState::default()
        },
        last_init: Some(InitConfig {
            system_prompt: Some("You are Maestro".to_string()),
            append_system_prompt: None,
            thinking_level: Some(crate::headless::messages::ThinkingLevel::High),
            approval_mode: Some(crate::headless::messages::ApprovalMode::Prompt),
            history: None,
            code_mode: None,
            tool_grant: None,
        }),
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    };

    let supervisor = SupervisorBuilder::new().session_replay(replay).build();
    assert_eq!(supervisor.state().model.as_deref(), Some("claude-3-opus"));
    assert_eq!(supervisor.state().provider.as_deref(), Some("anthropic"));
    assert!(supervisor.state().is_ready);
}

#[test]
fn session_replay_does_not_override_explicit_remote_session_id() {
    let replay = SessionReplay {
        system_prompt_parts: Vec::new(),
        state: AgentState {
            session_id: Some("sess_replayed".to_string()),
            is_ready: true,
            ..AgentState::default()
        },
        last_init: None,
        semantic_conversation: None,
        last_workspace_capability_set: None,
        last_process_budget: None,
    };

    let supervisor = SupervisorBuilder::new()
        .remote_base_url("http://127.0.0.1:8080")
        .remote_session_id("sess_explicit")
        .session_replay(replay)
        .build();

    assert_eq!(
        supervisor
            .config
            .remote
            .as_ref()
            .and_then(|remote| remote.session_id.as_deref()),
        Some("sess_explicit")
    );
    assert_eq!(
        supervisor.state().session_id.as_deref(),
        Some("sess_replayed")
    );
}
