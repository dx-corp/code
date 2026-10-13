use super::*;

#[tokio::test(flavor = "current_thread")]
async fn disconnected_native_and_subagent_admission_preserve_policy() {
    let _lock = test_env_guard();
    let fixture = MachineFixture::new();
    fixture.install(private_profile());
    let config = crate::agent::NativeAgentConfig {
        model: "ollama/customer-model".into(),
        cwd: fixture.directory.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let (agent, _) = crate::agent::NativeAgent::new(
        std::sync::Arc::new(crate::agent::tool_host::BuiltInToolHost),
        config.clone(),
    )
    .expect("private native agent");
    let (child, _) = crate::agent::NativeAgent::new_with_credential_vault_and_subagent_scope(
        std::sync::Arc::new(crate::agent::tool_host::BuiltInToolHost),
        config.clone(),
        crate::agent::CredentialVault::new(),
        "parent-scope".into(),
    )
    .expect("private child agent");
    drop(child);
    drop(agent);
    let forbidden = crate::agent::NativeAgentConfig {
        model: "evalops/default".into(),
        ..config.clone()
    };
    assert!(
        crate::agent::NativeAgent::new(
            std::sync::Arc::new(crate::agent::tool_host::BuiltInToolHost),
            forbidden.clone()
        )
        .is_err()
    );
    assert!(
        crate::agent::NativeAgent::new_with_credential_vault_and_subagent_scope(
            std::sync::Arc::new(crate::agent::tool_host::BuiltInToolHost),
            forbidden,
            crate::agent::CredentialVault::new(),
            "parent-scope".into()
        )
        .is_err()
    );
    let injected =
        crate::ai::UnifiedClient::OpenAI(crate::ai::OpenAiClient::new("fixture").unwrap());
    assert!(
        crate::agent::NativeAgent::new_with_client(
            std::sync::Arc::new(crate::agent::tool_host::BuiltInToolHost),
            config,
            injected
        )
        .is_err()
    );
}
