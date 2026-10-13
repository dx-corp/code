use super::*;

#[tokio::test]
async fn selected_tool_host_survives_child_scope_rotation_and_factory_launch() {
    use crate::agent::tool_host::{LocalHostBuild, ToolHost};
    use crate::agent::{NativeAgentConfig, NativeExecutionHostHandle, ToolDefinition};
    use crate::subagents::factory::ChildLaunchRequest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ChildHostProbe(AtomicUsize);

    impl ToolHost for ChildHostProbe {
        fn build_local_host(
            self: Arc<Self>,
            build: LocalHostBuild<'_>,
        ) -> anyhow::Result<NativeExecutionHostHandle> {
            assert!(!build.skip_identity_verification);
            assert_eq!(build.mailbox_identity.as_deref(), Some("child-mailbox"));
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("selected child tool host")
        }

        fn registry_tool_definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }

        fn executor_tool_definitions(&self, _cwd: &Path) -> Vec<ToolDefinition> {
            Vec::new()
        }
    }

    let _guard = crate::config::test_process_env_lock_async().await;
    let root = isolated_tempdir();
    let probe = Arc::new(ChildHostProbe(AtomicUsize::new(0)));
    let host: Arc<dyn ToolHost> = probe.clone();
    let executor = ToolExecutor::new(root.path().display().to_string())
        .with_tool_host(Arc::clone(&host))
        .with_subagent_parent_scope("new-parent-scope".to_owned());
    assert!(Arc::ptr_eq(&host, &executor.subagents.tool_host()));
    assert_eq!(executor.subagent_parent_scope_id(), "new-parent-scope");
    let result = executor
        .subagents
        .child_agent_factory()
        .spawn(ChildLaunchRequest {
            config: NativeAgentConfig {
                // An explicit local route constructs a client without contacting a
                // provider. The production child path still enables verification.
                model: "ollama/tool-host-child-test".to_owned(),
                cwd: root.path().display().to_string(),
                ..NativeAgentConfig::default()
            },
            allowed_tools: std::collections::HashSet::new(),
            credential_vault: CredentialVault::new(),
            mailbox_identity: "child-mailbox".to_owned(),
        });
    let error = match result {
        Ok(_) => panic!("probe host must stop before starting a child actor"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("selected child tool host"),
        "{error:#}"
    );
    assert_eq!(probe.0.load(Ordering::SeqCst), 1);
}

#[test]
fn test_registry_exposes_subagent_lifecycle_tools() {
    let registry = ToolRegistry::new();

    for tool in crate::tools::subagents::SUBAGENT_TOOL_NAMES {
        assert!(registry.get(tool).is_some(), "missing tool {tool}");
    }

    assert!(registry.requires_approval(
        "spawn_subagent",
        &serde_json::json!({"task": "inspect the parser"})
    ));
    assert!(registry.requires_approval(
        "resume_subagent",
        &serde_json::json!({"subagent_id": "child-1", "task": "continue"})
    ));
    assert!(registry.requires_approval(
        "cleanup_subagent",
        &serde_json::json!({"subagent_id": "child-1"})
    ));
    assert!(
        registry
            .missing_required(
                "resume_subagent",
                &serde_json::json!({
                    "subagent_id": "child-1",
                    "follow_up": "continue"
                })
            )
            .is_empty()
    );
    for tool in [
        "list_subagents",
        "get_subagent",
        "wait_subagent",
        "cancel_subagent",
        "control_subagent",
        "inspect_subagent",
    ] {
        assert!(!registry.requires_approval(tool, &serde_json::json!({"subagent_id": "child-1"})));
    }
}
