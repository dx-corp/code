//! Shared local host for the native Maestro runtime.
//!
//! The turn actor and its protocol live in maestro-runtime. This module
//! retains the public constructor and configuration surface used by the TUI,
//! print mode, headless mode, and delegated subagents while composing the one
//! concrete local execution host around that actor.

pub mod codex_app_server_turns;
#[cfg(test)]
pub mod harness;
#[cfg(test)]
mod native_admission_tests;
#[cfg(test)]
mod native_codex_tests;
mod native_host;
#[cfg(test)]
mod native_lifecycle_tests;
#[cfg(test)]
mod native_read_only_tests;

// The runtime owns these modules. Re-exporting them here keeps existing TUI
// callers source-compatible without putting a dependency from runtime back to
// the UI crate.
pub mod compaction;
pub use maestro_runtime::agent::credential_store;
pub use maestro_runtime::agent::extensions;
pub use maestro_runtime::agent::message_queue;
pub use maestro_runtime::agent::process_budget;
pub use maestro_runtime::agent::protocol;
pub use maestro_runtime::agent::reminders;
pub use maestro_runtime::agent::retry;
pub use maestro_runtime::agent::safety;
pub use maestro_runtime::agent::selective_summary;
pub use maestro_runtime::agent::session_scope;
pub use maestro_runtime::agent::steer_signal;
pub use maestro_runtime::agent::text_loop;
pub use maestro_runtime::agent::turn_budget;

pub use maestro_runtime::agent::{TokenCounter, token_counting, token_estimation};

pub use compaction::{CompactionConfig, CompactionResult, ContextCompactor, CutPoint};
pub use credential_store::{CredentialStats, CredentialStore, CredentialType, CredentialVault};
pub use extensions::{
    AgentExtension, BatchEndContext, DoomLoopExtension, ExtensionRegistry, ExtensionStats,
    ExtensionVerdict, ToolCallContext as ExtensionToolCallContext, ToolResultContext,
    ToolResultPayload, TurnEndContext, TurnStartContext,
};
pub use message_queue::{
    MAX_PENDING_MESSAGES, MessageQueue, PendingMessage, PromptKind, QueuePlacement, QueueStats,
};
pub use process_budget::{ProcessBudgetLimits, ProcessBudgetState};
pub use protocol::{
    DenialReason, ExecutionPhase, ExecutionReceipt, ExecutionSource, ExecutionStatus, FromAgent,
    InlineToolApprovalContext, ManagedInferenceAuthorization, ToAgent, TokenUsage, ToolError,
    ToolExecution, ToolOutcome, ToolOutput, ToolReceiptDetails, ToolResult,
    UNTRUSTED_CONTENT_POLICY, ensure_untrusted_content_policy,
};
pub use reminders::{REMINDER_CLOSE, REMINDER_OPEN, Reminder, ReminderContext, ReminderEngine};
pub use retry::{ErrorKind, RetryConfig, RetryDecision, RetryPolicy};
pub use safety::{
    SafetyConfig, SafetyController, SafetyVerdict, is_context_overflow, is_retryable_error,
    stable_stringify,
};
pub use selective_summary::{
    RangeSelection, SelectiveSummaryOutcome, SelectiveSummaryPreview, SelectiveSummaryRequest,
    SelectiveSummaryResult, SummaryTurn,
};
pub use session_scope::{ParentScopeId, SessionId, parent_scope_for_session};
pub use steer_signal::SteerSignal;
pub use text_loop::{LoopKind, TextLoopDetector, loop_reminder_message};
pub use turn_budget::{DEFAULT_MAX_TURN_STEPS, TurnOutcome, TurnStepBudget};

pub use maestro_runtime::agent::ExternalToolSchemaPolicy;
pub use maestro_runtime::agent::{
    ApprovalMode as RuntimeApprovalMode, BoostStatus, MaxTokensSource, ModelChoice,
    ModelDynamicsConfig, NativeCodexAuth, NativeCodingCompletion, NativeExecutionHost,
    NativeExecutionHostHandle, NativeFirewallVerdict, NativeHookEvent, NativeHookResult,
    NativeHostFuture, NativeModelRoute, NativeReadOnlyToolCall, NativeResolvedClient,
    NativeToolAnnotations, NativeToolExecutionOptions, QueueMode as RuntimeQueueMode,
    TaskDifficulty, ThinkingLevel, ToolDefinition, ToolResponseConsumption, ToolResponseMessage,
};
pub use maestro_runtime::agent::{DynamicToolSpec, dynamic_tools_from_native};

use std::collections::HashSet;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use maestro_ai::UnifiedClient;

use crate::agent::native_host::LocalNativeExecutionHost;
use crate::hooks::IntegratedHookSystem;
use crate::state::{ApprovalMode, QueueMode};
use crate::tools::ToolExecutor;

pub(crate) use native_host::catalog_model_capabilities;

/// Render the same standing instructions used by the native runtime.
pub(crate) fn provider_system_prompt(
    base: &str,
    model: &str,
    capabilities: maestro_runtime::agent::NativeModelCapabilities,
) -> String {
    maestro_runtime::agent::runtime_system_prompt(Some(base), None, model, capabilities)
        .expect("runtime model instructions are always present")
}

/// Configuration retained for the TUI-facing API.
///
/// managed_mcp_policy stays on this application-facing value because the TUI
/// resolves it alongside its sandbox and approval state. The runtime receives
/// the transport-neutral fields after the host has consumed that policy to
/// build its concrete executor.
#[derive(Debug, Clone)]
pub struct NativeAgentConfig {
    pub model: String,
    /// Headless sessions bind prompt receipts and provider rendering to one snapshot.
    pub model_capabilities: Option<maestro_runtime::agent::NativeModelCapabilities>,
    pub max_tokens: u32,
    pub max_tokens_source: maestro_runtime::agent::MaxTokensSource,
    pub system_prompt: Option<String>,
    pub thinking_enabled: bool,
    pub thinking_budget: u32,
    pub model_dynamics: ModelDynamicsConfig,
    pub cwd: String,
    pub approval_mode: ApprovalMode,
    pub context_window: Option<u64>,
    pub sandbox_policy: Option<crate::sandbox::SandboxPolicy>,
    pub managed_mcp_policy: Option<crate::mcp::ManagedMcpPolicy>,
    pub max_turn_steps: usize,
    pub allow_unbounded_turn: bool,
    pub retry_config: RetryConfig,
    pub external_tool_schema_policy: ExternalToolSchemaPolicy,
}

impl Default for NativeAgentConfig {
    fn default() -> Self {
        let model = "gpt-5.1-codex-max".to_owned();
        Self {
            max_tokens: crate::model_catalog::default_max_output_tokens(&model),
            model,
            model_capabilities: None,
            max_tokens_source: maestro_runtime::agent::MaxTokensSource::Catalog,
            system_prompt: None,
            thinking_enabled: false,
            thinking_budget: 10_000,
            model_dynamics: crate::config::model_dynamics_config(),
            cwd: std::env::current_dir().map_or_else(
                |_| ".".to_owned(),
                |path| path.to_string_lossy().into_owned(),
            ),
            approval_mode: ApprovalMode::default(),
            context_window: None,
            sandbox_policy: None,
            managed_mcp_policy: None,
            max_turn_steps: DEFAULT_MAX_TURN_STEPS,
            allow_unbounded_turn: false,
            retry_config: RetryConfig::default(),
            external_tool_schema_policy: ExternalToolSchemaPolicy::default(),
        }
    }
}

impl NativeAgentConfig {
    #[must_use]
    pub fn resolved_max_turn_steps(&self) -> usize {
        if self.allow_unbounded_turn {
            usize::MAX
        } else {
            self.max_turn_steps.max(1)
        }
    }

    fn into_runtime(self) -> maestro_runtime::agent::NativeAgentConfig {
        maestro_runtime::agent::NativeAgentConfig {
            model: self.model,
            max_tokens: self.max_tokens,
            max_tokens_source: self.max_tokens_source,
            system_prompt: self.system_prompt,
            thinking_enabled: self.thinking_enabled,
            thinking_budget: self.thinking_budget,
            model_dynamics: self.model_dynamics,
            cwd: self.cwd,
            approval_mode: runtime_approval_mode(self.approval_mode),
            context_window: self.context_window,
            sandbox_policy: self.sandbox_policy,
            max_turn_steps: self.max_turn_steps,
            allow_unbounded_turn: self.allow_unbounded_turn,
            retry_config: self.retry_config,
            safety_config: crate::agent::safety::SafetyConfig::default(),
            external_tool_schema_policy: self.external_tool_schema_policy,
        }
    }
}

fn runtime_approval_mode(mode: ApprovalMode) -> RuntimeApprovalMode {
    match mode {
        ApprovalMode::Yolo => RuntimeApprovalMode::Yolo,
        ApprovalMode::Selective => RuntimeApprovalMode::Selective,
        ApprovalMode::Safe => RuntimeApprovalMode::Safe,
    }
}

fn runtime_queue_mode(mode: QueueMode) -> RuntimeQueueMode {
    match mode {
        QueueMode::All => RuntimeQueueMode::All,
        QueueMode::One => RuntimeQueueMode::One,
    }
}

/// TUI-owned handle around the runtime actor.
pub struct NativeAgent {
    inner: maestro_runtime::agent::NativeAgent,
}

impl Deref for NativeAgent {
    type Target = maestro_runtime::agent::NativeAgent;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl NativeAgent {
    /// Transfer the one runtime actor to a host-owned lifecycle manager.
    /// The event relay continues to preserve the existing telemetry stream.
    pub fn into_runtime(self) -> maestro_runtime::agent::NativeAgent {
        self.inner
    }

    fn start(
        config: NativeAgentConfig,
        external_tool_definitions: Vec<ToolDefinition>,
        credential_vault: CredentialVault,
        allowed_tools: Option<&HashSet<String>>,
        client_override: Option<ClientOverride>,
        subagent_parent_scope_id: Option<String>,
        mailbox_identity: Option<String>,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        let (resolved, initial_identity_scope) =
            resolve_native_client(&config.model, client_override.clone())?;
        let telemetry_identity_scope = Arc::new(RwLock::new(initial_identity_scope));
        let provider_name = resolved.provider_name.clone();
        let telemetry_config = config.clone();
        let host = build_local_host(
            &config,
            credential_vault.clone(),
            client_override,
            subagent_parent_scope_id.clone(),
            mailbox_identity.clone(),
            Arc::clone(&telemetry_identity_scope),
        )?;
        let runtime_config = config.into_runtime();
        let telemetry_host = host.clone();
        let (inner, events) = maestro_runtime::agent::NativeAgent::start_with_resolved_client(
            runtime_config,
            host,
            external_tool_definitions,
            credential_vault,
            allowed_tools,
            resolved,
        )?;
        let events = relay_runtime_events(
            events,
            &telemetry_config,
            provider_name,
            telemetry_identity_scope,
            Some(telemetry_host),
        );
        Ok((Self { inner }, events))
    }

    pub fn new(
        config: NativeAgentConfig,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::new_with_tools_and_credential_vault(config, Vec::new(), CredentialVault::new())
    }

    pub fn new_with_credential_vault(
        config: NativeAgentConfig,
        credential_vault: CredentialVault,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::new_with_tools_and_credential_vault(config, Vec::new(), credential_vault)
    }

    pub fn new_with_credential_vault_and_subagent_scope(
        config: NativeAgentConfig,
        credential_vault: CredentialVault,
        subagent_parent_scope_id: String,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            Vec::new(),
            credential_vault,
            None,
            None,
            Some(subagent_parent_scope_id),
            None,
        )
    }

    pub fn new_with_allowed_tools_and_credential_vault(
        config: NativeAgentConfig,
        allowed_tools: &HashSet<String>,
        credential_vault: CredentialVault,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            Vec::new(),
            credential_vault,
            Some(allowed_tools),
            None,
            None,
            None,
        )
    }

    pub fn new_with_allowed_tools_and_credential_vault_runner(
        config: NativeAgentConfig,
        allowed_tools: &HashSet<String>,
        credential_vault: CredentialVault,
        mailbox_identity: String,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            Vec::new(),
            credential_vault,
            Some(allowed_tools),
            None,
            None,
            Some(mailbox_identity),
        )
    }

    pub fn new_with_tools(
        config: NativeAgentConfig,
        external_tool_definitions: Vec<ToolDefinition>,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::new_with_tools_and_credential_vault(
            config,
            external_tool_definitions,
            CredentialVault::new(),
        )
    }

    pub fn new_with_tools_and_credential_vault(
        config: NativeAgentConfig,
        external_tool_definitions: Vec<ToolDefinition>,
        credential_vault: CredentialVault,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            external_tool_definitions,
            credential_vault,
            None,
            None,
            None,
            None,
        )
    }

    /// Construct an agent with caller-owned tools and a deterministic client.
    ///
    /// This constructor is available only to the local test-support feature.
    /// Production embeddings resolve an authenticated provider through
    /// [`Self::new_with_tools`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_tools_and_test_client(
        config: NativeAgentConfig,
        external_tool_definitions: Vec<ToolDefinition>,
        client: UnifiedClient,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            external_tool_definitions,
            CredentialVault::new(),
            None,
            Some(ClientOverride::UnverifiedTest(client)),
            None,
            None,
        )
    }

    pub fn new_with_governed_tools_and_credential_vault(
        config: NativeAgentConfig,
        allowed_tools: &HashSet<String>,
        external_tool_definitions: Vec<ToolDefinition>,
        credential_vault: CredentialVault,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            external_tool_definitions,
            credential_vault,
            Some(allowed_tools),
            None,
            None,
            None,
        )
    }

    pub fn new_with_client(
        config: NativeAgentConfig,
        client: UnifiedClient,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            Vec::new(),
            CredentialVault::new(),
            None,
            Some(ClientOverride::Production(client)),
            None,
            None,
        )
    }

    pub fn new_with_client_and_allowed_tools(
        config: NativeAgentConfig,
        allowed_tools: &HashSet<String>,
        client: UnifiedClient,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            Vec::new(),
            CredentialVault::new(),
            Some(allowed_tools),
            Some(ClientOverride::Production(client)),
            None,
            None,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_test_client(
        config: NativeAgentConfig,
        client: UnifiedClient,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<FromAgent>)> {
        Self::start(
            config,
            Vec::new(),
            CredentialVault::new(),
            None,
            Some(ClientOverride::UnverifiedTest(client)),
            None,
            None,
        )
    }

    /// Consume the TUI facade after the runtime has drained its one actor.
    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }

    pub fn set_approval_mode(&self, mode: ApprovalMode) -> Result<()> {
        self.inner.set_approval_mode(runtime_approval_mode(mode))
    }

    pub fn set_steering_mode(&self, mode: QueueMode) -> Result<()> {
        self.inner.set_steering_mode(runtime_queue_mode(mode))
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) -> Result<()> {
        self.inner.set_follow_up_mode(runtime_queue_mode(mode))
    }
}

#[derive(Clone)]
enum ClientOverride {
    Production(UnifiedClient),
    #[cfg(any(test, feature = "test-support"))]
    UnverifiedTest(UnifiedClient),
}

impl ClientOverride {
    fn skips_identity_verification(&self) -> bool {
        match self {
            Self::Production(_) => false,
            #[cfg(any(test, feature = "test-support"))]
            Self::UnverifiedTest(_) => true,
        }
    }

    fn client(&self) -> UnifiedClient {
        match self {
            Self::Production(client) => client.clone(),
            #[cfg(any(test, feature = "test-support"))]
            Self::UnverifiedTest(client) => client.clone(),
        }
    }
}

fn model_route(model: &str) -> NativeModelRoute {
    match crate::codex_auth::resolve_model_route(model) {
        crate::codex_auth::CodexModelRoute::AppServer { model_id } => {
            NativeModelRoute::CodexAppServer { model_id }
        }
        crate::codex_auth::CodexModelRoute::DirectProvider => NativeModelRoute::DirectProvider,
    }
}

fn resolve_native_client(
    model: &str,
    client_override: Option<ClientOverride>,
) -> Result<(
    NativeResolvedClient,
    Option<crate::telemetry::TelemetryIdentityScope>,
)> {
    if let Some(client) = crate::credential_mode::disconnected_client(model)? {
        anyhow::ensure!(
            client_override.is_none(),
            "Disconnected policy does not allow client overrides"
        );
        return Ok((
            NativeResolvedClient {
                provider_name: client.provider_name().to_owned(),
                client: Some(client),
                model_route: NativeModelRoute::DirectProvider,
            },
            None,
        ));
    }
    let route = crate::codex_auth::resolve_model_route(model);
    if let Some(override_client) = client_override {
        let identity_scope = if override_client.skips_identity_verification() {
            None
        } else {
            let identity = crate::credential_mode::verified_current_identity_session()?;
            crate::telemetry::TelemetryIdentityScope::new(
                &identity.organization_id,
                identity.workspace_id.as_deref(),
            )
        };
        let client = override_client.client();
        return Ok((
            NativeResolvedClient {
                provider_name: client.provider_name().to_owned(),
                client: Some(client),
                model_route: NativeModelRoute::DirectProvider,
            },
            identity_scope,
        ));
    }

    let (credential_mode, identity) = crate::credential_mode::require_ready_with_identity(model)?;
    let identity_scope = identity.as_ref().and_then(|identity| {
        crate::telemetry::TelemetryIdentityScope::new(
            &identity.organization_id,
            identity.workspace_id.as_deref(),
        )
    });
    if let crate::codex_auth::CodexModelRoute::AppServer { model_id } = route {
        return Ok((
            NativeResolvedClient {
                provider_name: "openai-codex".to_owned(),
                client: None,
                model_route: NativeModelRoute::CodexAppServer { model_id },
            },
            identity_scope,
        ));
    }

    let client = match credential_mode {
        crate::credential_mode::DetectedMode::Platform(session) => {
            let process_env = std::env::vars().collect::<std::collections::HashMap<_, _>>();
            let env = session.managed_env(model, &process_env)?;
            let routed = session.managed_model_route(model);
            UnifiedClient::from_model_with_env(&routed, &env)?
        }
        crate::credential_mode::DetectedMode::Byok => {
            let mut env = std::env::vars().collect::<std::collections::HashMap<_, _>>();
            let _ = crate::codex_auth::merge_codex_auth_snapshot_into_env(
                &mut env,
                crate::codex_auth::read_codex_auth(),
                false,
            );
            let _ = crate::service_connections::ConnectionBroker::merge_default_for_model(
                model, &mut env,
            )?;
            UnifiedClient::from_model_with_env(model, &env)?
        }
    };
    let provider_name = client.provider_name().to_owned();
    Ok((
        NativeResolvedClient {
            client: Some(client),
            provider_name,
            model_route: NativeModelRoute::DirectProvider,
        },
        identity_scope,
    ))
}

fn build_local_host(
    config: &NativeAgentConfig,
    credential_vault: CredentialVault,
    client_override: Option<ClientOverride>,
    subagent_parent_scope_id: Option<String>,
    mailbox_identity: Option<String>,
    telemetry_identity_scope: Arc<RwLock<Option<crate::telemetry::TelemetryIdentityScope>>>,
) -> Result<NativeExecutionHostHandle> {
    let mut executor = ToolExecutor::with_credential_vault(&config.cwd, credential_vault)
        .with_code_authority()
        .with_managed_mcp_policy(config.managed_mcp_policy.clone());
    if let Some(policy) = config.sandbox_policy.clone() {
        executor = executor.with_sandbox_policy(policy);
    }
    if let Some(scope) = subagent_parent_scope_id {
        executor = executor.with_subagent_parent_scope(scope);
    }
    if let Some(identity) = mailbox_identity {
        executor = executor.with_mailbox_identity(identity);
    }

    let mut hooks = IntegratedHookSystem::load_from_config(&config.cwd);
    hooks.set_model(&config.model);
    if client_override
        .as_ref()
        .is_none_or(|override_client| !override_client.skips_identity_verification())
    {
        if let Ok(session) = crate::credential_mode::verified_current_identity_session() {
            hooks.set_identity_context(
                Some(session.organization_id.clone()),
                session.workspace_id.clone(),
            );
            if let (Some(workspace_id), Some(maestro_home)) =
                (session.workspace_id, crate::path_utils::maestro_home_dir())
            {
                hooks.enable_authenticated_session_history(
                    session.organization_id,
                    workspace_id,
                    session.access_token,
                    Some(crate::evalops_cli::authenticated_session_history_endpoint()),
                    maestro_home.join("session-history"),
                );
            }
        }
    }

    let experiment_scope = Arc::clone(&telemetry_identity_scope);
    let resolve_model = move |model: &str, preserve_scope: bool| {
        // An injected client selects only the initial model. Later model
        // changes resolve fresh authorization just as the original actor did.
        let (resolved, identity_scope) = resolve_native_client(model, None)
            .map_err(|error| format!("resolve native model {model}: {error:#}"))?;
        update_model_identity_scope(
            &mut telemetry_identity_scope
                .write()
                .expect("telemetry identity scope lock poisoned"),
            identity_scope,
            preserve_scope,
        )?;
        Ok(resolved)
    };
    Ok(LocalNativeExecutionHost::compose_with_experiments(
        Arc::new(executor),
        hooks,
        resolve_model,
        model_route,
        config
            .model_capabilities
            .map(|caps| (config.model.clone(), caps)),
        Some(experiment_scope),
    ))
}

pub(crate) use maestro_runtime::agent::managed_turn_lineage_id;

/// Observe the same event stream delivered to the caller, preserving the
/// canonical turn accounting that was attached to the actor before extraction.
fn relay_runtime_events(
    runtime_event_rx: tokio::sync::mpsc::UnboundedReceiver<FromAgent>,
    config: &NativeAgentConfig,
    provider_name: String,
    telemetry_identity_scope: Arc<RwLock<Option<crate::telemetry::TelemetryIdentityScope>>>,
    telemetry_host: Option<maestro_runtime::agent::NativeExecutionHostHandle>,
) -> tokio::sync::mpsc::UnboundedReceiver<FromAgent> {
    let (consumer_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let initial_identity_scope = telemetry_identity_scope
        .read()
        .expect("telemetry identity scope lock poisoned")
        .clone();
    let telemetry = crate::telemetry::TurnTelemetryRelay::spawn(
        crate::telemetry::TurnTrackerConfig {
            // A runtime run ID is not a captured conversation ID. Missing
            // session context stays unattributed in the cloud projection.
            session_id: String::new(),
            sampling_config: crate::telemetry::TailSamplingConfig::from_env(),
        },
        crate::telemetry::TurnTrackerContext {
            model: Some(crate::telemetry::ModelInfo {
                id: config.model.clone(),
                provider: provider_name,
                thinking_level: crate::telemetry::ThinkingLevel::Off,
            }),
            sandbox_mode: if config.sandbox_policy.is_some() {
                crate::telemetry::SandboxMode::Local
            } else {
                crate::telemetry::SandboxMode::None
            },
            approval_mode: match config.approval_mode {
                ApprovalMode::Yolo => crate::telemetry::ApprovalMode::Auto,
                ApprovalMode::Selective | ApprovalMode::Safe => {
                    crate::telemetry::ApprovalMode::Prompt
                }
            },
            mcp_servers: Vec::new(),
            context_source_count: 0,
            features: crate::telemetry::FeatureFlags {
                safe_mode: config.approval_mode != ApprovalMode::Yolo,
                ..Default::default()
            },
            identity_scope: initial_identity_scope,
        },
        telemetry_identity_scope,
        telemetry_host,
    );
    relay_runtime_events_with_telemetry(runtime_event_rx, telemetry, consumer_event_tx);
    event_rx
}

fn relay_runtime_events_with_telemetry(
    mut runtime_event_rx: tokio::sync::mpsc::UnboundedReceiver<FromAgent>,
    mut telemetry: crate::telemetry::TurnTelemetryRelay,
    consumer_event_tx: tokio::sync::mpsc::UnboundedSender<FromAgent>,
) {
    tokio::spawn(async move {
        while let Some(event) = runtime_event_rx.recv().await {
            let observation = telemetry.project(&event);
            // A detached consumer does not end the actor's turn. Keep draining
            // until the runtime exits so terminal telemetry is still recorded.
            let _ = consumer_event_tx.send(event);
            telemetry.submit(observation);
        }
    });
}

/// Bind a model transition before publishing its identity to turn telemetry.
/// Automatic routing cannot replace the actor's active tenant. Direct-provider
/// BYOK may have no tenant; staying unbound is not a tenant change. Explicit
/// model changes refresh the scope.
fn update_model_identity_scope(
    active: &mut Option<crate::telemetry::TelemetryIdentityScope>,
    resolved: Option<crate::telemetry::TelemetryIdentityScope>,
    preserve_scope: bool,
) -> Result<(), String> {
    if preserve_scope && resolved != *active {
        return Err(
            "Automatic routing cannot change the active organization or workspace".to_owned(),
        );
    }
    *active = resolved;
    Ok(())
}

#[cfg(test)]
pub(crate) use self::native_host::LocalNativeExecutionHost as TestLocalNativeExecutionHost;

#[cfg(test)]
mod identity_transition_tests {
    use super::update_model_identity_scope;
    use crate::telemetry::TelemetryIdentityScope;

    #[tokio::test]
    async fn detached_consumer_keeps_runtime_event_accounting_alive() {
        let (runtime_tx, runtime_rx) = tokio::sync::mpsc::unbounded_channel();
        let consumer = super::relay_runtime_events(
            runtime_rx,
            &super::NativeAgentConfig::default(),
            "test".to_owned(),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            None,
        );
        drop(consumer);
        runtime_tx
            .send(super::FromAgent::Ready {
                model: "test".to_owned(),
                provider: "test".to_owned(),
            })
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), runtime_tx.closed())
                .await
                .is_err(),
            "consumer closure must not drop the actor's telemetry stream"
        );
        runtime_tx
            .send(super::FromAgent::Ready {
                model: "test".to_owned(),
                provider: "test".to_owned(),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn saturated_telemetry_never_delays_runtime_event_delivery() {
        let (runtime_tx, runtime_rx) = tokio::sync::mpsc::unbounded_channel();
        let (consumer_tx, mut consumer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (telemetry, _blocked_receiver) =
            crate::telemetry::TurnTelemetryRelay::saturated_for_test();
        super::relay_runtime_events_with_telemetry(runtime_rx, telemetry, consumer_tx);

        runtime_tx
            .send(super::FromAgent::ResponseStart {
                response_id: "response".to_owned(),
            })
            .unwrap();

        let delivered =
            tokio::time::timeout(std::time::Duration::from_millis(100), consumer_rx.recv())
                .await
                .expect("a full telemetry queue must not delay the consumer")
                .expect("runtime event relay closed unexpectedly");
        assert!(matches!(delivered, super::FromAgent::ResponseStart { .. }));
    }

    fn scope(org: &str, workspace: &str) -> Option<TelemetryIdentityScope> {
        TelemetryIdentityScope::new(org, Some(workspace))
    }

    #[test]
    fn automatic_resolution_rejects_tenant_changes_without_changing_active_scope() {
        let initial = scope("org-a", "workspace-a");
        for candidate in [
            scope("org-b", "workspace-a"),
            scope("org-a", "workspace-b"),
            None,
        ] {
            let mut active = initial.clone();
            assert_eq!(
                update_model_identity_scope(&mut active, candidate, true).unwrap_err(),
                "Automatic routing cannot change the active organization or workspace"
            );
            assert_eq!(active, initial);
        }
        let mut unbound = None;
        update_model_identity_scope(&mut unbound, None, true).unwrap();
        assert!(unbound.is_none());
        let mut still_unbound = None;
        assert_eq!(
            update_model_identity_scope(&mut still_unbound, scope("org-a", "workspace-a"), true)
                .unwrap_err(),
            "Automatic routing cannot change the active organization or workspace"
        );
        assert!(still_unbound.is_none());
    }

    #[test]
    fn automatic_resolution_accepts_the_complete_existing_tenant() {
        let mut active = scope("org-a", "workspace-a");
        let candidate = active.clone();
        update_model_identity_scope(&mut active, candidate.clone(), true).unwrap();
        assert_eq!(active, candidate);
    }

    #[test]
    fn explicit_resolution_refreshes_identity_scope() {
        let mut active = scope("org-a", "workspace-a");
        let candidate = scope("org-b", "workspace-b");
        update_model_identity_scope(&mut active, candidate.clone(), false).unwrap();
        assert_eq!(active, candidate);
        update_model_identity_scope(&mut active, None, false).unwrap();
        assert!(active.is_none());
    }
}
