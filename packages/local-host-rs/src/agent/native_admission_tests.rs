use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::{Arc, RwLock};

use maestro_runtime::agent::NativeExecutionHostHandle;
use serde_json::json;

use super::{CredentialVault, NativeAgent, NativeAgentConfig};
use crate::ai::{
    Message, MessageContent, RequestConfig, Role, ScriptedClient, ScriptedResponse, Tool,
    UnifiedClient,
};
use crate::mcp::ManagedMcpPolicy;
use crate::sandbox::SandboxPolicy;
use crate::tools::{ToolExecutor, ToolRegistry};

struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

impl EnvRestore {
    fn capture(names: &[&'static str]) -> Self {
        Self(
            names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        )
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn identity_env_names(include_base_url: bool, include_authority: bool) -> Vec<&'static str> {
    let mut names = vec![
        "MAESTRO_HOME",
        "MAESTRO_OAUTH_STORAGE_MODE",
        "MAESTRO_DISABLE_KEYCHAIN",
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
    ];
    if include_base_url {
        names.push(crate::credential_mode::BASE_URL_ENV);
    }
    if include_authority {
        names.push(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV);
    }
    names
}

fn tui_host(config: NativeAgentConfig) -> NativeExecutionHostHandle {
    super::build_local_host(
        &config,
        CredentialVault::new(),
        None,
        None,
        None,
        Arc::new(RwLock::new(None)),
    )
    .expect("TUI execution host")
}

fn composed_tui_host(executor: ToolExecutor) -> NativeExecutionHostHandle {
    super::native_host::LocalNativeExecutionHost::compose(
        Arc::new(executor),
        crate::hooks::IntegratedHookSystem::new("."),
        |_model, _preserve_scope| Err("test host has no model resolver".to_owned()),
        |_model| super::NativeModelRoute::DirectProvider,
        None,
    )
}

#[test]
fn codex_models_resolve_to_app_server_after_identity_check() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, false));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var(
        crate::credential_mode::ACCESS_TOKEN_ENV,
        "platform-test-token",
    );
    std::env::remove_var(crate::credential_mode::ACCESS_TOKEN_FILE_ENV);
    std::env::set_var(crate::credential_mode::ORG_ID_ENV, "org-test");
    std::env::set_var(crate::credential_mode::WORKSPACE_ID_ENV, "workspace-test");
    std::env::set_var(
        "MAESTRO_IDENTITY_URL",
        crate::credential_mode::test_identity_base_url(),
    );

    let (resolved, telemetry_scope) =
        super::resolve_native_client("openai-codex/gpt-5.5", None).expect("Codex transport");
    assert!(resolved.client.is_none());
    assert_eq!(resolved.provider_name, "openai-codex");
    assert!(resolved.model_route.uses_app_server());
    assert!(telemetry_scope.is_some());
}

#[test]
fn codex_models_require_evalops_identity_before_using_the_app_server() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, false));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
    ] {
        std::env::remove_var(name);
    }

    let error = match super::resolve_native_client("openai-codex/gpt-5.5", None) {
        Ok(_) => panic!("Codex transport must not bypass EvalOps Identity"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deixic-code evalops login"));
}

#[test]
fn openai_api_key_resolves_native_client_without_identity() {
    let _guard = crate::config::test_process_env_lock();
    let mut names = identity_env_names(false, true);
    names.extend(["OPENAI_API_KEY", "OPENAI_BASE_URL"]);
    let _restore = EnvRestore::capture(&names);
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var("OPENAI_API_KEY", "synthetic-maestro-smoke-key");
    std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:9/v1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
    ] {
        std::env::remove_var(name);
    }

    let (resolved, telemetry_scope) =
        super::resolve_native_client("openai/gpt-5.4", None).expect("direct OpenAI client");
    assert_eq!(resolved.provider_name, "openai");
    assert!(!resolved.model_route.uses_app_server());
    assert!(resolved.client.is_some());
    assert!(telemetry_scope.is_none());
}

#[test]
fn local_runtime_ignores_expired_stored_identity_but_managed_route_does_not() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, true));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
    ] {
        std::env::remove_var(name);
    }
    std::fs::write(
        maestro_home.path().join("oauth.json"),
        serde_json::to_vec(&json!({
            "evalops": {
                "type": "oauth",
                "refresh": "",
                "access": "expired-airplane-token",
                "expires": 1,
                "metadata": {
                    "organizationId": "org-test",
                    "workspaceId": "workspace-test",
                    "identityBaseUrl": "https://identity.evalops.dev"
                }
            }
        }))
        .expect("stored OAuth fixture"),
    )
    .expect("write stored OAuth fixture");
    crate::init_cli::invalidate_evalops_credentials_cache();

    for provider in ["ollama", "lmstudio", "llamacpp"] {
        let (resolved, telemetry_scope) =
            super::resolve_native_client(&format!("{provider}/qwen3"), None)
                .expect("local runtime must remain available offline");
        assert_eq!(resolved.provider_name, provider);
        assert!(resolved.client.is_some());
        assert!(telemetry_scope.is_none());
    }

    let managed = match super::resolve_native_client("evalops/gpt-5.5", None) {
        Ok(_) => panic!("managed inference must not use an expired offline session"),
        Err(error) => error,
    };
    assert!(format!("{managed:#}").contains("EvalOps login expired"));
    crate::init_cli::invalidate_evalops_credentials_cache();
}

#[test]
fn evalops_route_requires_identity_even_with_openai_api_key() {
    let _guard = crate::config::test_process_env_lock();
    let mut names = identity_env_names(false, true);
    names.extend(["OPENAI_API_KEY", "OPENAI_BASE_URL"]);
    let _restore = EnvRestore::capture(&names);
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var("OPENAI_API_KEY", "synthetic-maestro-smoke-key");
    std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:9/v1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
        crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV,
    ] {
        std::env::remove_var(name);
    }

    let error = match super::resolve_native_client("evalops/gpt-5.5", None) {
        Ok(_) => panic!("managed evalops must not bypass EvalOps Identity"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deixic-code evalops login"));
}

#[test]
fn injected_network_client_requires_evalops_identity() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, false));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
    ] {
        std::env::remove_var(name);
    }

    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", "http://127.0.0.1:1/v1")
            .expect("test client"),
    );
    let error = match NativeAgent::new_with_client(
        NativeAgentConfig {
            model: "openai/gpt-5.5".to_owned(),
            ..NativeAgentConfig::default()
        },
        client,
    ) {
        Ok(_) => panic!("an injected provider client must not bypass EvalOps Identity"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deixic-code evalops login"));
}

#[test]
fn injected_scripted_client_requires_evalops_identity() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, false));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
    ] {
        std::env::remove_var(name);
    }

    let client = UnifiedClient::Scripted(ScriptedClient::new(
        "scripted-replay/maestro-replay-v1",
        vec![ScriptedResponse::text("replay")],
    ));
    let error = match NativeAgent::new_with_client(
        NativeAgentConfig {
            model: "scripted-replay/maestro-replay-v1".to_owned(),
            ..NativeAgentConfig::default()
        },
        client,
    ) {
        Ok(_) => panic!("an injected scripted client must not bypass EvalOps Identity"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deixic-code evalops login"));
}

#[tokio::test]
async fn injected_scripted_client_admits_after_verified_identity() {
    let _guard = crate::config::test_process_env_lock_async().await;
    let mut names = vec![
        "MAESTRO_HOME",
        "MAESTRO_OAUTH_STORAGE_MODE",
        "MAESTRO_DISABLE_KEYCHAIN",
    ];
    names.extend(
        crate::credential_mode::TEST_IDENTITY_ENV_VARS
            .iter()
            .copied(),
    );
    let _restore = EnvRestore::capture(&names);
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    crate::credential_mode::install_test_identity_env();

    let client = UnifiedClient::Scripted(ScriptedClient::new(
        "scripted-replay/maestro-replay-v1",
        vec![ScriptedResponse::text("replay")],
    ));
    NativeAgent::new_with_client(
        NativeAgentConfig {
            model: "scripted-replay/maestro-replay-v1".to_owned(),
            ..NativeAgentConfig::default()
        },
        client,
    )
    .expect("verified Identity must admit an injected scripted client");
}

#[test]
fn injected_network_client_rejects_caller_selected_identity_authority() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, true));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var(
        crate::credential_mode::ACCESS_TOKEN_ENV,
        "attacker-selected-token",
    );
    std::env::set_var(crate::credential_mode::ORG_ID_ENV, "attacker-org");
    std::env::set_var(
        "MAESTRO_IDENTITY_URL",
        crate::credential_mode::test_identity_base_url(),
    );
    std::env::set_var(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV, "0");

    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", "http://127.0.0.1:1/v1")
            .expect("test client"),
    );
    let error = match NativeAgent::new_with_client(
        NativeAgentConfig {
            model: "openai/gpt-5.5".to_owned(),
            ..NativeAgentConfig::default()
        },
        client,
    ) {
        Ok(_) => panic!("a caller-selected Identity authority must not admit a provider"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deixic-code evalops login"));
    assert!(format!("{error:#}").contains("untrusted EvalOps Identity authority"));
}

#[test]
fn injected_allowed_tools_client_requires_evalops_identity() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(false, false));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    for name in [
        crate::credential_mode::ACCESS_TOKEN_ENV,
        crate::credential_mode::ACCESS_TOKEN_FILE_ENV,
        crate::credential_mode::ORG_ID_ENV,
        crate::credential_mode::WORKSPACE_ID_ENV,
        "MAESTRO_IDENTITY_URL",
    ] {
        std::env::remove_var(name);
    }

    let client = UnifiedClient::OpenAI(
        crate::ai::OpenAiClient::with_base_url("test-key", "http://127.0.0.1:1/v1")
            .expect("test client"),
    );
    let error = match NativeAgent::new_with_client_and_allowed_tools(
        NativeAgentConfig {
            model: "openai/gpt-5.5".to_owned(),
            ..NativeAgentConfig::default()
        },
        &HashSet::new(),
        client,
    ) {
        Ok(_) => panic!("an allowed-tools provider client must not bypass EvalOps Identity"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deixic-code evalops login"));
}

#[test]
fn codex_models_ignore_platform_credentials_and_use_app_server() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&identity_env_names(true, false));
    let maestro_home = tempfile::tempdir().expect("maestro home");
    std::env::set_var("MAESTRO_HOME", maestro_home.path());
    std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
    std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
    std::env::set_var(
        crate::credential_mode::ACCESS_TOKEN_ENV,
        "platform-test-token",
    );
    std::env::remove_var(crate::credential_mode::ACCESS_TOKEN_FILE_ENV);
    std::env::set_var(crate::credential_mode::ORG_ID_ENV, "org-test");
    std::env::set_var(crate::credential_mode::WORKSPACE_ID_ENV, "workspace-test");
    std::env::remove_var(crate::credential_mode::BASE_URL_ENV);
    std::env::set_var(
        "MAESTRO_IDENTITY_URL",
        crate::credential_mode::test_identity_base_url(),
    );

    let (resolved, telemetry_scope) =
        super::resolve_native_client("openai-codex/gpt-5.5", None).expect("Codex transport");
    assert!(
        resolved.client.is_none(),
        "Codex must use the app-server even when an EvalOps session is available"
    );
    assert_eq!(resolved.provider_name, "openai-codex");
    assert!(resolved.model_route.uses_app_server());
    assert!(telemetry_scope.is_some());
}

#[test]
fn ide_tool_flag_accepts_only_explicit_truthy_values() {
    let _guard = crate::config::test_process_env_lock();
    let _restore = EnvRestore::capture(&["MAESTRO_INCLUDE_IDE_TOOLS"]);
    let host = tui_host(NativeAgentConfig::default());
    for value in ["1", "true", "TRUE", " yes ", "on"] {
        std::env::set_var("MAESTRO_INCLUDE_IDE_TOOLS", value);
        assert!(host.include_ide_tools(), "{value:?}");
    }
    for value in ["", "0", "false", "y", "random"] {
        std::env::set_var("MAESTRO_INCLUDE_IDE_TOOLS", value);
        assert!(!host.include_ide_tools(), "{value:?}");
    }
}

#[test]
fn test_config_default_output_budget_follows_catalog() {
    assert_eq!(
        crate::model_catalog::default_max_output_tokens("gpt-5.6"),
        128_000
    );
    assert_eq!(
        crate::model_catalog::default_max_output_tokens("gpt-99-turbo"),
        16_384
    );
}

#[test]
fn discovered_context_limit_reaches_compaction_for_uncataloged_model() {
    let model = crate::model_catalog::ModelInfo {
        id: "small-runtime-model".to_owned(),
        name: "small-runtime-model".to_owned(),
        provider: "llamacpp".to_owned(),
        description: "test runtime model".to_owned(),
        capabilities: crate::model_catalog::ModelCapabilities {
            protocol: crate::model_catalog::ModelProtocol::OpenAiChat,
            tools: false,
            vision: false,
            reasoning: false,
            streaming: true,
            context_tokens: 8_192,
            output_tokens: None,
        },
        verification: crate::model_catalog::ModelVerification {
            state: crate::model_catalog::VerificationState::Verified,
            source: "test-runtime".to_owned(),
            detail: None,
        },
    };
    crate::local_models::replace_discovered_models(9_001, &[model], None);
    assert_eq!(
        crate::model_catalog::default_max_output_tokens("llamacpp/small-runtime-model"),
        4_096
    );

    let compactor = super::ContextCompactor::new(super::CompactionConfig::for_model(
        "llamacpp/small-runtime-model",
        None,
    ));
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text("x".repeat(200_000)),
    }];
    assert!(compactor.should_auto_compact(&messages));
}

#[test]
fn runner_tool_executor_carries_the_configured_sandbox_policy() {
    let _guard = crate::config::test_process_env_lock();
    let bypass_args = json!({"command": "ls -la", "bypass_sandbox": true});
    let sandboxed = tui_host(NativeAgentConfig {
        sandbox_policy: Some(SandboxPolicy::ReadOnly),
        ..NativeAgentConfig::default()
    });
    assert!(
        sandboxed.requires_sandbox_bypass_approval("bash", &bypass_args),
        "a configured sandbox_policy must reach the TUI host's executor"
    );

    let unsandboxed = tui_host(NativeAgentConfig::default());
    assert!(
        !unsandboxed.requires_sandbox_bypass_approval("bash", &bypass_args),
        "no sandbox_policy configured must produce no sandbox awareness"
    );
}

#[test]
fn tui_host_propagates_composed_code_authority() {
    let _guard = crate::config::test_process_env_lock();
    let workspace = tempfile::tempdir().expect("workspace");

    let without_authority =
        composed_tui_host(ToolExecutor::new(workspace.path().display().to_string()));
    assert!(
        !without_authority.has_code_authority(),
        "a default executor must not acquire code authority through host composition"
    );

    let with_authority = composed_tui_host(
        ToolExecutor::new(workspace.path().display().to_string()).with_test_code_authority(),
    );
    assert!(
        with_authority.has_code_authority(),
        "the TUI host must expose the concrete executor's code authority"
    );
}

#[test]
fn runner_tool_executor_carries_the_managed_mcp_policy() {
    let executor = ToolExecutor::with_credential_vault(".", CredentialVault::new())
        .with_code_authority()
        .with_managed_mcp_policy(Some(ManagedMcpPolicy {
            version: 42,
            policy: Default::default(),
        }));
    assert_eq!(executor.managed_mcp_policy_version_for_test(), Some(42));
}

#[test]
fn runner_tool_executor_uses_the_parent_subagent_scope() {
    let executor = ToolExecutor::with_credential_vault(".", CredentialVault::new())
        .with_code_authority()
        .with_subagent_parent_scope("app-parent-scope".to_owned());
    assert_eq!(
        executor.subagent_parent_scope_id(),
        "app-parent-scope",
        "auto-executed delegation must publish events to the caller's scope"
    );
}

#[test]
fn runner_tool_executor_binds_model_mailbox_calls_to_runtime_identity() {
    let executor = ToolExecutor::with_credential_vault(".", CredentialVault::new())
        .with_code_authority()
        .with_mailbox_identity("subagent:child:3");
    assert_eq!(executor.mailbox_identity(), "subagent:child:3");
}

#[test]
fn test_tool_registry_integration() {
    let registry = ToolRegistry::new();
    let tools: Vec<_> = registry.tools().collect();
    assert!(tools.len() >= 5);

    let names: Vec<_> = tools.iter().map(|tool| tool.tool.name.as_str()).collect();
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"read"));
    assert!(names.contains(&"write"));
    assert!(names.contains(&"glob"));
    assert!(names.contains(&"grep"));
}

#[test]
fn raw_orb_lifecycle_tools_stay_out_of_model_discovery() {
    let _guard = crate::config::test_process_env_lock();
    let host = tui_host(NativeAgentConfig::default());
    let raw_name = "mcp__orb__orb_run_task";
    assert!(host.is_reserved_tool(raw_name));
    assert!(
        host.tool_definitions()
            .iter()
            .all(|definition| definition.tool.name != raw_name),
        "raw Computer lifecycle operations must not be registered as model tools"
    );
}

#[test]
fn test_request_config_building() {
    let config = NativeAgentConfig {
        model: "claude-sonnet-4-5-20250514".to_owned(),
        max_tokens: 8192,
        system_prompt: Some("Test system prompt".to_owned()),
        thinking_enabled: false,
        thinking_budget: 0,
        cwd: ".".to_owned(),
        ..NativeAgentConfig::default()
    };

    let tools: Vec<Tool> = ToolRegistry::new()
        .tools()
        .map(|definition| definition.tool.clone())
        .collect();
    let request_config = RequestConfig {
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        temperature: Some(0.7),
        system: config.system_prompt.clone(),
        tools: tools.into(),
        thinking: None,
        cache_system_prompt: true,
        cache_topology: None,
    };

    assert_eq!(request_config.model, "claude-sonnet-4-5-20250514");
    assert_eq!(request_config.max_tokens, 8192);
    assert!(request_config.system.is_some());
    assert!(!request_config.tools.is_empty());
    assert!(request_config.cache_system_prompt);
}

#[test]
fn codex_native_mutations_are_denied_by_real_tui_host() {
    let _guard = crate::config::test_process_env_lock();
    let workspace = tempfile::tempdir().expect("workspace");
    let config = NativeAgentConfig {
        cwd: workspace.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let host = tui_host(config);

    let command_denial = maestro_runtime::agent::codex_native_effect_denial_for_test(
        &host,
        "item/commandExecution/requestApproval",
        Some(&json!({"command": "rm -rf /"})),
    );
    assert!(
        command_denial.is_some(),
        "dangerous commands must be blocked by the real TUI host firewall"
    );

    let workspace_file = workspace.path().join("ok");
    let workspace_file_path = workspace_file.to_string_lossy().into_owned();
    let multi_file_denial = maestro_runtime::agent::codex_native_effect_denial_for_test(
        &host,
        "item/fileChange/requestApproval",
        Some(&json!({
            "files": [workspace_file_path.clone(), "/etc/passwd"],
            "content": "x"
        })),
    );
    assert!(
        multi_file_denial.is_some(),
        "a later out-of-workspace path must fail the whole multi-file request"
    );

    let item_id_only_denial = maestro_runtime::agent::codex_native_effect_denial_for_test(
        &host,
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-1",
            "threadId": "t",
            "turnId": "u",
            "startedAtMs": 1
        })),
    );
    assert!(
        item_id_only_denial.is_some(),
        "itemId-only file-change approvals must fail closed with no recoverable paths"
    );

    let move_denial = maestro_runtime::agent::codex_native_effect_denial_for_test(
        &host,
        "item/fileChange/requestApproval",
        Some(&json!({
            "itemId": "item-4",
            "changes": [{
                "path": workspace_file_path,
                "kind": {"move_path": "/etc/passwd"}
            }]
        })),
    );
    assert!(
        move_denial.is_some(),
        "out-of-workspace move destination must be blocked"
    );
}
