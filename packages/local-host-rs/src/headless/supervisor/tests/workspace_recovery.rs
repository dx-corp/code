//! Reconnect preserves exact accepted workspace receipt ownership.
use super::*;
#[cfg(unix)]
#[tokio::test]
async fn reconnect_replays_last_init_and_matching_accepted_capability_set() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script_path = create_test_headless_script(temp.path()).expect("script");
    let sessions_dir = temp.path().join("sessions");
    let recorder = SessionRecorder::new(&sessions_dir).expect("recorder");
    let session_id = recorder.id().to_string();

    let mut config = SupervisorConfig::default();
    config.transport.cli_path = script_path.to_string_lossy().into_owned();
    config.auto_reconnect = false;

    let init = InitConfig {
        system_prompt: Some("system prompt".to_string()),
        append_system_prompt: Some("appendix".to_string()),
        thinking_level: Some(crate::headless::messages::ThinkingLevel::High),
        approval_mode: Some(crate::headless::messages::ApprovalMode::Prompt),
        history: None,
        code_mode: None,
        tool_grant: None,
    };

    let mut supervisor = AgentSupervisor::new(config).with_session_recorder(recorder);
    supervisor.connect().await.expect("connect");
    supervisor.init(init.clone()).expect("initial init");
    let capability = ApplyWorkspaceCapabilitySet {
        organization_id: "org-1".to_string(),
        workspace_id: "workspace-1".to_string(),
        runner_session_id: "runner-1".to_string(),
        runtime_generation: 7,
        activation_generation: 3,
        workspace_snapshot_digest: "sha256:snapshot".to_string(),
        workspace_skill_set_digest: "sha256:skills".to_string(),
        capability_set_digest: "sha256:catalog".to_string(),
        workspace_instructions: vec!["Use the workspace review skill.".to_string()],
        admitted_catalog: Vec::new(),
        admission_receipt_id: "admission-3".to_string(),
    };
    supervisor
        .send(ToAgentMessage::ApplyWorkspaceCapabilitySet {
            request: capability.clone(),
        })
        .expect("send capability set");
    let receipt = WorkspaceCapabilitySetApplied {
        schema_version: "evalops.maestro.workspace-prompt-capability-set.v1".to_string(),
        organization_id: capability.organization_id.clone(),
        workspace_id: capability.workspace_id.clone(),
        runner_session_id: capability.runner_session_id.clone(),
        runtime_generation: capability.runtime_generation,
        activation_generation: capability.activation_generation,
        effective_catalog_digest: capability.capability_set_digest.clone(),
        accepted_entry_digests: Vec::new(),
        rejected_entries: Vec::new(),
        replay_cursor: workspace_capability_replay_cursor(&capability),
        applied_at: 123,
        controller_binding_sha256: "sha256:binding".to_string(),
        provider_prompt_sha256: "sha256:provider-prompt".to_string(),
        staged_for_next_turn: false,
        current_activation_generation: None,
        current_catalog_digest: None,
        idempotent: false,
    };
    let mut partial = receipt.clone();
    partial.rejected_entries.push("skill.review".to_string());
    assert!(
        supervisor
            .apply_agent_message(FromAgentMessage::WorkspaceCapabilitySetApplied {
                receipt: partial,
            })
            .is_none(),
        "a partial receipt must not become a live activation event"
    );
    assert!(supervisor.last_workspace_capability_set.is_none());
    let _ =
        supervisor.apply_agent_message(FromAgentMessage::WorkspaceCapabilitySetApplied { receipt });

    supervisor.disconnect();
    tokio::time::sleep(Duration::from_millis(50)).await;

    supervisor.reconnect().await.expect("reconnect");
    supervisor.disconnect();
    supervisor.flush_session().expect("flush");

    let logged_inits: Vec<_> = SessionReader::load(&sessions_dir, &session_id)
        .expect("load session")
        .sent_messages()
        .into_iter()
        .filter_map(|message| match message {
            ToAgentMessage::Init {
                system_prompt,
                append_system_prompt,
                thinking_level,
                approval_mode,
                history: _,
            } => Some((
                system_prompt.clone(),
                append_system_prompt.clone(),
                *thinking_level,
                *approval_mode,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(logged_inits.len(), 2);
    for (system_prompt, append_system_prompt, thinking_level, approval_mode) in logged_inits {
        assert_eq!(system_prompt, init.system_prompt);
        assert_eq!(append_system_prompt, init.append_system_prompt);
        assert_eq!(thinking_level, init.thinking_level);
        assert_eq!(approval_mode, init.approval_mode);
    }

    let logged_capabilities: Vec<_> = SessionReader::load(&sessions_dir, &session_id)
        .expect("reload session")
        .sent_messages()
        .into_iter()
        .filter_map(|message| match message {
            ToAgentMessage::ApplyWorkspaceCapabilitySet { request } => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(logged_capabilities, vec![capability.clone(), capability]);
}
