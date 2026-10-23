use super::*;

#[test]
fn candidate_prompt_experiment_reaches_provider_bound_system_prompt() {
    let artifact_content = "When debugging, state the causal path before proposing a fix.";
    let mut state = HeadlessState::new(
        Arc::new(crate::agent::tool_host::BuiltInToolHost),
        Some("gpt-test".to_string()),
    );
    apply_init_settings(
        &mut state,
        Some("base prompt".to_string()),
        None,
        None,
        None,
        None,
    );
    configure_prompt_experiment(
        &mut state,
        PromptExperimentAssignment {
            experiment_id: "debug-causal-path-v1".to_string(),
            assignment_id: "assignment-1".to_string(),
            arm: PromptExperimentArm::Candidate,
            artifact_id: "skill-debug-causal-path".to_string(),
            artifact_version: "1".to_string(),
            artifact_sha256: sha256_prefixed(artifact_content),
            artifact_content: artifact_content.to_string(),
        },
    )
    .expect("candidate assignment is valid");

    assert!(state.system_prompt.contains(artifact_content));
    let exposure = state
        .meta
        .lock()
        .expect("runtime metadata")
        .prompt_experiment
        .clone()
        .expect("exposure prepared");
    let provider_prompt = crate::agent::provider_system_prompt(
        &state.system_prompt,
        &state.model,
        state.model_capabilities,
    );
    assert!(exposure.applied);
    assert_eq!(
        exposure.provider_prompt_sha256,
        sha256_prefixed(&provider_prompt)
    );
}

#[test]
fn control_prompt_experiment_leaves_provider_bound_system_prompt_unchanged() {
    let artifact_content = "When debugging, state the causal path before proposing a fix.";
    let mut state = HeadlessState::new(
        Arc::new(crate::agent::tool_host::BuiltInToolHost),
        Some("gpt-test".to_string()),
    );
    apply_init_settings(
        &mut state,
        Some("base prompt".to_string()),
        None,
        None,
        None,
        None,
    );
    let provider_prompt_before = crate::agent::provider_system_prompt(
        &state.system_prompt,
        &state.model,
        state.model_capabilities,
    );

    configure_prompt_experiment(
        &mut state,
        PromptExperimentAssignment {
            experiment_id: "debug-causal-path-v1".to_string(),
            assignment_id: "assignment-control-1".to_string(),
            arm: PromptExperimentArm::Control,
            artifact_id: "skill-debug-causal-path".to_string(),
            artifact_version: "1".to_string(),
            artifact_sha256: sha256_prefixed(artifact_content),
            artifact_content: artifact_content.to_string(),
        },
    )
    .expect("control assignment is valid");

    let provider_prompt_after = crate::agent::provider_system_prompt(
        &state.system_prompt,
        &state.model,
        state.model_capabilities,
    );
    let exposure = state
        .meta
        .lock()
        .expect("runtime metadata")
        .prompt_experiment
        .clone()
        .expect("exposure prepared");
    assert_eq!(provider_prompt_after, provider_prompt_before);
    assert!(!provider_prompt_after.contains(artifact_content));
    assert!(!exposure.applied);
    assert_eq!(
        exposure.provider_prompt_sha256,
        sha256_prefixed(&provider_prompt_before)
    );
}
