//! Native headless **server** — Rust agent speaks the headless protocol on stdio.
//!
//! Replaces the TypeScript `runHeadlessMode` agent path. Clients (including the
//! native TUI headless client, IDE bridges, and tests) send `ToAgentMessage`
//! lines on stdin and receive `FromAgentMessage` lines on stdout.
//!
//! The provider client is created before the first `Ready` message so clients
//! never admit a runtime whose configured model or credential is invalid.
//!
//! ## Tool execution ownership
//!
//! Tools that do **not** require approval are auto-executed by the native agent
//! loop, which emits `ToolStart` / `ToolOutput` / `ToolEnd` through the event
//! bridge. Headless must **not** re-execute those tools (that would double-run
//! side effects and drop streaming `tool_output`).
//!
//! Tools that **do** require approval are resolved by:
//! - `ApprovalMode::Auto` → approve and let the native agent execute
//! - `ApprovalMode::Fail` → deny immediately
//! - `ApprovalMode::Prompt` / unset → wait for client `ToolResponse`

use maestro_runtime_contracts::tool_wire;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(feature = "test-support")]
use std::sync::OnceLock;

use anyhow::{Context, Result};
use base64::Engine as _;
use maestro_runtime::{TelemetryConfig, TelemetryGuard};
use ring::signature::{ED25519, UnparsedPublicKey};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::agent::protocol::ExecutionReceipt;
use crate::agent::{
    CredentialVault, ExecutionSource, FromAgent, ManagedInferenceAuthorization, MaxTokensSource,
    NativeAgent, NativeAgentConfig, PromptKind, ToolDefinition, ToolResponseConsumption,
    ToolResponseMessage, ToolResult, managed_turn_lineage_id,
};
use crate::git;
use crate::headless::controller_binding::{
    ControllerBindingReceipt, ControllerScopeExpectation, controller_binding_from_hello_json,
};
use crate::headless::messages::{
    ApprovalMode, ClientToolExecutionOwner, ClientToolResultContent, CodeMode,
    ExternalToolDefinition, FromAgentMessage, GovernedToolGrant, HeadlessErrorType,
    PromptExperimentArm, PromptExperimentAssignment, PromptExperimentExposure,
    ServerRequestResolutionStatus, ServerRequestResolvedBy, ServerRequestType, ToAgentMessage,
    TokenUsage as HeadlessTokenUsage, ToolResult as HeadlessToolResult, ToolRetryDecisionAction,
    UtilityCommandShellMode, UtilityCommandStream, UtilityCommandTerminalMode,
    UtilityFileSearchMatch,
};
use crate::headless::{HEADLESS_PROTOCOL_VERSION, native_server_capabilities};

/// Test-only provider override for the local SDK conformance fixture.
///
/// The fixture is an executable built with `test-support`; it enters the
/// production `run_headless_server` loop and therefore uses the same stdio
/// parser and event bridge as `maestro --headless`. This value has no
/// production environment switch and cannot be installed in a shipped build.
#[cfg(feature = "test-support")]
static LOCAL_SDK_CONFORMANCE_CLIENT: OnceLock<crate::ai::UnifiedClient> = OnceLock::new();

/// Run the local SDK protocol fixture with a deterministic provider.
///
/// This is deliberately available only with `test-support`. The fixed script
/// covers a native approval-gated `bash` call, a completed follow-up response,
/// and a pending response that the client cancels. It never resolves a network
/// credential or changes the production headless command surface.
#[cfg(feature = "test-support")]
pub async fn run_scripted_local_sdk_conformance_server() -> Result<i32> {
    let client = crate::ai::UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "local-sdk-conformance",
        vec![
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::ToolUse {
                    id: "local-sdk-tool-call".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({
                        "command": "printf local-sdk-native-tool"
                    }),
                }],
                stop_reason: crate::ai::StopReason::ToolUse,
                error: None,
            },
            crate::ai::ScriptedResponse::text("LOCAL_SDK_CONFORMANCE_COMPLETED"),
            crate::ai::ScriptedResponse {
                blocks: vec![crate::ai::ScriptedBlock::Pending],
                stop_reason: crate::ai::StopReason::EndTurn,
                error: None,
            },
        ],
    ));
    LOCAL_SDK_CONFORMANCE_CLIENT
        .set(client)
        .map_err(|_| anyhow::anyhow!("local SDK conformance provider already installed"))?;
    run_headless_server(Some("gpt-4o".to_string())).await
}

/// Shared headless runtime metadata updated from Init / SessionInfo.
#[derive(Debug, Default, Clone)]
struct RuntimeMeta {
    session_id: Option<String>,
    approval_mode: Option<ApprovalMode>,
    /// Controller-owned execution ids awaiting a native terminal event.
    tool_execution_ids: HashMap<String, String>,
    /// Governed executions for which this connection already accepted a decision.
    decided_tool_execution_ids: HashSet<String>,
    /// Tool call ids currently awaiting a raw client decision.
    pending_tool_calls: HashSet<String>,
    client_tool_bindings: HashMap<String, ClientToolBinding>,
    pending_client_tools: HashMap<String, PendingClientTool>,
    emitted_client_tool_terminals: HashSet<String>,
    conversation_snapshot: Option<Vec<maestro_ai::Message>>,
    process_budget: Option<Arc<Mutex<crate::agent::process_budget::ProcessBudgetState>>>,
    turn_active: bool,
    transcript_grade: crate::transcript::TranscriptGrade,
    response_chunks: Vec<(String, bool)>,
    /// Last safe managed-Gateway evidence for the active turn.
    managed_gateway_receipt: Option<maestro_ai::ManagedGatewayReceipt>,
    /// Controller assignment proven against the provider-bound system prompt.
    prompt_experiment: Option<PromptExperimentExposure>,
    /// Detached consumption-receipt acknowledgement tasks. Shutdown drains
    /// these so a dropped receipt's protocol error and rollback are emitted
    /// before the process exits. Shared behind an `Arc` because `RuntimeMeta`
    /// is `Clone` and every clone must observe the same registry.
    receipt_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

#[derive(Debug, Clone)]
struct ClientToolBinding {
    provider_tool_name: String,
    tool_id: String,
    connection_binding_id: Option<String>,
    logical_name: String,
    owner: ClientToolExecutionOwner,
    grant_id: String,
    grant_version: u64,
    grant_hash: String,
    turn_digest: String,
    definition_digest: String,
    expires_at_ms: i64,
}

#[derive(Debug, Clone)]
struct PendingClientTool {
    binding: ClientToolBinding,
    tool_execution_id: String,
    args_digest: String,
    idempotency_key: String,
    result_digest: Option<String>,
}

impl RuntimeMeta {
    fn reserve_tool_decision(&mut self, tool_execution_id: Option<&str>) -> bool {
        let Some(tool_execution_id) = tool_execution_id else {
            // Legacy, ungoverned clients have no durable id to deduplicate.
            return true;
        };
        self.decided_tool_execution_ids
            .insert(tool_execution_id.to_string())
    }

    fn record_response_chunk(
        &mut self,
        content: &str,
        is_thinking: bool,
    ) -> crate::transcript::TranscriptGrade {
        let grade = self.transcript_grade;
        if grade != crate::transcript::TranscriptGrade::Delta {
            self.response_chunks
                .push((content.to_string(), is_thinking));
        }
        grade
    }
}

struct HeadlessState {
    model: String,
    model_capabilities: maestro_runtime::agent::NativeModelCapabilities,
    cwd: String,
    system_prompt: String,
    thinking_enabled: bool,
    thinking_budget: u32,
    credential_vault: CredentialVault,
    /// Seeded conversation history applied on agent creation / init.
    history: Option<Vec<crate::headless::messages::HistoryMessage>>,
    /// Semantic provider history is meaningful only after an Init boundary.
    init_applied: bool,
    governed_grant: Option<GovernedToolGrant>,
    controller_binding_sha256: Option<String>,
    controller_binding: Option<ControllerBindingReceipt>,
    workspace_capabilities: crate::headless::workspace_capabilities::WorkspaceCapabilityActivation,
    next_prompt_queue_id: u64,
    ready_emitted: bool,
    meta: Arc<Mutex<RuntimeMeta>>,
    agent: Option<NativeAgent>,
    tool_tx: Option<mpsc::UnboundedSender<ToolResponseMessage>>,
    event_task: Option<tokio::task::JoinHandle<()>>,
    utility_commands: HashMap<String, mpsc::UnboundedSender<UtilityCommandControl>>,
    file_watches: HashMap<String, tokio::task::JoinHandle<()>>,
}

enum UtilityCommandControl {
    Terminate,
    Stdin { content: String, eof: bool },
}

struct UtilityCommandOptions {
    command_id: String,
    command: String,
    cwd: String,
    env: Option<HashMap<String, String>>,
    shell_mode: UtilityCommandShellMode,
    terminal_mode: UtilityCommandTerminalMode,
    allow_stdin: bool,
    columns: Option<u32>,
    rows: Option<u32>,
}

impl HeadlessState {
    fn new(model_override: Option<String>) -> Self {
        let model = resolve_headless_model(model_override, &std::env::vars().collect());
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let system_prompt = format!(
            "You are Deixic Code, an AI coding assistant. Working directory: {cwd}. Be concise and use tools when helpful."
        );
        let session_id = env_session_id();
        Self {
            model_capabilities: crate::agent::catalog_model_capabilities(&model),
            model,
            cwd,
            system_prompt: system_prompt.clone(),
            thinking_enabled: false,
            thinking_budget: 10_000,
            credential_vault: CredentialVault::new(),
            history: None,
            init_applied: false,
            governed_grant: None,
            controller_binding_sha256: None,
            controller_binding: None,
            workspace_capabilities:
                crate::headless::workspace_capabilities::WorkspaceCapabilityActivation::new(
                    system_prompt.clone(),
                ),
            next_prompt_queue_id: 1,
            ready_emitted: false,
            meta: Arc::new(Mutex::new(RuntimeMeta {
                session_id,
                approval_mode: None,
                tool_execution_ids: HashMap::new(),
                decided_tool_execution_ids: HashSet::new(),
                pending_tool_calls: HashSet::new(),
                client_tool_bindings: HashMap::new(),
                pending_client_tools: HashMap::new(),
                emitted_client_tool_terminals: HashSet::new(),
                conversation_snapshot: None,
                process_budget: None,
                turn_active: false,
                transcript_grade: crate::transcript::TranscriptGrade::Delta,
                response_chunks: Vec::new(),
                managed_gateway_receipt: None,
                prompt_experiment: None,
                receipt_tasks: Arc::new(Mutex::new(Vec::new())),
            })),
            agent: None,
            tool_tx: None,
            event_task: None,
            utility_commands: HashMap::new(),
            file_watches: HashMap::new(),
        }
    }

    fn session_id(&self) -> Option<String> {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id
            .clone()
    }

    fn set_approval_mode(&self, mode: Option<ApprovalMode>) {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .approval_mode = mode;
    }

    fn ensure_session_id(&self) -> String {
        let mut meta = self
            .meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(id) = meta.session_id.clone().filter(|s| !s.is_empty()) {
            return id;
        }
        let id = uuid::Uuid::new_v4().to_string();
        meta.session_id = Some(id.clone());
        id
    }

    fn accept_controller_binding(
        &mut self,
        binding: Option<&ControllerBindingReceipt>,
    ) -> Result<()> {
        let next = binding.map(|binding| binding.binding_sha256.as_str());
        match (self.controller_binding_sha256.as_deref(), next) {
            (None, None) => Ok(()),
            (Some(existing), Some(next)) if existing == next => Ok(()),
            (Some(_), _) => anyhow::bail!("controller binding cannot be removed or replaced"),
            (None, Some(_)) if self.init_applied => {
                anyhow::bail!("controller binding must be accepted before init")
            }
            (None, Some(next)) => {
                self.controller_binding_sha256 = Some(next.to_string());
                self.controller_binding = binding.cloned();
                Ok(())
            }
        }
    }

    async fn apply_workspace_capability_set(
        &mut self,
        request: crate::headless::workspace_capabilities::ApplyWorkspaceCapabilitySet,
    ) -> Result<crate::headless::workspace_capabilities::WorkspaceCapabilitySetApplied> {
        let binding = self
            .controller_binding
            .as_ref()
            .context("workspace prompt capabilities require an accepted controller binding")?
            .clone();
        let runner_session_id = required_governed_runtime_env("MAESTRO_RUNNER_SESSION_ID")?;
        let turn_active = self
            .meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .turn_active;
        let mut prepared = self
            .workspace_capabilities
            .prepare(
                request,
                &binding,
                &binding.controller_context,
                &runner_session_id,
            )
            .map_err(anyhow::Error::from)?;
        prepared.bind_provider_prompt(&crate::agent::provider_system_prompt(
            prepared.prompt(),
            &self.model,
            self.model_capabilities,
        ));
        if turn_active || prepared.is_idempotent() {
            return Ok(self
                .workspace_capabilities
                .commit(prepared, &binding, turn_active));
        }

        let uses_app_server =
            crate::agent::codex_app_server_turns::model_should_use_app_server_turns(&self.model);
        let next_prompt = prepared.prompt().to_string();
        if uses_app_server {
            // Retire the old provider thread before committing the generation.
            // Any failure leaves activation state untouched, so an identical
            // retry must traverse rotation and installation again.
            self.rotate_codex_agent_for_prompt_change().await?;
        }
        self.system_prompt = next_prompt;
        if uses_app_server {
            self.ensure_agent()?;
            self.agent
                .as_ref()
                .context("provider prompt install requires a native agent")?
                .ensure_provider_prompt_installed()
                .await?;
        } else if let Some(agent) = self.agent.as_ref() {
            agent.set_system_prompt(self.system_prompt.clone())?;
        }
        Ok(self
            .workspace_capabilities
            .commit(prepared, &binding, false))
    }

    async fn rotate_codex_agent_for_prompt_change(&mut self) -> Result<()> {
        let session_id = self.session_id();
        crate::agent::codex_app_server_turns::retire_persistent_thread_for_prompt_change(
            &self.model,
            &self.cwd,
            session_id.as_deref(),
        )?;
        if let Some(agent) = self.agent.take() {
            agent.shutdown().await;
        }
        if let Some(task) = self.event_task.take() {
            let _ = task.await;
        }
        self.tool_tx = None;
        Ok(())
    }

    fn ensure_agent(&mut self) -> Result<&NativeAgent> {
        if self.agent.is_none() {
            let started = Instant::now();
            let config = NativeAgentConfig {
                model_dynamics: headless_model_dynamics(crate::config::model_dynamics_config()),
                model: self.model.clone(),
                model_capabilities: Some(self.model_capabilities),
                max_tokens: crate::model_catalog::default_max_output_tokens(&self.model),
                max_tokens_source: MaxTokensSource::Catalog,
                system_prompt: Some(self.system_prompt.clone()),
                thinking_enabled: self.thinking_enabled,
                thinking_budget: self.thinking_budget,
                cwd: self.cwd.clone(),
                // The headless protocol's own `ApprovalMode` (Auto/Fail/Prompt,
                // imported above) only resolves calls the runner already
                // marked `requires_approval`; preserve the prior (mode-unaware)
                // per-tool heuristic here exactly so that decision is unchanged.
                approval_mode: crate::state::ApprovalMode::Selective,
                context_window: None,
                // Headless has no sandbox-policy resolution today (unlike the interactive TUI's
                // `config::resolve_interactive_sandbox_policy` or print
                // mode's `PrintModeOptions::sandbox_policy`); preserve that
                // status quo explicitly rather than silently expanding this
                // PR's scope to headless sandboxing.
                sandbox_policy: None,
                managed_mcp_policy: None,
                max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
                allow_unbounded_turn: false,
                retry_config: crate::agent::retry::RetryConfig::hosted_outage(),
                external_tool_schema_policy: crate::agent::ExternalToolSchemaPolicy::Eager,
            };
            let (agent, mut event_rx) = if let Some(grant) = self.governed_grant.as_ref() {
                let (allowed_tools, external_tools, bindings) = governed_agent_inputs(grant)?;
                let created = NativeAgent::new_with_governed_tools_and_credential_vault(
                    config,
                    &allowed_tools,
                    external_tools,
                    self.credential_vault.clone(),
                )
                .context("Failed to create governed native agent for headless server")?;
                self.meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .client_tool_bindings = bindings;
                created
            } else {
                self.meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .client_tool_bindings
                    .clear();
                #[cfg(feature = "test-support")]
                if let Some(client) = LOCAL_SDK_CONFORMANCE_CLIENT.get().cloned() {
                    NativeAgent::new_with_test_client(config, client).context(
                        "Failed to create scripted native agent for local SDK conformance",
                    )?
                } else {
                    NativeAgent::new_with_credential_vault(config, self.credential_vault.clone())
                        .context("Failed to create native agent for headless server")?
                }
                #[cfg(not(feature = "test-support"))]
                NativeAgent::new_with_credential_vault(config, self.credential_vault.clone())
                    .context("Failed to create native agent for headless server")?
            };
            // Apply any seeded multi-turn history before the first prompt.
            let conversation_snapshot = self
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .conversation_snapshot
                .clone();
            if let Some(messages) = conversation_snapshot {
                agent.replace_history_preserving_credentials(messages);
            } else if let Some(history) = self.history.as_deref() {
                let messages = crate::headless::messages::history_to_ai_messages(Some(history));
                if !messages.is_empty() {
                    agent.replace_history(messages);
                }
            }
            let tool_tx = agent.tool_response_sender();
            let tool_tx_bg = tool_tx.clone();
            let meta_bg = Arc::clone(&self.meta);
            let reported_model = self.model.clone();
            let routed_provider = managed_provider_override();

            // Hold native events until the validated Ready boundary has been
            // written. In particular, SessionInfo must never race ahead of
            // the first protocol event.
            let session_id = self.ensure_session_id();
            let git_branch = git::current_branch(Path::new(&self.cwd));
            let (model, provider) = reported_identity(
                &self.model,
                infer_provider_label(&self.model),
                managed_provider_override().as_deref(),
            );
            tracing::info!(
                target: "maestro.model_binding",
                event = "maestro_model_binding_ready",
                session_id = %session_id,
                configured_model = %self.model,
                configured_provider = infer_provider_label(&self.model),
                reported_model = %model,
                reported_provider = %provider,
                routed_provider = managed_provider_override().as_deref().unwrap_or(""),
                binding_mode = model_binding_mode(&self.model),
                duration_ms = started.elapsed().as_millis() as u64,
            );
            let emit_ready = !self.ready_emitted;
            if emit_ready {
                emit(&FromAgentMessage::Ready {
                    protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                    model,
                    provider,
                    session_id: Some(session_id.clone()),
                })?;
            }
            // This is a protocol identity, not a SessionManager transcript
            // with an owner that deletes tool-output spills.
            agent.set_subagent_parent_scope(format!("session:{session_id}"))?;
            agent.set_session_context(Some(session_id.clone()), "headless", false)?;
            if emit_ready {
                agent.send_session_info(&self.cwd, self.session_id(), git_branch);
                self.ready_emitted = true;
            }
            let event_task = tokio::spawn(async move {
                while let Some(msg) = event_rx.recv().await {
                    if let Err(err) = handle_agent_event(
                        msg,
                        &meta_bg,
                        &tool_tx_bg,
                        &reported_model,
                        routed_provider.as_deref(),
                    )
                    .await
                    {
                        let _ = emit(&FromAgentMessage::Error {
                            request_id: None,
                            message: format!("headless event bridge failed: {err:#}"),
                            fatal: false,
                            terminal: true,
                            error_type: Some(HeadlessErrorType::Protocol),
                        });
                    }
                }
            });
            self.tool_tx = Some(tool_tx);
            self.event_task = Some(event_task);
            self.agent = Some(agent);
        }
        Ok(self.agent.as_ref().expect("agent just created"))
    }

    fn agent_mut(&mut self) -> Result<&NativeAgent> {
        self.ensure_agent()
    }

    async fn apply_governed_grant(
        &mut self,
        code_mode: Option<CodeMode>,
        grant: Option<GovernedToolGrant>,
    ) -> Result<()> {
        match (code_mode, grant) {
            (None, None) => Ok(()),
            (None, Some(_)) => anyhow::bail!("tool_grant requires code_mode=governed_code"),
            (Some(CodeMode::GovernedCode), None) => {
                anyhow::bail!("governed_code requires an authenticated tool_grant")
            }
            (Some(CodeMode::GovernedCode), Some(grant)) => {
                let organization_id = required_governed_runtime_env("MAESTRO_ORGANIZATION_ID")?;
                let workspace_id = required_governed_runtime_env("MAESTRO_WORKSPACE_ID")?;
                let thread_id = required_governed_runtime_env("MAESTRO_RUNNER_SESSION_ID")?;
                let runtime_generation =
                    required_governed_runtime_env("MAESTRO_PLACEMENT_GENERATION")?
                        .parse::<u64>()
                        .context("MAESTRO_PLACEMENT_GENERATION must be an unsigned integer")?;
                let context = GovernedGrantVerificationContext {
                    organization_id: &organization_id,
                    workspace_id: &workspace_id,
                    thread_id: &thread_id,
                    turn_id: &grant.turn_id,
                    run_id: &grant.run_id,
                    runtime_generation,
                };
                verify_governed_tool_grant(
                    &grant,
                    &context,
                    chrono::Utc::now().timestamp_millis(),
                )?;

                self.apply_verified_governed_grant(grant).await
            }
        }
    }

    /// Apply authority only after the runtime-owned context and signature have been verified.
    async fn apply_verified_governed_grant(&mut self, grant: GovernedToolGrant) -> Result<()> {
        if let Some(existing) = self.governed_grant.as_ref() {
            if existing == &grant {
                return Ok(());
            }
            if existing.identity() == grant.identity() {
                anyhow::bail!("governed tool grant identity reused with different content");
            }
        }
        let handled_active_turn = {
            let mut meta = self
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !meta.pending_tool_calls.is_empty() || !meta.pending_client_tools.is_empty() {
                anyhow::bail!("cannot change governed tool grant during an active turn");
            }
            if meta.turn_active {
                let existing = self
                    .governed_grant
                    .as_ref()
                    .context("active governed turn is missing its prior grant")?;
                if governed_authority_material_digest(existing)?
                    != governed_authority_material_digest(&grant)?
                {
                    anyhow::bail!("a steer grant cannot change the active run's tool capabilities");
                }
                let (_, _, bindings) = governed_agent_inputs(&grant)?;
                meta.client_tool_bindings = bindings;
                true
            } else {
                false
            }
        };
        if handled_active_turn {
            self.governed_grant = Some(grant);
            return Ok(());
        }

        let (allowed_tools, external_tools, bindings) = governed_agent_inputs(&grant)?;
        let previous_event = self
            .governed_grant
            .as_ref()
            .and_then(|grant| grant.process_budget.as_ref())
            .map(|budget| budget.event_id.as_str());
        let next_event = grant
            .process_budget
            .as_ref()
            .map(|budget| budget.event_id.as_str());
        if previous_event.is_some() && previous_event != next_event {
            let prompt = self
                .workspace_capabilities
                .prompt_for_next_turn()
                .to_string();
            if let Some(agent) = self.agent.as_ref() {
                agent.clear_process_budget(prompt.clone()).await?;
            }
            self.meta
                .lock()
                .map_err(|_| anyhow::anyhow!("runtime metadata poisoned"))?
                .process_budget = None;
            self.system_prompt = prompt;
        }
        if let Some(agent) = self.agent.as_ref() {
            agent.replace_governed_tools(allowed_tools, external_tools)?;
            self.meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .client_tool_bindings = bindings;
        }
        self.governed_grant = Some(grant);
        self.ensure_agent()?;
        Ok(())
    }
}

fn required_governed_runtime_env(name: &str) -> Result<String> {
    std::env::var(name)
        .with_context(|| format!("governed code requires runtime-owned {name}"))
        .and_then(|value| {
            let value = value.trim();
            if value.is_empty() {
                anyhow::bail!("governed code requires non-empty runtime-owned {name}");
            }
            Ok(value.to_string())
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn canonical_json_digest(value: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(value).context("serialize governed tool material")?;
    Ok(format!("sha256:{}", sha256_hex(&bytes)))
}

fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn is_plain_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_authorization_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("authz_fingerprint_v1_")
        .is_some_and(is_plain_sha256_digest)
}

fn is_normalized_authority_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn governed_authority_material_digest(grant: &GovernedToolGrant) -> Result<String> {
    let mut value = serde_json::json!({
        "native_tool_ids": grant.native_tool_ids,
        "external_tools": grant.external_tools,
    });
    if let Some(budget) = &grant.process_budget {
        value["process_budget"] = serde_json::json!(budget);
        value["process_system_prompt"] = serde_json::json!(grant.process_system_prompt);
    }
    if !grant.connection_bindings.is_empty() {
        value["connection_bindings"] = serde_json::json!(grant.connection_bindings);
    }
    canonical_json_digest(&value)
}

fn external_definition_digest(definition: &ExternalToolDefinition) -> Result<String> {
    let mut value = serde_json::json!({
        "tool_id": definition.tool_id,
        "name": definition.name,
        "description": definition.description,
        "input_schema": definition.input_schema,
        "execution_owner": definition.execution_owner,
        "metadata": definition.metadata,
    });
    if let Some(binding_id) = &definition.connection_binding_id {
        value["connection_binding_id"] = serde_json::json!(binding_id);
    }
    canonical_json_digest(&value)
}

fn qualified_client_tool_name(definition: &ExternalToolDefinition, digest: &str) -> String {
    let identity = format!(
        "{}\0{}\0{}",
        definition.execution_owner.client_instance_id, definition.tool_id, digest
    );
    format!("client_{}", &sha256_hex(identity.as_bytes())[..40])
}

type GovernedAgentInputs = (
    HashSet<String>,
    Vec<ToolDefinition>,
    HashMap<String, ClientToolBinding>,
);

fn governed_agent_inputs(grant: &GovernedToolGrant) -> Result<GovernedAgentInputs> {
    validate_governed_grant_shape(grant)?;
    let allowed_tools = grant
        .native_tool_ids
        .iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut provider_names = HashSet::new();
    let turn_digest = canonical_json_digest(&serde_json::json!({
        "organization_id": grant.organization_id,
        "workspace_id": grant.workspace_id,
        "thread_id": grant.thread_id,
        "turn_id": grant.turn_id,
        "run_id": grant.run_id,
        "runtime_generation": grant.runtime_generation,
        "identity_authorization": grant.identity_authorization,
    }))?;
    let mut external_tools = Vec::with_capacity(grant.external_tools.len());
    let mut bindings = HashMap::new();
    for definition in &grant.external_tools {
        let definition_digest = external_definition_digest(definition)?;
        let provider_tool_name = qualified_client_tool_name(definition, &definition_digest);
        if !provider_names.insert(provider_tool_name.clone()) {
            anyhow::bail!("governed client tool identity collision");
        }
        external_tools.push(ToolDefinition {
            tool: crate::ai::Tool::new(
                &provider_tool_name,
                format!(
                    "Caller-owned tool `{}`. {}",
                    definition.name, definition.description
                ),
            )
            .with_schema(definition.input_schema.clone()),
            requires_approval: true,
        });
        bindings.insert(
            provider_tool_name.clone(),
            ClientToolBinding {
                provider_tool_name,
                tool_id: definition.tool_id.clone(),
                connection_binding_id: definition.connection_binding_id.clone(),
                logical_name: definition.name.clone(),
                owner: definition.execution_owner.clone(),
                grant_id: grant.grant_id.clone(),
                grant_version: grant.grant_version,
                grant_hash: grant.grant_hash.clone(),
                turn_digest: turn_digest.clone(),
                definition_digest,
                expires_at_ms: grant.expires_at_ms,
            },
        );
    }
    Ok((allowed_tools, external_tools, bindings))
}

fn validate_governed_grant_shape(grant: &GovernedToolGrant) -> Result<()> {
    if grant.envelope_version != 2 {
        anyhow::bail!("unsupported governed tool grant envelope version");
    }
    for (field, value) in [
        ("grant_id", grant.grant_id.as_str()),
        ("issuer", grant.issuer.as_str()),
        ("audience", grant.audience.as_str()),
        ("organization_id", grant.organization_id.as_str()),
        ("workspace_id", grant.workspace_id.as_str()),
        ("thread_id", grant.thread_id.as_str()),
        ("turn_id", grant.turn_id.as_str()),
        ("run_id", grant.run_id.as_str()),
        ("grant_hash", grant.grant_hash.as_str()),
        ("signing_key_id", grant.signing_key_id.as_str()),
        ("grant_signature", grant.grant_signature.as_str()),
    ] {
        if value.trim().is_empty() {
            anyhow::bail!("governed tool grant {field} must not be empty");
        }
    }
    if grant.grant_version == 0 || grant.grant_epoch == 0 || grant.runtime_generation == 0 {
        anyhow::bail!("governed tool grant versions and epochs must be positive");
    }
    if grant.not_before_ms > grant.expires_at_ms || grant.issued_at_ms > grant.expires_at_ms {
        anyhow::bail!("governed tool grant validity window is invalid");
    }
    let identity = grant
        .identity_authorization
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("governed tool grant has no Identity authorization"))?;
    if identity.schema_version != "identity.tool_authorization.v1"
        || identity.organization_id != grant.organization_id
        || identity.workspace_id != grant.workspace_id
        || identity.application_id != "deixic"
        || identity.audience != GOVERNED_GRANT_AUDIENCE
        || identity.subject_id.trim().is_empty()
        || identity.decision_id.trim().is_empty()
        || identity.authorization_lineage_id.trim().is_empty()
        || identity.policy_id.trim().is_empty()
        || identity.policy_version.trim().is_empty()
        || identity.revocation_epoch != grant.grant_epoch
        || identity.expires_at_ms > grant.expires_at_ms
        || identity.issued_at_ms > grant.issued_at_ms
        || !is_sha256_digest(&identity.actor_chain_digest)
        || !is_plain_sha256_digest(&identity.policy_digest)
        || !is_authorization_fingerprint(&identity.authorization_fingerprint)
        || !is_sha256_digest(&identity.capability_digest)
        || !is_sha256_digest(&identity.action_digest)
    {
        anyhow::bail!("governed tool grant Identity authorization is invalid");
    }
    let mut native_ids = HashSet::new();
    let mut previous_native_id: Option<&str> = None;
    for name in &grant.native_tool_ids {
        let normalized = name.trim().to_ascii_lowercase();
        if normalized.is_empty()
            || normalized != *name
            || previous_native_id.is_some_and(|previous| previous >= name.as_str())
            || !native_ids.insert(normalized)
        {
            anyhow::bail!("governed native tool ids must be normalized, sorted, and unique");
        }
        previous_native_id = Some(name);
    }
    let mut owner_scoped_ids = HashSet::new();
    let mut connection_binding_ids = HashSet::new();
    let mut previous_connection_binding_id: Option<&str> = None;
    if grant.connection_bindings.len() > 64 {
        anyhow::bail!("governed connection binding count exceeds size limits");
    }
    for binding in &grant.connection_bindings {
        if binding.binding_id.trim().is_empty()
            || binding.connection_id.trim().is_empty()
            || binding.provider_id.trim().is_empty()
            || binding.policy_hash.trim().is_empty()
            || !is_normalized_authority_id(&binding.binding_id)
            || !is_normalized_authority_id(&binding.connection_id)
            || !is_normalized_authority_id(&binding.provider_id)
            || !is_sha256_digest(&binding.policy_hash)
            || binding.generation == 0
            || binding.capabilities.is_empty()
            || binding.capabilities.len() > 128
            || binding.resources.len() > 256
            || binding.binding_id.len() > 256
            || binding.connection_id.len() > 256
            || binding.provider_id.len() > 128
            || binding.policy_hash.len() > 256
        {
            anyhow::bail!("governed connection binding identity and authority must be present");
        }
        let mut previous_resource: Option<&str> = None;
        for resource in &binding.resources {
            if resource.trim().is_empty()
                || resource.len() > 2 * 1024
                || previous_resource.is_some_and(|previous| previous >= resource.as_str())
            {
                anyhow::bail!("governed connection resources must be sorted and unique");
            }
            previous_resource = Some(resource);
        }
        if previous_connection_binding_id
            .is_some_and(|previous| previous >= binding.binding_id.as_str())
            || !connection_binding_ids.insert(binding.binding_id.clone())
        {
            anyhow::bail!("governed connection bindings must be sorted and unique");
        }
        let mut previous_capability: Option<&str> = None;
        for capability in &binding.capabilities {
            if !is_normalized_authority_id(capability)
                || previous_capability.is_some_and(|previous| previous >= capability.as_str())
            {
                anyhow::bail!("governed connection capabilities must be sorted and unique");
            }
            previous_capability = Some(capability);
        }
        previous_connection_binding_id = Some(&binding.binding_id);
    }
    let mut previous_external_identity: Option<(&str, &str, u64)> = None;
    for definition in &grant.external_tools {
        if definition.tool_id.trim().is_empty()
            || definition.name.trim().is_empty()
            || definition
                .execution_owner
                .client_instance_id
                .trim()
                .is_empty()
            || definition.execution_owner.lease_epoch == 0
        {
            anyhow::bail!("governed client tool identity and lease must be present");
        }
        if definition
            .connection_binding_id
            .as_ref()
            .is_some_and(|id| !connection_binding_ids.contains(id))
        {
            anyhow::bail!("governed client tool references an unknown connection binding");
        }
        if definition.description.len() > 16 * 1024
            || serde_json::to_vec(&definition.input_schema)?.len() > 256 * 1024
        {
            anyhow::bail!("governed client tool definition exceeds size limits");
        }
        let identity = (
            definition.execution_owner.client_instance_id.clone(),
            definition.tool_id.clone(),
        );
        if !owner_scoped_ids.insert(identity) {
            anyhow::bail!("duplicate owner-scoped governed client tool id");
        }
        let ordered_identity = (
            definition.execution_owner.client_instance_id.as_str(),
            definition.tool_id.as_str(),
            definition.execution_owner.lease_epoch,
        );
        if previous_external_identity.is_some_and(|previous| previous >= ordered_identity) {
            anyhow::bail!(
                "governed client tools must be sorted by owner, tool id, and lease epoch"
            );
        }
        previous_external_identity = Some(ordered_identity);
    }
    Ok(())
}

pub(crate) const GOVERNED_GRANT_ISSUER: &str = "evalops.platform";
pub(crate) const GOVERNED_GRANT_AUDIENCE: &str = "evalops.maestro";
const GOVERNED_GRANT_PUBLIC_KEYS_ENV: &str = "MAESTRO_PLATFORM_TOOL_GRANT_ED25519_PUBLIC_KEYS";
#[cfg(test)]
pub(crate) static GOVERNED_GRANT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GovernedGrantPublicKey {
    algorithm: GovernedGrantPublicKeyAlgorithm,
    public_key: String,
    state: GovernedGrantPublicKeyState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum GovernedGrantPublicKeyAlgorithm {
    Ed25519,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum GovernedGrantPublicKeyState {
    Active,
    Retiring,
    Inactive,
}

#[derive(Debug, Clone)]
pub(crate) struct GovernedGrantVerificationContext<'a> {
    pub organization_id: &'a str,
    pub workspace_id: &'a str,
    pub thread_id: &'a str,
    pub turn_id: &'a str,
    pub run_id: &'a str,
    pub runtime_generation: u64,
}

fn governed_grant_canonical_value(grant: &GovernedToolGrant) -> serde_json::Value {
    let mut value = serde_json::json!({
        "envelope_version": grant.envelope_version,
        "grant_id": grant.grant_id,
        "grant_version": grant.grant_version,
        "issuer": grant.issuer,
        "audience": grant.audience,
        "organization_id": grant.organization_id,
        "workspace_id": grant.workspace_id,
        "thread_id": grant.thread_id,
        "turn_id": grant.turn_id,
        "run_id": grant.run_id,
        "runtime_generation": grant.runtime_generation,
        "grant_epoch": grant.grant_epoch,
        "issued_at_ms": grant.issued_at_ms,
        "not_before_ms": grant.not_before_ms,
        "expires_at_ms": grant.expires_at_ms,
        "signing_key_id": grant.signing_key_id,
        "identity_authorization": grant.identity_authorization,
        "native_tool_ids": grant.native_tool_ids,
        "external_tools": grant.external_tools,
    });
    if let Some(budget) = &grant.process_budget {
        value["process_budget"] = serde_json::json!(budget);
        value["process_system_prompt"] = serde_json::json!(grant.process_system_prompt);
    }
    // Preserve the exact v2 canonical form for grants minted before
    // connection bindings existed. New authority is included whenever used.
    if !grant.connection_bindings.is_empty() {
        value["connection_bindings"] = serde_json::json!(grant.connection_bindings);
    }
    value
}

fn governed_grant_canonical_bytes(grant: &GovernedToolGrant) -> Result<Vec<u8>> {
    serde_json::to_vec(&governed_grant_canonical_value(grant))
        .context("serialize governed tool grant canonical payload")
}

#[cfg(test)]
pub(crate) fn governed_tool_grant_canonical_bytes_for_test(
    grant: &GovernedToolGrant,
) -> Result<Vec<u8>> {
    governed_grant_canonical_bytes(grant)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn governed_grant_keys_from_env() -> Result<HashMap<String, GovernedGrantPublicKey>> {
    let raw = std::env::var(GOVERNED_GRANT_PUBLIC_KEYS_ENV)
        .context("governed tool grant public verification keys are not configured")?;
    governed_grant_keys_from_json(&raw)
}

/// Advertises only verifier algorithms that are fully configured and valid.
/// Old resident processes omit this capability, allowing a newer Runner Host
/// to preserve v1 chat continuity without attempting a governed v2 turn.
pub(crate) fn governed_grant_verifier_algorithms() -> Vec<&'static str> {
    governed_grant_keys_from_env()
        .ok()
        .filter(|keys| !keys.is_empty())
        .map(|_| vec!["ed25519"])
        .unwrap_or_default()
}

fn governed_grant_keys_from_json(raw: &str) -> Result<HashMap<String, GovernedGrantPublicKey>> {
    let keys = serde_json::from_str::<HashMap<String, GovernedGrantPublicKey>>(raw)
        .context("parse governed tool grant public verification keys")?;
    if keys.keys().any(|key_id| key_id.trim().is_empty()) {
        anyhow::bail!("governed tool grant public key ids must not be blank");
    }
    if keys
        .values()
        .any(|key| key.state == GovernedGrantPublicKeyState::Inactive)
    {
        anyhow::bail!("inactive governed tool grant public keys must not be distributed");
    }
    for key in keys.values() {
        let public_key = base64::engine::general_purpose::STANDARD
            .decode(&key.public_key)
            .context("decode governed tool grant public verification key")?;
        let public_key: [u8; 32] = public_key.try_into().map_err(|_| {
            anyhow::anyhow!("governed tool grant public verification key must be 32 bytes")
        })?;
        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&public_key)
            .map_err(|_| anyhow::anyhow!("validate governed tool grant public verification key"))?;
        if verifying_key.is_weak() {
            anyhow::bail!("governed tool grant public verification key must not be weak");
        }
    }
    Ok(keys)
}

pub(crate) fn verify_governed_tool_grant(
    grant: &GovernedToolGrant,
    context: &GovernedGrantVerificationContext<'_>,
    now_ms: i64,
) -> Result<()> {
    let keys = governed_grant_keys_from_env()?;
    verify_governed_tool_grant_with_keys(grant, context, now_ms, &keys)
}

fn verify_governed_tool_grant_with_keys(
    grant: &GovernedToolGrant,
    context: &GovernedGrantVerificationContext<'_>,
    now_ms: i64,
    keys: &HashMap<String, GovernedGrantPublicKey>,
) -> Result<()> {
    validate_governed_grant_shape(grant)?;
    if grant.issuer != GOVERNED_GRANT_ISSUER || grant.audience != GOVERNED_GRANT_AUDIENCE {
        anyhow::bail!("governed tool grant issuer or audience mismatch");
    }
    if grant.organization_id != context.organization_id
        || grant.workspace_id != context.workspace_id
        || grant.thread_id != context.thread_id
        || grant.turn_id != context.turn_id
        || grant.run_id != context.run_id
        || grant.runtime_generation != context.runtime_generation
    {
        anyhow::bail!("governed tool grant scope mismatch");
    }
    if now_ms < grant.not_before_ms || now_ms > grant.expires_at_ms {
        anyhow::bail!("governed tool grant is not currently valid");
    }
    if grant
        .identity_authorization
        .as_ref()
        .is_none_or(|identity| now_ms > identity.expires_at_ms)
    {
        anyhow::bail!("governed Identity authorization is expired");
    }
    if grant.process_budget.is_some() != grant.process_system_prompt.is_some() {
        anyhow::bail!("process budget requires its signed system instructions");
    }
    if let Some(budget) = &grant.process_budget {
        let prompt = grant.process_system_prompt.as_deref().unwrap_or_default();
        if prompt.trim().is_empty() || prompt.len() > 32 * 1024 {
            anyhow::bail!("invalid process system instructions");
        }
        budget.validate().map_err(anyhow::Error::msg)?;
        if budget.event_id != grant.turn_id {
            anyhow::bail!("process budget event does not match the governed turn");
        }
    }
    let canonical = governed_grant_canonical_bytes(grant)?;
    let expected_hash = format!("sha256:{}", sha256_hex(&canonical));
    if !constant_time_eq(expected_hash.as_bytes(), grant.grant_hash.as_bytes()) {
        anyhow::bail!("governed tool grant hash mismatch");
    }
    let key = keys
        .get(&grant.signing_key_id)
        .context("unknown governed tool grant signing key")?;
    if key.algorithm != GovernedGrantPublicKeyAlgorithm::Ed25519
        || key.state == GovernedGrantPublicKeyState::Inactive
    {
        anyhow::bail!("governed tool grant signing key is not active");
    }
    let public_key = base64::engine::general_purpose::STANDARD
        .decode(&key.public_key)
        .context("decode governed tool grant public verification key")?;
    if public_key.len() != 32 {
        anyhow::bail!("governed tool grant public verification key must be 32 bytes");
    }
    let signature = grant
        .grant_signature
        .strip_prefix("ed25519:")
        .context("governed tool grant signature algorithm mismatch")?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .context("decode governed tool grant signature")?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&canonical, &signature)
        .map_err(|_| anyhow::anyhow!("governed tool grant signature mismatch"))?;
    Ok(())
}

async fn submit_prompt_with_kind(
    state: &mut HeadlessState,
    content: String,
    attachments: Option<Vec<String>>,
    kind: PromptKind,
    managed_inference_authorization: Option<ManagedInferenceAuthorization>,
) -> Result<()> {
    let atts = attachments.unwrap_or_default();
    let queue_id = state.next_prompt_queue_id;
    state.next_prompt_queue_id = state.next_prompt_queue_id.saturating_add(1);
    let (turn_active, mut workspace_prompt, staged_workspace_prompt) = {
        let turn_active = state
            .meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .turn_active;
        (
            turn_active,
            state
                .workspace_capabilities
                .prompt_for_next_turn()
                .to_string(),
            state.workspace_capabilities.has_staged_set(),
        )
    };
    if let Some(prompt) = state
        .governed_grant
        .as_ref()
        .and_then(|grant| grant.process_system_prompt.as_deref())
    {
        workspace_prompt.push_str("\n\n");
        workspace_prompt.push_str(prompt);
    }
    let uses_app_server =
        crate::agent::codex_app_server_turns::model_should_use_app_server_turns(&state.model);
    if turn_active && staged_workspace_prompt && uses_app_server {
        anyhow::bail!(
            "a staged workspace prompt requires the active Codex turn to complete before the next prompt"
        );
    }
    let managed_request_lineage = state
        .governed_grant
        .as_ref()
        .map(managed_request_lineage_id);
    if !turn_active {
        state.system_prompt = workspace_prompt.clone();
    }
    if !turn_active && staged_workspace_prompt && uses_app_server {
        state.rotate_codex_agent_for_prompt_change().await?;
        state.ensure_agent()?;
        state
            .agent
            .as_ref()
            .context("staged provider prompt install requires a native agent")?
            .ensure_provider_prompt_installed()
            .await?;
    }
    if let Some(limits) = state
        .governed_grant
        .as_ref()
        .and_then(|grant| grant.process_budget.clone())
    {
        let checkpoint = state
            .meta
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime metadata poisoned"))?
            .process_budget
            .clone();
        state.ensure_agent()?;
        let budget = state
            .agent
            .as_ref()
            .context("process budget requires native agent")?
            .install_process_budget(limits, checkpoint)
            .await?;
        state
            .meta
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime metadata poisoned"))?
            .process_budget = Some(budget);
    }
    match state.agent_mut() {
        Ok(agent) => {
            if turn_active {
                agent.set_system_prompt_for_queued_prompt(queue_id, workspace_prompt.clone())?;
            } else {
                agent.set_system_prompt(workspace_prompt)?;
            }
            if let Err(err) = agent
                .prompt_with_kind_and_managed_context(
                    content,
                    atts,
                    kind,
                    Some(queue_id),
                    managed_request_lineage,
                    managed_inference_authorization,
                )
                .await
            {
                emit(&FromAgentMessage::Error {
                    request_id: None,
                    message: format!("Failed to send prompt: {err:#}"),
                    fatal: false,
                    terminal: true,
                    error_type: Some(HeadlessErrorType::Protocol),
                })?;
            } else {
                if staged_workspace_prompt {
                    state.workspace_capabilities.activate_staged_for_next_turn();
                }
                state
                    .meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .turn_active = true;
            }
        }
        Err(err) => {
            emit(&FromAgentMessage::Error {
                request_id: None,
                message: format!("Failed to start agent: {err:#}"),
                fatal: true,
                terminal: true,
                error_type: Some(HeadlessErrorType::Fatal),
            })?;
        }
    }
    Ok(())
}

fn managed_request_lineage_id(grant: &GovernedToolGrant) -> String {
    managed_turn_lineage_id(
        &grant.organization_id,
        &grant.workspace_id,
        &grant.thread_id,
        &grant.run_id,
        &grant.turn_id,
    )
}

fn sha256_prefixed(content: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(content.as_bytes()))
}

fn validate_prompt_experiment(assignment: &PromptExperimentAssignment) -> Result<(), &'static str> {
    if [
        assignment.experiment_id.as_str(),
        assignment.assignment_id.as_str(),
        assignment.artifact_id.as_str(),
        assignment.artifact_version.as_str(),
    ]
    .iter()
    .any(|value| value.is_empty() || value.trim() != *value || value.chars().any(char::is_control))
    {
        return Err("prompt experiment identity is invalid");
    }
    if assignment.artifact_content.trim().is_empty() {
        return Err("prompt experiment artifact content is empty");
    }
    if sha256_prefixed(&assignment.artifact_content) != assignment.artifact_sha256 {
        return Err("prompt experiment artifact digest does not match its content");
    }
    Ok(())
}

fn apply_init_settings(
    state: &mut HeadlessState,
    system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    thinking_level: Option<crate::headless::messages::ThinkingLevel>,
    approval_mode: Option<ApprovalMode>,
    history: Option<Vec<crate::headless::messages::HistoryMessage>>,
) {
    state.init_applied = true;
    let mut base_prompt = state.workspace_capabilities.base_prompt().to_string();
    if let Some(system_prompt) = system_prompt {
        base_prompt = system_prompt;
    }
    if let Some(append) = append_system_prompt {
        base_prompt.push_str("\n\n");
        base_prompt.push_str(&append);
    }
    state.workspace_capabilities.set_base_prompt(base_prompt);
    state.system_prompt = state.workspace_capabilities.current_prompt().to_string();
    if let Some(level) = thinking_level {
        let (enabled, budget) = match level {
            crate::headless::messages::ThinkingLevel::Off => (false, 0),
            crate::headless::messages::ThinkingLevel::Minimal => (true, 1_000),
            crate::headless::messages::ThinkingLevel::Low => (true, 5_000),
            crate::headless::messages::ThinkingLevel::Medium => (true, 10_000),
            crate::headless::messages::ThinkingLevel::High => (true, 20_000),
            crate::headless::messages::ThinkingLevel::XHigh => (true, 32_000),
            crate::headless::messages::ThinkingLevel::Ultra => (true, 50_000),
        };
        state.thinking_enabled = enabled;
        state.thinking_budget = budget;
    }
    if approval_mode.is_some() {
        state.set_approval_mode(approval_mode);
    }
    if history.is_some() {
        state.history = history;
    }
    if let Some(agent) = state.agent.as_ref() {
        let _ = agent.set_system_prompt(state.system_prompt.clone());
        let _ = agent.set_thinking(state.thinking_enabled, state.thinking_budget);
        if let Some(history) = state.history.as_deref() {
            let messages = crate::headless::messages::history_to_ai_messages(Some(history));
            agent.replace_history(messages);
        }
    }
}

fn configure_prompt_experiment(
    state: &mut HeadlessState,
    assignment: PromptExperimentAssignment,
) -> Result<()> {
    if !state.init_applied {
        anyhow::bail!("prompt experiment requires a prior init");
    }
    validate_prompt_experiment(&assignment).map_err(anyhow::Error::msg)?;
    let mut meta = state
        .meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if meta.turn_active {
        anyhow::bail!("prompt experiment cannot change during a turn");
    }
    if let Some(existing) = meta.prompt_experiment.as_ref() {
        if existing.assignment_id == assignment.assignment_id
            && existing.artifact_sha256 == assignment.artifact_sha256
            && existing.arm == assignment.arm
        {
            return Ok(());
        }
        anyhow::bail!("prompt experiment assignment cannot change in place");
    }
    if assignment.arm == PromptExperimentArm::Candidate {
        let mut base_prompt = state.workspace_capabilities.base_prompt().to_string();
        base_prompt.push_str("\n\n");
        base_prompt.push_str(&assignment.artifact_content);
        state.workspace_capabilities.set_base_prompt(base_prompt);
        state.system_prompt = state.workspace_capabilities.current_prompt().to_string();
        if let Some(agent) = state.agent.as_ref() {
            agent.set_system_prompt(state.system_prompt.clone())?;
        }
    }
    meta.prompt_experiment = Some(PromptExperimentExposure {
        experiment_id: assignment.experiment_id,
        assignment_id: assignment.assignment_id,
        arm: assignment.arm,
        artifact_id: assignment.artifact_id,
        artifact_version: assignment.artifact_version,
        artifact_sha256: assignment.artifact_sha256,
        provider_prompt_sha256: sha256_prefixed(&crate::agent::provider_system_prompt(
            &state.system_prompt,
            &state.model,
            state.model_capabilities,
        )),
        applied: assignment.arm == PromptExperimentArm::Candidate,
    });
    Ok(())
}

// Headless prompt receipts bind the launch model for the session lifetime.
// A receipt may arrive after an agent has already started, so pin automatic
// transitions from creation rather than waiting until receipt activation.
fn headless_model_dynamics(
    mut config: crate::model_dynamics::ModelDynamicsConfig,
) -> crate::model_dynamics::ModelDynamicsConfig {
    config.auto_boost = false;
    config.fallbacks.clear();
    config
}

/// Run the native headless protocol server until EOF or shutdown.
/// Installs the structured logger for the headless server process.
///
/// The hosted runner spawns `maestro-tui --headless` as a child process with
/// inherited stderr, and that child had no tracing subscriber of its own. Every
/// `tracing` event the agent emitted was therefore discarded, including the
/// `maestro.llm` events that name why a provider stream failed to open. A turn
/// could fail with `provider_error kind=transient_protocol` and leave no log
/// line anywhere in the fleet explaining the cause.
///
/// The logger writes to stderr only: headless stdout carries the protocol
/// frames and must stay machine-readable. When a subscriber is already
/// installed — the `maestro-tui hosted-runner` compatibility entrypoint
/// installs one before dispatching — this returns `None` and leaves it alone.
fn init_headless_tracing() -> Option<TelemetryGuard> {
    if tracing::dispatcher::has_been_set() {
        return None;
    }
    Some(TelemetryGuard::init(TelemetryConfig::new(
        "maestro-headless",
        env!("CARGO_PKG_VERSION"),
        "info",
        "local",
    )))
}

pub async fn run_headless_server(model_override: Option<String>) -> Result<i32> {
    crate::safety::disconnected_policy().map_err(anyhow::Error::msg)?;
    let _telemetry = init_headless_tracing();
    let mut state = HeadlessState::new(model_override);
    prepare_headless_local_model_with(
        &state.model,
        crate::local_models::discover_local_model(&state.model),
        |route, models| crate::local_models::replace_discovered_models(0, models, Some(route)),
    )
    .await
    .context("Failed to discover local model metadata for headless mode")?;
    tracing::info!(
        target: "maestro.model_binding",
        event = "maestro_model_binding_selected",
        session_id = ?state.session_id(),
        configured_model = %state.model,
        configured_provider = infer_provider_label(&state.model),
        routed_provider = managed_provider_override().as_deref().unwrap_or(""),
        binding_mode = model_binding_mode(&state.model),
    );

    // Provider construction resolves the exact model route and validates its
    // credential before `ensure_agent` emits the first Ready boundary.
    state.ensure_agent()?;

    // stdin reader on a blocking thread → channel
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        while let Some(Ok(line)) = lines.next() {
            if stdin_tx.send(line).is_err() {
                break;
            }
        }
    });

    let exit_code = 0i32;
    while let Some(line) = stdin_rx.recv().await {
        state
            .utility_commands
            .retain(|_, control| !control.is_closed());
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: ToAgentMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(err) => {
                emit(&FromAgentMessage::Error {
                    request_id: None,
                    message: format!("Invalid headless message: {err}"),
                    fatal: false,
                    terminal: false,
                    error_type: Some(HeadlessErrorType::Protocol),
                })?;
                continue;
            }
        };
        if let Err(error) = msg.validate_managed_inference_authorization() {
            protocol_error(None, error)?;
            continue;
        }

        match msg {
            ToAgentMessage::Hello {
                protocol_version,
                client_info,
                capabilities,
                role,
                opt_out_notifications,
                controller_binding: _,
            } => {
                // A client this build cannot serve must not get a session: the
                // error is fatal and ends the stdio loop, so a client that
                // ignores it cannot go on to Init and prompt anyway.
                if !crate::headless::messages::client_protocol_version_is_supported(
                    protocol_version.as_deref(),
                ) {
                    emit(&FromAgentMessage::Error {
                        request_id: None,
                        message:
                            crate::headless::messages::unsupported_client_protocol_version_message(
                                protocol_version.as_deref().unwrap_or_default(),
                            ),
                        fatal: true,
                        terminal: true,
                        error_type: Some(HeadlessErrorType::Protocol),
                    })?;
                    break;
                }
                let controller_binding = match controller_binding_from_hello_json(
                    line,
                    HEADLESS_PROTOCOL_VERSION,
                    &ControllerScopeExpectation::from_evalops_environment(),
                ) {
                    Ok(binding) => binding,
                    Err(error) => {
                        emit(&FromAgentMessage::Error {
                            request_id: None,
                            message: format!("Invalid controller binding: {error}"),
                            fatal: true,
                            terminal: true,
                            error_type: Some(HeadlessErrorType::Protocol),
                        })?;
                        break;
                    }
                };
                if let Err(error) = state.accept_controller_binding(controller_binding.as_ref()) {
                    emit(&FromAgentMessage::Error {
                        request_id: None,
                        message: format!("Invalid controller binding: {error}"),
                        fatal: true,
                        terminal: true,
                        error_type: Some(HeadlessErrorType::Protocol),
                    })?;
                    break;
                }
                let client_capabilities = capabilities.clone();
                if let Some(grade) = capabilities.and_then(|value| value.transcript_grade) {
                    state
                        .meta
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .transcript_grade = grade;
                }
                emit(&FromAgentMessage::HelloOk {
                    protocol_version: HEADLESS_PROTOCOL_VERSION.to_string(),
                    controller_binding_version: controller_binding
                        .as_ref()
                        .map(|binding| binding.binding_version.clone()),
                    controller_binding_sha256: controller_binding
                        .as_ref()
                        .map(|binding| binding.binding_sha256.clone()),
                    connection_id: Some("native-local".to_string()),
                    client_protocol_version: protocol_version,
                    client_info,
                    capabilities: client_capabilities,
                    server_capabilities: Some(native_server_capabilities()),
                    opt_out_notifications,
                    role,
                    controller_connection_id: None,
                    lease_expires_at: None,
                })?;
            }
            ToAgentMessage::Init {
                system_prompt: sp,
                append_system_prompt,
                thinking_level,
                approval_mode,
                history,
            } => {
                apply_init_settings(
                    &mut state,
                    sp,
                    append_system_prompt,
                    thinking_level,
                    approval_mode,
                    history,
                );
                emit(&FromAgentMessage::Status {
                    message: "init applied".to_string(),
                })?;
            }
            ToAgentMessage::GovernedInit {
                system_prompt,
                append_system_prompt,
                thinking_level,
                approval_mode,
                history,
                code_mode,
                tool_grant,
            } => {
                if let Err(error) = state
                    .apply_governed_grant(Some(code_mode), Some(tool_grant))
                    .await
                {
                    protocol_error(None, format!("governed code init rejected: {error:#}"))?;
                    continue;
                }
                apply_init_settings(
                    &mut state,
                    system_prompt,
                    append_system_prompt,
                    thinking_level,
                    approval_mode,
                    history,
                );
                emit(&FromAgentMessage::Status {
                    message: "governed init applied".to_string(),
                })?;
            }
            ToAgentMessage::ApplyWorkspaceCapabilitySet { request } => {
                match state.apply_workspace_capability_set(request).await {
                    Ok(receipt) => {
                        emit(&FromAgentMessage::WorkspaceCapabilitySetApplied { receipt })?;
                    }
                    Err(error) => protocol_error(
                        None,
                        format!("workspace prompt capability activation rejected: {error:#}"),
                    )?,
                }
            }
            ToAgentMessage::ManagedAuthorizationResult {
                request_id,
                authorization,
            } => {
                let result = state
                    .agent
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no active native agent"))
                    .and_then(|agent| {
                        agent
                            .managed_authorization_coordinator()
                            .respond(&request_id, authorization)
                    });
                match result {
                    Ok(()) => emit(&FromAgentMessage::ResponseAccepted(
                        tool_wire::ResponseAccepted { request_id },
                    ))?,
                    Err(error) => protocol_error(
                        Some(request_id),
                        format!("authorization result rejected: {error}"),
                    )?,
                }
            }
            ToAgentMessage::ConfigurePromptExperiment { assignment } => {
                if let Err(error) = configure_prompt_experiment(&mut state, assignment) {
                    protocol_error(None, format!("prompt experiment rejected: {error:#}"))?;
                    continue;
                }
                emit(&FromAgentMessage::Status {
                    message: "prompt experiment configured".to_string(),
                })?;
            }
            ToAgentMessage::RestoreConversation {
                protocol_version,
                messages,
            } => {
                if !state.init_applied {
                    protocol_error(None, "semantic conversation restore requires a prior init")?;
                    continue;
                }
                if protocol_version != crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL {
                    protocol_error(
                        None,
                        format!(
                            "unsupported semantic conversation protocol version: {protocol_version}"
                        ),
                    )?;
                    continue;
                }
                match state.agent_mut() {
                    Ok(agent) => agent.replace_history(messages),
                    Err(error) => {
                        protocol_error(
                            None,
                            format!("failed to restore semantic conversation: {error:#}"),
                        )?;
                    }
                }
            }
            ToAgentMessage::Prompt {
                content,
                attachments,
                managed_inference_authorization,
            } => {
                if state.governed_grant.is_some() {
                    protocol_error(
                        None,
                        "governed code sessions require a grant on every prompt",
                    )?;
                    continue;
                }
                submit_prompt_with_kind(
                    &mut state,
                    content,
                    attachments,
                    PromptKind::Prompt,
                    managed_inference_authorization,
                )
                .await?;
            }
            ToAgentMessage::GovernedPrompt {
                content,
                attachments,
                code_mode,
                tool_grant,
                managed_inference_authorization,
            } => {
                if let Err(error) = state
                    .apply_governed_grant(Some(code_mode), Some(tool_grant))
                    .await
                {
                    protocol_error(None, format!("governed code turn rejected: {error:#}"))?;
                    continue;
                }
                submit_prompt_with_kind(
                    &mut state,
                    content,
                    attachments,
                    PromptKind::Prompt,
                    managed_inference_authorization,
                )
                .await?;
            }
            ToAgentMessage::Steer {
                content,
                attachments,
                managed_inference_authorization,
            } => {
                if state.governed_grant.is_some() {
                    protocol_error(
                        None,
                        "governed code sessions require a grant on every steer",
                    )?;
                    continue;
                }
                submit_prompt_with_kind(
                    &mut state,
                    content,
                    attachments,
                    PromptKind::Steer,
                    managed_inference_authorization,
                )
                .await?;
            }
            ToAgentMessage::GovernedSteer {
                content,
                attachments,
                code_mode,
                tool_grant,
                managed_inference_authorization,
            } => {
                if let Err(error) = state
                    .apply_governed_grant(Some(code_mode), Some(tool_grant))
                    .await
                {
                    protocol_error(None, format!("governed code steer rejected: {error:#}"))?;
                    continue;
                }
                submit_prompt_with_kind(
                    &mut state,
                    content,
                    attachments,
                    PromptKind::Steer,
                    managed_inference_authorization,
                )
                .await?;
            }
            ToAgentMessage::Interrupt | ToAgentMessage::Cancel => {
                if let Some(agent) = state.agent.as_ref() {
                    agent.cancel();
                }
                emit(&FromAgentMessage::Error {
                    request_id: None,
                    message: "operation cancelled".to_string(),
                    fatal: false,
                    terminal: true,
                    error_type: Some(HeadlessErrorType::Cancelled),
                })?;
            }
            ToAgentMessage::ToolResponse {
                call_id,
                tool_execution_id,
                approved,
                result,
            } => {
                let Some(tool_tx) = state.tool_tx.as_ref() else {
                    protocol_error(Some(call_id), "no pending native tool request")?;
                    continue;
                };
                match prepare_tool_response(
                    &state.meta,
                    call_id.clone(),
                    tool_execution_id,
                    approved,
                    result,
                ) {
                    Ok(accepted) => {
                        match dispatch_accepted_tool_response(
                            &state.meta,
                            tool_tx,
                            accepted,
                            call_id.clone(),
                        ) {
                            Ok(()) => {}
                            Err(message) => protocol_error(Some(call_id), message)?,
                        }
                    }
                    Err(message) => protocol_error(Some(call_id), message)?,
                }
            }
            ToAgentMessage::ClientToolResult {
                call_id,
                content,
                is_error,
            } => {
                let Some(tool_tx) = state.tool_tx.as_ref() else {
                    protocol_error(Some(call_id), "no pending native client-tool request")?;
                    continue;
                };
                match prepare_client_tool_result(
                    &state.meta,
                    call_id.clone(),
                    content,
                    is_error,
                    ClientToolResultBinding::default(),
                ) {
                    Ok(accepted) => {
                        match dispatch_accepted_tool_response(
                            &state.meta,
                            tool_tx,
                            accepted,
                            call_id.clone(),
                        ) {
                            Ok(()) => {}
                            Err(message) => protocol_error(Some(call_id), message)?,
                        }
                    }
                    Err(error) => dispatch_client_tool_preparation_error(
                        &state.meta,
                        tool_tx,
                        call_id,
                        error,
                    )?,
                }
            }
            message @ ToAgentMessage::GovernedClientToolResult(
                tool_wire::GovernedClientToolResult { .. },
            ) => {
                handle_governed_client_tool_result(&state, message)?;
            }
            ToAgentMessage::ServerRequestResponse {
                request_id,
                request_type,
                approved,
                result,
                content,
                is_error,
                decision_action,
                reason,
            } => {
                let resolution = server_request_resolution(
                    request_type,
                    approved,
                    result.as_ref(),
                    is_error,
                    decision_action,
                );
                let agent_result = result.map(headless_tool_result_to_agent).or_else(|| {
                    content.map(|value| {
                        client_content_to_agent_result(value, is_error.unwrap_or(false))
                    })
                });
                let response_queued = if let Some(tool_tx) = state.tool_tx.as_ref() {
                    let approved = approved.unwrap_or(!matches!(
                        resolution,
                        ServerRequestResolutionStatus::Denied
                            | ServerRequestResolutionStatus::Failed
                            | ServerRequestResolutionStatus::Skipped
                            | ServerRequestResolutionStatus::Aborted
                    ));
                    send_tool_response_with_consumption_ack(
                        &state.meta,
                        tool_tx,
                        (
                            request_id.clone(),
                            approved,
                            agent_result,
                            ExecutionSource::RemoteClient,
                            None,
                        ),
                        request_id.clone(),
                    )
                } else {
                    false
                };
                if !response_queued {
                    protocol_error(
                        Some(request_id),
                        "native server-request response channel is closed",
                    )?;
                    continue;
                }
                emit(&FromAgentMessage::ServerRequestResolved {
                    request_id: request_id.clone(),
                    request_type,
                    call_id: request_id,
                    resolution,
                    reason,
                    resolved_by: ServerRequestResolvedBy::Client,
                    started_at_ms: None,
                    resolved_at_ms: Some(unix_timestamp_ms()),
                })?;
            }
            ToAgentMessage::UtilityCommandStart {
                command_id,
                command,
                cwd,
                env,
                shell_mode,
                terminal_mode,
                allow_stdin,
                columns,
                rows,
            } => {
                if state.utility_commands.contains_key(&command_id) {
                    protocol_error(Some(command_id), "utility command id is already running")?;
                    continue;
                }
                let cwd = cwd.unwrap_or_else(|| state.cwd.clone());
                match start_utility_command(UtilityCommandOptions {
                    command_id: command_id.clone(),
                    command,
                    cwd,
                    env,
                    shell_mode: shell_mode.unwrap_or(UtilityCommandShellMode::Shell),
                    terminal_mode: terminal_mode.unwrap_or(UtilityCommandTerminalMode::Pipe),
                    allow_stdin: allow_stdin.unwrap_or(false),
                    columns,
                    rows,
                })
                .await
                {
                    Ok(control) => {
                        state.utility_commands.insert(command_id, control);
                    }
                    Err(err) => protocol_error(
                        Some(command_id),
                        format!("utility command failed: {err:#}"),
                    )?,
                }
            }
            ToAgentMessage::UtilityCommandTerminate { command_id, .. } => {
                match state.utility_commands.remove(&command_id) {
                    Some(control) => {
                        let _ = control.send(UtilityCommandControl::Terminate);
                    }
                    None => protocol_error(Some(command_id), "utility command is not running")?,
                }
            }
            ToAgentMessage::UtilityCommandStdin {
                command_id,
                content,
                eof,
            } => match state.utility_commands.get(&command_id) {
                Some(control) => {
                    let _ = control.send(UtilityCommandControl::Stdin {
                        content,
                        eof: eof.unwrap_or(false),
                    });
                }
                None => protocol_error(Some(command_id), "utility command is not running")?,
            },
            ToAgentMessage::UtilityCommandResize {
                command_id,
                columns,
                rows,
            } => {
                if state.utility_commands.contains_key(&command_id) {
                    emit(&FromAgentMessage::UtilityCommandResized {
                        command_id,
                        columns,
                        rows,
                    })?;
                } else {
                    protocol_error(Some(command_id), "utility command is not running")?;
                }
            }
            ToAgentMessage::UtilityFileSearch {
                search_id,
                query,
                cwd,
                limit,
            } => {
                let cwd = cwd.unwrap_or_else(|| state.cwd.clone());
                match utility_file_search(&cwd, &query, limit.unwrap_or(50) as usize) {
                    Ok((results, truncated)) => {
                        emit(&FromAgentMessage::UtilityFileSearchResults {
                            search_id,
                            query,
                            cwd,
                            results,
                            truncated,
                        })?;
                    }
                    Err(err) => {
                        protocol_error(Some(search_id), format!("file search failed: {err:#}"))?;
                    }
                }
            }
            ToAgentMessage::UtilityFileRead {
                read_id,
                path,
                cwd,
                offset,
                limit,
            } => {
                let cwd = cwd.unwrap_or_else(|| state.cwd.clone());
                match utility_file_read(&cwd, &path, offset.unwrap_or(0), limit.unwrap_or(2_000))
                    .await
                {
                    Ok(result) => emit(&FromAgentMessage::UtilityFileReadResult {
                        read_id,
                        path,
                        relative_path: result.relative_path,
                        cwd,
                        content: result.content,
                        start_line: result.start_line,
                        end_line: result.end_line,
                        total_lines: result.total_lines,
                        truncated: result.truncated,
                    })?,
                    Err(err) => {
                        protocol_error(Some(read_id), format!("file read failed: {err:#}"))?;
                    }
                }
            }
            ToAgentMessage::UtilityFileWatchStart {
                watch_id,
                root_dir,
                include_patterns,
                exclude_patterns,
                debounce_ms,
            } => {
                if state.file_watches.contains_key(&watch_id) {
                    protocol_error(Some(watch_id), "file watch id is already running")?;
                    continue;
                }
                let root_dir = root_dir.unwrap_or_else(|| state.cwd.clone());
                match start_file_watch(
                    watch_id.clone(),
                    root_dir.clone(),
                    include_patterns.clone(),
                    exclude_patterns.clone(),
                    debounce_ms.unwrap_or(100),
                ) {
                    Ok(task) => {
                        state.file_watches.insert(watch_id.clone(), task);
                        emit(&FromAgentMessage::UtilityFileWatchStarted {
                            watch_id,
                            root_dir,
                            include_patterns,
                            exclude_patterns,
                            debounce_ms: debounce_ms.unwrap_or(100),
                            owner_connection_id: Some("native-local".to_string()),
                        })?;
                    }
                    Err(err) => {
                        protocol_error(Some(watch_id), format!("file watch failed: {err:#}"))?;
                    }
                }
            }
            ToAgentMessage::UtilityFileWatchStop { watch_id } => {
                match state.file_watches.remove(&watch_id) {
                    Some(task) => {
                        task.abort();
                        emit(&FromAgentMessage::UtilityFileWatchStopped {
                            watch_id,
                            reason: Some("client requested".to_string()),
                        })?;
                    }
                    None => protocol_error(Some(watch_id), "file watch is not running")?,
                }
            }
            ToAgentMessage::Shutdown => {
                for (_, control) in state.utility_commands.drain() {
                    let _ = control.send(UtilityCommandControl::Terminate);
                }
                for (_, task) in state.file_watches.drain() {
                    task.abort();
                }
                emit(&FromAgentMessage::Status {
                    message: "shutting down".to_string(),
                })?;
                break;
            }
        }
    }

    if let Some(agent) = state.agent.take() {
        agent.shutdown().await;
    }
    if let Some(task) = state.event_task.take() {
        let _ = task.await;
    }
    // The agent shutdown resolved every outstanding consumption receipt
    // (consumed or dropped); drain the acknowledgement tasks so dropped
    // receipts emit their protocol error and rollback before the process
    // exits and before the interrupted terminals are taken.
    for task in take_receipt_tasks(&state.meta) {
        let _ = task.await;
    }
    for message in take_interrupted_tool_terminal_messages(&state.meta) {
        emit(&message)?;
    }
    Ok(exit_code)
}

async fn prepare_headless_local_model_with<F>(
    route: &str,
    discovery: impl std::future::Future<Output = Result<Option<crate::model_catalog::ModelInfo>>>,
    publish: F,
) -> Result<()>
where
    F: FnOnce(&str, &[crate::model_catalog::ModelInfo]),
{
    if let Some(discovered) = discovery.await? {
        publish(route, std::slice::from_ref(&discovered));
    }
    Ok(())
}

fn protocol_error(request_id: Option<String>, message: impl Into<String>) -> Result<()> {
    emit(&FromAgentMessage::Error {
        request_id,
        message: message.into(),
        fatal: false,
        terminal: false,
        error_type: Some(HeadlessErrorType::Protocol),
    })
}

fn client_content_to_agent_result(
    content: Vec<ClientToolResultContent>,
    is_error: bool,
) -> ToolResult {
    let output = content
        .into_iter()
        .map(|item| match item {
            ClientToolResultContent::Text { text } => text,
            ClientToolResultContent::Image { data, mime_type } => {
                format!("data:{mime_type};base64,{data}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    ToolResult {
        success: !is_error,
        error: is_error.then(|| output.clone()),
        output,
        details: None,
    }
}

fn server_request_resolution(
    request_type: ServerRequestType,
    approved: Option<bool>,
    result: Option<&HeadlessToolResult>,
    is_error: Option<bool>,
    decision: Option<ToolRetryDecisionAction>,
) -> ServerRequestResolutionStatus {
    if let Some(action) = decision {
        return match action {
            ToolRetryDecisionAction::Retry => ServerRequestResolutionStatus::Retried,
            ToolRetryDecisionAction::Skip => ServerRequestResolutionStatus::Skipped,
            ToolRetryDecisionAction::Abort => ServerRequestResolutionStatus::Aborted,
        };
    }
    if approved == Some(false) {
        return ServerRequestResolutionStatus::Denied;
    }
    if is_error == Some(true) || result.is_some_and(|value| !value.success) {
        return ServerRequestResolutionStatus::Failed;
    }
    match request_type {
        ServerRequestType::Approval => ServerRequestResolutionStatus::Approved,
        ServerRequestType::ClientTool => ServerRequestResolutionStatus::Completed,
        ServerRequestType::UserInput => ServerRequestResolutionStatus::Answered,
        ServerRequestType::ToolRetry => ServerRequestResolutionStatus::Retried,
    }
}

async fn start_utility_command(
    options: UtilityCommandOptions,
) -> Result<mpsc::UnboundedSender<UtilityCommandControl>> {
    let UtilityCommandOptions {
        command_id,
        command,
        cwd,
        env,
        shell_mode,
        terminal_mode,
        allow_stdin,
        columns,
        rows,
    } = options;
    let cwd_path = PathBuf::from(&cwd);
    if !cwd_path.is_dir() {
        anyhow::bail!("working directory does not exist: {cwd}");
    }
    let mut process = match shell_mode {
        UtilityCommandShellMode::Shell => {
            #[cfg(windows)]
            let process = {
                let mut process = Command::new("cmd");
                process.args(["/C", &command]);
                process
            };
            #[cfg(not(windows))]
            let process = {
                let mut process = Command::new("sh");
                process.args(["-lc", &command]);
                process
            };
            process
        }
        UtilityCommandShellMode::Direct => {
            let args = shlex::split(&command).context("parse direct command")?;
            let (program, args) = args.split_first().context("direct command is empty")?;
            let mut process = Command::new(program);
            process.args(args);
            process
        }
    };
    process
        .current_dir(&cwd_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if allow_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if let Some(env) = env {
        process.envs(env);
    }
    let mut child = process.spawn().context("spawn utility command")?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdin = child.stdin.take();
    emit(&FromAgentMessage::UtilityCommandStarted {
        command_id: command_id.clone(),
        command,
        cwd: Some(cwd),
        shell_mode,
        terminal_mode,
        pid,
        columns,
        rows,
        owner_connection_id: Some("native-local".to_string()),
    })?;
    if let Some(stdout) = stdout {
        spawn_command_reader(command_id.clone(), UtilityCommandStream::Stdout, stdout);
    }
    if let Some(stderr) = stderr {
        spawn_command_reader(command_id.clone(), UtilityCommandStream::Stderr, stderr);
    }
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (success, exit_code, reason) = loop {
            tokio::select! {
                status = child.wait() => {
                    match status {
                        Ok(status) => break (status.success(), status.code(), None),
                        Err(err) => break (false, None, Some(format!("wait failed: {err}"))),
                    }
                }
                control = control_rx.recv() => {
                    match control {
                        Some(UtilityCommandControl::Terminate) => {
                            let kill_result = child.kill().await;
                            let status = child.wait().await.ok();
                            break (
                                false,
                                status.and_then(|value| value.code()),
                                kill_result.err().map(|err| format!("terminate failed: {err}"))
                                    .or_else(|| Some("terminated by client".to_string())),
                            );
                        }
                        Some(UtilityCommandControl::Stdin { content, eof }) => {
                            if let Some(writer) = stdin.as_mut() {
                                if writer.write_all(content.as_bytes()).await.is_err() {
                                    stdin = None;
                                } else if eof {
                                    let _ = writer.shutdown().await;
                                    stdin = None;
                                }
                            }
                        }
                        None => {
                            let _ = child.kill().await;
                            let status = child.wait().await.ok();
                            break (false, status.and_then(|value| value.code()), Some("runtime closed".to_string()));
                        }
                    }
                }
            }
        };
        let _ = emit(&FromAgentMessage::UtilityCommandExited {
            command_id,
            success,
            exit_code,
            signal: None,
            reason,
        });
    });
    Ok(control_tx)
}

fn spawn_command_reader<R>(command_id: String, stream: UtilityCommandStream, mut reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(count) => {
                    let content = String::from_utf8_lossy(&buffer[..count]).into_owned();
                    let _ = emit(&FromAgentMessage::UtilityCommandOutput {
                        command_id: command_id.clone(),
                        stream,
                        content,
                    });
                }
                Err(_) => break,
            }
        }
    });
}

fn utility_file_search(
    cwd: &str,
    query: &str,
    limit: usize,
) -> Result<(Vec<UtilityFileSearchMatch>, bool)> {
    let root = Path::new(cwd);
    if !root.is_dir() {
        anyhow::bail!("search directory does not exist: {cwd}");
    }
    let scan_limit = limit.saturating_mul(100).clamp(1_000, 100_000);
    let files = crate::files::get_workspace_files(root, scan_limit);
    let total_files = files.len();
    let result = crate::files::FileSearch::new(files)
        .max_results(limit.max(1))
        .search(query);
    let results = result
        .matches
        .into_iter()
        .map(|item| UtilityFileSearchMatch {
            path: item.file.relative_path,
            score: item.score,
        })
        .collect();
    Ok((results, total_files >= scan_limit))
}

struct FileReadResult {
    relative_path: String,
    content: String,
    start_line: u32,
    end_line: u32,
    total_lines: u32,
    truncated: bool,
}

async fn utility_file_read(
    cwd: &str,
    path: &str,
    offset: u32,
    limit: u32,
) -> Result<FileReadResult> {
    let root = tokio::fs::canonicalize(cwd)
        .await
        .with_context(|| format!("resolve read directory {cwd}"))?;
    let requested = Path::new(path);
    let target = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let target = tokio::fs::canonicalize(&target)
        .await
        .with_context(|| format!("resolve file {}", target.display()))?;
    if !target.starts_with(&root) {
        anyhow::bail!("file escapes the requested workspace");
    }
    let bytes = tokio::fs::read(&target)
        .await
        .context("read workspace file")?;
    let text = String::from_utf8(bytes).context("workspace file is not UTF-8")?;
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(lines.len());
    let take = usize::try_from(limit).unwrap_or(usize::MAX);
    let end = start.saturating_add(take).min(lines.len());
    let relative_path = target
        .strip_prefix(&root)
        .unwrap_or(&target)
        .to_string_lossy()
        .into_owned();
    Ok(FileReadResult {
        relative_path,
        content: lines[start..end].join("\n"),
        start_line: u32::try_from(start.saturating_add(1)).unwrap_or(u32::MAX),
        end_line: u32::try_from(end).unwrap_or(u32::MAX),
        total_lines,
        truncated: end < lines.len(),
    })
}

type WatchSnapshot = HashMap<String, (u64, u64)>;

fn start_file_watch(
    watch_id: String,
    root_dir: String,
    include_patterns: Option<Vec<String>>,
    exclude_patterns: Option<Vec<String>>,
    debounce_ms: u32,
) -> Result<tokio::task::JoinHandle<()>> {
    let root = PathBuf::from(&root_dir);
    if !root.is_dir() {
        anyhow::bail!("watch directory does not exist: {root_dir}");
    }
    let includes = compile_patterns(include_patterns.as_deref())?;
    let excludes = compile_patterns(exclude_patterns.as_deref())?;
    let mut previous = watch_snapshot(&root, &includes, &excludes);
    Ok(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(u64::from(
            debounce_ms.max(25),
        )));
        interval.tick().await;
        loop {
            interval.tick().await;
            let current = watch_snapshot(&root, &includes, &excludes);
            for (path, stamp) in &current {
                let change_type = match previous.get(path) {
                    None => Some(crate::headless::messages::UtilityFileWatchChangeType::Create),
                    Some(previous_stamp) if previous_stamp != stamp => {
                        Some(crate::headless::messages::UtilityFileWatchChangeType::Modify)
                    }
                    _ => None,
                };
                if let Some(change_type) = change_type {
                    emit_watch_event(&watch_id, &root, path, change_type);
                }
            }
            for path in previous.keys().filter(|path| !current.contains_key(*path)) {
                emit_watch_event(
                    &watch_id,
                    &root,
                    path,
                    crate::headless::messages::UtilityFileWatchChangeType::Delete,
                );
            }
            previous = current;
        }
    }))
}

fn compile_patterns(patterns: Option<&[String]>) -> Result<Vec<glob::Pattern>> {
    patterns
        .unwrap_or(&[])
        .iter()
        .map(|pattern| {
            glob::Pattern::new(pattern).with_context(|| format!("invalid glob {pattern}"))
        })
        .collect()
}

fn watch_snapshot(
    root: &Path,
    includes: &[glob::Pattern],
    excludes: &[glob::Pattern],
) -> WatchSnapshot {
    crate::files::get_workspace_files(root, 100_000)
        .into_iter()
        .filter_map(|file| {
            let relative = file.relative_path;
            let included =
                includes.is_empty() || includes.iter().any(|pattern| pattern.matches(&relative));
            let excluded = excludes.iter().any(|pattern| pattern.matches(&relative));
            if !included || excluded {
                return None;
            }
            let metadata = std::fs::metadata(root.join(&relative)).ok()?;
            let modified = metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            Some((relative, (modified, metadata.len())))
        })
        .collect()
}

fn emit_watch_event(
    watch_id: &str,
    root: &Path,
    relative_path: &str,
    change_type: crate::headless::messages::UtilityFileWatchChangeType,
) {
    let _ = emit(&FromAgentMessage::UtilityFileWatchEvent {
        watch_id: watch_id.to_string(),
        change_type,
        path: root.join(relative_path).to_string_lossy().into_owned(),
        relative_path: relative_path.to_string(),
        timestamp: unix_timestamp_ms(),
        is_directory: false,
    });
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn take_interrupted_tool_terminal_messages(
    meta: &Arc<Mutex<RuntimeMeta>>,
) -> Vec<FromAgentMessage> {
    let (pending, pending_client_tools, emitted_client_tool_terminals) = {
        let mut meta = meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        meta.pending_tool_calls.clear();
        (
            meta.tool_execution_ids.drain().collect::<Vec<_>>(),
            meta.pending_client_tools.drain().collect::<Vec<_>>(),
            meta.emitted_client_tool_terminals.clone(),
        )
    };
    let mut pending = pending
        .into_iter()
        .filter(|(_, tool_execution_id)| !emitted_client_tool_terminals.contains(tool_execution_id))
        .collect::<Vec<_>>();
    pending.extend(
        pending_client_tools
            .into_iter()
            .filter(|(_, pending)| {
                !emitted_client_tool_terminals.contains(&pending.tool_execution_id)
            })
            .map(|(call_id, pending)| (call_id, pending.tool_execution_id)),
    );
    pending.sort_by(|(left, _), (right, _)| left.cmp(right));
    pending.dedup();
    pending
        .into_iter()
        .map(|(call_id, tool_execution_id)| {
            FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                success: false,
                tool: None,
                details: Some(serde_json::json!({
                    "reason": "interrupted_before_tool_completion"
                })),
                receipt: None,
            })
        })
        .collect()
}

pub(crate) fn headless_error_type(
    fatal: bool,
    terminal: bool,
    retryable: bool,
) -> HeadlessErrorType {
    if fatal {
        HeadlessErrorType::Fatal
    } else if retryable || !terminal {
        HeadlessErrorType::Transient
    } else {
        HeadlessErrorType::Protocol
    }
}

async fn handle_agent_event(
    msg: FromAgent,
    meta: &Arc<Mutex<RuntimeMeta>>,
    tool_tx: &mpsc::UnboundedSender<ToolResponseMessage>,
    configured_model: &str,
    routed_provider: Option<&str>,
) -> Result<()> {
    if matches!(
        &msg,
        FromAgent::ResponseEnd { .. }
            | FromAgent::Error { .. }
            | FromAgent::TurnCompleted { .. }
            | FromAgent::ToolCall { .. }
            | FromAgent::ToolEnd { .. }
    ) {
        let budget = meta
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime metadata poisoned"))?
            .process_budget
            .clone();
        if let Some(budget) = budget {
            let budget = budget
                .lock()
                .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                .clone();
            emit(&FromAgentMessage::ProcessBudgetCheckpoint { budget })?;
        }
    }
    match msg {
        FromAgent::ManagedAuthorizationRequest { request_id } => {
            emit(&FromAgentMessage::ManagedAuthorizationRequest { request_id })?;
        }
        FromAgent::LocalAssistantContent { .. } => return Ok(()),
        FromAgent::ConversationSnapshot {
            protocol_version,
            messages,
            processed_queue_ids,
        } => {
            meta.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .conversation_snapshot = Some(messages.clone());
            emit(&FromAgentMessage::ConversationSnapshot {
                protocol_version,
                messages,
                processed_queue_ids,
            })?;
        }
        FromAgent::ManagedGatewayReceipt {
            request_id,
            record_id,
            lineage_id,
            record_status,
            provider_prompt_sha256,
            provider_tools_sha256,
            provider_tool_count,
        } => {
            let prompt_experiment = {
                let mut meta = meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                meta.managed_gateway_receipt = Some(maestro_ai::ManagedGatewayReceipt {
                    request_id: request_id.clone(),
                    record_id: record_id.clone(),
                    lineage_id: lineage_id.clone(),
                    record_status: record_status.clone(),
                    provider_prompt_sha256: provider_prompt_sha256.clone(),
                    provider_tools_sha256: provider_tools_sha256.clone(),
                    provider_tool_count,
                });
                // Bind each exposure to this successful HTTP request, including
                // dynamic prompt context. Error receipts never attest delivery.
                provider_prompt_sha256.and_then(|digest| {
                    meta.prompt_experiment.clone().map(|mut exposure| {
                        exposure.provider_prompt_sha256 = digest;
                        exposure
                    })
                })
            };
            emit(&FromAgentMessage::ManagedGatewayReceipt {
                request_id,
                record_id,
                lineage_id,
                record_status,
                prompt_experiment,
            })?;
        }
        FromAgent::Ready { model, provider } => {
            let session_id = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id
                .clone();
            let (native_model, native_provider, model, provider) =
                observed_ready_identity(&model, &provider, routed_provider);
            tracing::info!(
                target: "maestro.model_binding",
                event = "maestro_model_binding_observed",
                session_id = ?session_id,
                configured_model = %configured_model,
                native_model = %native_model,
                native_provider = %native_provider,
                reported_model = %model,
                reported_provider = %provider,
                routed_provider = routed_provider.unwrap_or(""),
                binding_mode = model_binding_mode(configured_model),
            );
            emit(&FromAgentMessage::Ready {
                protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
                model,
                provider,
                session_id,
            })?;
        }
        FromAgent::ResponseStart { response_id } => {
            let session_id = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id
                .clone();
            tracing::debug!(
                target: "maestro.llm",
                event = "maestro_response_started",
                session_id = ?session_id,
                response_id = %response_id,
            );
            let mut meta = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            meta.response_chunks.clear();
            meta.managed_gateway_receipt = None;
            drop(meta);
            emit(&FromAgentMessage::ResponseStart { response_id })?;
        }
        FromAgent::ResponseChunk {
            response_id,
            content,
            is_thinking,
        } => {
            let grade = {
                let mut meta = meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                meta.record_response_chunk(&content, is_thinking)
            };
            if grade == crate::transcript::TranscriptGrade::Delta {
                emit(&FromAgentMessage::ResponseChunk {
                    response_id,
                    content,
                    is_thinking,
                })?;
            }
        }
        FromAgent::ResponseEnd { response_id, usage } => {
            let session_id = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id
                .clone();
            tracing::info!(
                target: "maestro.llm",
                event = "maestro_response_completed",
                session_id = ?session_id,
                response_id = %response_id,
                usage_present = usage.is_some(),
                configured_model = %configured_model,
                routed_provider = routed_provider.unwrap_or(""),
            );
            for message in take_interrupted_tool_terminal_messages(meta) {
                emit(&message)?;
            }
            let (grade, content) = {
                let mut meta = meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let content = coalesce_response_chunks(&mut meta.response_chunks);
                (meta.transcript_grade, content)
            };
            if matches!(
                grade,
                crate::transcript::TranscriptGrade::Turn
                    | crate::transcript::TranscriptGrade::Block
            ) && !content.is_empty()
            {
                emit(&FromAgentMessage::ResponseChunk {
                    response_id: response_id.clone(),
                    content,
                    is_thinking: false,
                })?;
            }
            emit(&FromAgentMessage::ResponseEnd {
                response_id,
                usage: usage
                    .map(|usage| to_headless_usage(usage, configured_model, routed_provider)),
                tools_summary: None,
                duration_ms: None,
                ttft_ms: None,
            })?;
        }
        FromAgent::TurnCompleted {
            response_id,
            coding_completion,
            coding_child_records,
        } => {
            meta.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .turn_active = false;
            emit(&FromAgentMessage::TurnCompleted {
                response_id,
                coding_completion,
                coding_child_records,
            })?;
        }
        FromAgent::TurnInterrupted {
            response_id,
            reason,
        } => {
            meta.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .turn_active = false;
            emit(&FromAgentMessage::TurnInterrupted {
                response_id,
                reason,
            })?;
        }
        FromAgent::ToolCall {
            call_id,
            tool,
            args,
            requires_approval,
            ..
        } => {
            let client_binding = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .client_tool_bindings
                .get(&tool.to_ascii_lowercase())
                .cloned();
            if let Some(binding) = client_binding {
                let tool_execution_id = uuid::Uuid::new_v4().to_string();
                let args_digest = canonical_json_digest(&args)?;
                let idempotency_key = format!("client-tool:{tool_execution_id}");
                {
                    let mut runtime = meta
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    runtime.pending_tool_calls.insert(call_id.clone());
                    runtime.pending_client_tools.insert(
                        call_id.clone(),
                        PendingClientTool {
                            binding: binding.clone(),
                            tool_execution_id: tool_execution_id.clone(),
                            args_digest: args_digest.clone(),
                            idempotency_key: idempotency_key.clone(),
                            result_digest: None,
                        },
                    );
                }
                emit(&FromAgentMessage::GovernedClientToolRequest(
                    tool_wire::GovernedClientToolRequest {
                        call_id,
                        tool_execution_id,
                        tool: binding.logical_name,
                        args,
                        provider_tool_name: binding.provider_tool_name,
                        tool_id: binding.tool_id,
                        connection_binding_id: binding.connection_binding_id,
                        client_instance_id: binding.owner.client_instance_id,
                        grant_id: binding.grant_id,
                        grant_version: binding.grant_version,
                        grant_hash: binding.grant_hash,
                        turn_digest: binding.turn_digest,
                        definition_digest: binding.definition_digest,
                        args_digest,
                        owner_lease_epoch: binding.owner.lease_epoch,
                        idempotency_key,
                    },
                ))?;
                return Ok(());
            }
            // Register an unresolved client decision before exposing the call.
            // A raw client can respond immediately after observing ToolCall.
            let immediate_approval = {
                let mut meta = meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let immediate = resolve_tool_approval(requires_approval, meta.approval_mode);
                if requires_approval && immediate.is_none() {
                    meta.pending_tool_calls.insert(call_id.clone());
                }
                immediate
            };
            let message = FromAgentMessage::ToolCall {
                call_id: call_id.clone(),
                tool_execution_id: None,
                tool,
                args,
                requires_approval,
            };
            if requires_approval {
                emit(&message)?;
            } else {
                emit_transcript(meta, crate::transcript::TranscriptLevel::Block, &message)?;
            }

            // Tools that do not require approval are auto-executed by the
            // native agent (with ToolStart/ToolOutput/ToolEnd). Do not
            // re-execute here — that double-ran side effects and omitted
            // streaming tool_output.
            //
            // For approval-gated tools, honor Init approval_mode when set.
            if let Some(approved) = immediate_approval {
                let _ =
                    tool_tx.send((call_id, approved, None, ExecutionSource::RemoteClient, None));
            }
        }
        FromAgent::ToolStart { call_id } => {
            emit_transcript(
                meta,
                crate::transcript::TranscriptLevel::Block,
                &FromAgentMessage::ToolStart { call_id },
            )?;
        }
        FromAgent::ToolOutput { call_id, content } => {
            emit_transcript(
                meta,
                crate::transcript::TranscriptLevel::Delta,
                &FromAgentMessage::ToolOutput { call_id, content },
            )?;
        }
        FromAgent::ToolEnd {
            call_id,
            success,
            receipt,
            ..
        } => {
            let tool_execution_id = {
                let mut meta = meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                meta.pending_tool_calls.remove(&call_id);
                meta.tool_execution_ids.remove(&call_id)
            };
            let terminal = tool_end_message(call_id, tool_execution_id.clone(), success, receipt);
            if tool_execution_id.is_some() {
                emit(&terminal)?;
            } else {
                emit_transcript(meta, crate::transcript::TranscriptLevel::Block, &terminal)?;
            }
        }
        FromAgent::Error {
            message,
            fatal,
            terminal,
            retryable,
        } => {
            if terminal {
                meta.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .turn_active = false;
            }
            let session_id = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id
                .clone();
            tracing::warn!(
                target: "maestro.llm",
                event = "maestro_response_failed",
                session_id = ?session_id,
                fatal,
                terminal,
                error_kind = if fatal {
                    "fatal"
                } else if terminal {
                    "terminal"
                } else {
                    "transient"
                },
                configured_model = %configured_model,
                routed_provider = routed_provider.unwrap_or(""),
            );
            emit(&FromAgentMessage::Error {
                request_id: None,
                message,
                fatal,
                terminal,
                error_type: Some(headless_error_type(fatal, terminal, retryable)),
            })?;
        }
        FromAgent::ProviderError { kind, message } => {
            meta.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .turn_active = false;
            let session_id = meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_id
                .clone();
            tracing::warn!(
                target: "maestro.llm",
                event = "maestro_provider_response_failed",
                session_id = ?session_id,
                provider_error_kind = ?kind,
                configured_model = %configured_model,
                routed_provider = routed_provider.unwrap_or(""),
            );
            emit(&FromAgentMessage::ProviderError { kind, message })?;
        }
        FromAgent::Status { message } => {
            emit_transcript(
                meta,
                crate::transcript::TranscriptLevel::Delta,
                &FromAgentMessage::Status { message },
            )?;
        }
        FromAgent::SessionInfo {
            session_id,
            cwd,
            git_branch,
        } => {
            if let Some(ref id) = session_id {
                if !id.is_empty() {
                    meta.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .session_id = Some(id.clone());
                }
            }
            emit(&FromAgentMessage::SessionInfo {
                session_id,
                cwd,
                git_branch,
            })?;
        }
        FromAgent::Compaction {
            summary,
            first_kept_entry_index,
            tokens_before,
            auto,
            custom_instructions,
            continuation,
            timestamp,
        } => {
            emit_transcript(
                meta,
                crate::transcript::TranscriptLevel::Block,
                &FromAgentMessage::Compaction {
                    summary,
                    first_kept_entry_index,
                    tokens_before,
                    auto,
                    custom_instructions,
                    continuation,
                    timestamp,
                },
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn tool_end_message(
    call_id: String,
    tool_execution_id: Option<String>,
    success: bool,
    receipt: Option<ExecutionReceipt>,
) -> FromAgentMessage {
    let tool = receipt.as_ref().map(|receipt| receipt.tool_name.clone());
    FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id,
        tool_execution_id,
        success,
        tool,
        details: None,
        receipt,
    })
}

#[cfg(test)]
pub(crate) fn tool_end_message_from_agent(event: FromAgent) -> Option<FromAgentMessage> {
    let FromAgent::ToolEnd {
        call_id,
        success,
        receipt,
        ..
    } = event
    else {
        return None;
    };
    Some(tool_end_message(call_id, None, success, receipt))
}

fn coalesce_response_chunks(chunks: &mut Vec<(String, bool)>) -> String {
    chunks
        .drain(..)
        .filter(|(_, is_thinking)| !is_thinking)
        .map(|(content, _)| content)
        .collect()
}

fn emit_transcript(
    meta: &Arc<Mutex<RuntimeMeta>>,
    level: crate::transcript::TranscriptLevel,
    message: &FromAgentMessage,
) -> Result<()> {
    let grade = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .transcript_grade;
    if grade.includes(level) {
        emit(message)?;
    }
    Ok(())
}

fn emit(msg: &FromAgentMessage) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, msg).context("serialize headless message")?;
    stdout.write_all(b"\n").context("write headless newline")?;
    stdout.flush().context("flush headless stdout")?;
    Ok(())
}

fn to_headless_usage(
    usage: crate::agent::TokenUsage,
    configured_model: &str,
    routed_provider: Option<&str>,
) -> HeadlessTokenUsage {
    let (model, provider) = reported_identity(
        configured_model,
        infer_provider_label(configured_model),
        routed_provider,
    );
    HeadlessTokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost: usage.cost,
        total_tokens: Some(usage.input_tokens + usage.output_tokens),
        model_id: Some(model),
        provider: Some(provider),
    }
}

fn managed_provider_override() -> Option<String> {
    std::env::var("MAESTRO_EVALOPS_PROVIDER")
        .ok()
        .map(|provider| provider.trim().to_string())
        .filter(|provider| !provider.is_empty())
}

pub(crate) fn resolve_headless_model(
    model_override: Option<String>,
    env: &HashMap<String, String>,
) -> String {
    model_override
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            ["MAESTRO_MODEL", "MAESTRO_DEFAULT_MODEL"]
                .into_iter()
                .filter_map(|key| env.get(key))
                .map(String::as_str)
                .map(str::trim)
                .find(|model| !model.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| crate::credential_mode::DEFAULT_MANAGED_MODEL.to_string())
}

/// Return the identity of the provider/model selected by the hosted caller.
/// Managed model prefixes remain part of the advertised binding so the hosted
/// parent can compare Ready against the exact model it passed to the child.
fn reported_identity(
    model: &str,
    fallback_provider: &str,
    routed_provider: Option<&str>,
) -> (String, String) {
    let model = model.trim();
    let managed = ["evalops/", "maestro-managed/"].into_iter().any(|prefix| {
        model
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    });
    let inferred_provider = infer_provider_label(model);
    (
        model.to_string(),
        if managed {
            routed_provider
                .filter(|provider| !provider.trim().is_empty())
                .unwrap_or(inferred_provider)
                .to_string()
        } else if inferred_provider == "OpenRouter" {
            inferred_provider.to_string()
        } else {
            fallback_provider.to_string()
        },
    )
}

fn observed_ready_identity(
    native_model: &str,
    native_provider: &str,
    routed_provider: Option<&str>,
) -> (String, String, String, String) {
    let (reported_model, reported_provider) =
        reported_identity(native_model, native_provider, routed_provider);
    (
        native_model.to_string(),
        native_provider.to_string(),
        reported_model,
        reported_provider,
    )
}

fn model_binding_mode(model: &str) -> &'static str {
    let model = model.trim();
    if ["evalops/", "maestro-managed/"].into_iter().any(|prefix| {
        model
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
    }) {
        "managed_normalized"
    } else {
        "native"
    }
}

fn infer_provider_label(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.split_once('/')
        .is_some_and(|(provider, _)| provider == "openrouter")
    {
        "OpenRouter"
    } else if m.contains("claude") || m.contains("anthropic") {
        "Anthropic"
    } else if m.contains("gemini") || m.contains("google") {
        "Google"
    } else {
        "OpenAI"
    }
}

fn env_session_id() -> Option<String> {
    normalize_session_id(std::env::var("MAESTRO_SESSION_ID").ok().as_deref())
}

/// Trim and empty-filter a raw `MAESTRO_SESSION_ID` value. Split out of
/// `env_session_id` so the trimming/filtering behavior is testable without
/// mutating the process environment (`std::env::set_var`/`remove_var` are
/// unsound to call from a test when other tests may be reading or writing
/// the environment concurrently on the same `cargo test` process).
fn normalize_session_id(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn headless_tool_result_to_agent(r: HeadlessToolResult) -> ToolResult {
    ToolResult {
        success: r.success,
        output: r.output,
        error: r.error,
        details: r.details,
    }
}

#[derive(Debug)]
struct AcceptedToolResponse {
    messages: Vec<FromAgentMessage>,
    agent_response: ToolResponseMessage,
    rollback: ToolResponseRollback,
}

#[derive(Clone, Debug)]
struct ToolResponseRollback {
    call_id: String,
    tool_execution_id: Option<String>,
    pending_client_tool: Option<PendingClientTool>,
    restore_pending: bool,
}

fn dispatch_accepted_tool_response(
    meta: &Arc<Mutex<RuntimeMeta>>,
    tool_tx: &mpsc::UnboundedSender<ToolResponseMessage>,
    accepted: AcceptedToolResponse,
    request_id: String,
) -> std::result::Result<(), String> {
    dispatch_accepted_tool_response_with(
        meta,
        tool_tx,
        accepted,
        request_id,
        |message| emit(message).map_err(|error| format!("emit tool lifecycle: {error:#}")),
        |request_id, outcome, _dropped| {
            let _ = emit(&response_consumption_message(request_id, outcome));
        },
    )
}

fn dispatch_accepted_tool_response_with<Emit, Acknowledge>(
    meta: &Arc<Mutex<RuntimeMeta>>,
    tool_tx: &mpsc::UnboundedSender<ToolResponseMessage>,
    accepted: AcceptedToolResponse,
    request_id: String,
    mut emit_lifecycle: Emit,
    acknowledge: Acknowledge,
) -> std::result::Result<(), String>
where
    Emit: FnMut(&FromAgentMessage) -> std::result::Result<(), String>,
    Acknowledge: FnOnce(String, ToolResponseConsumption, bool) + Send + 'static,
{
    let lifecycle_id = accepted.rollback.tool_execution_id.as_deref();
    let lifecycle_already_emitted = lifecycle_id.is_some_and(|execution_id| {
        meta.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .emitted_client_tool_terminals
            .contains(execution_id)
    });
    if !lifecycle_already_emitted {
        for message in &accepted.messages {
            if let Err(error) = emit_lifecycle(message) {
                rollback_accepted_tool_response(meta, accepted.rollback);
                return Err(error);
            }
        }
        if !accepted.messages.is_empty() {
            if let Some(execution_id) = lifecycle_id {
                meta.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .emitted_client_tool_terminals
                    .insert(execution_id.to_string());
            }
        }
    }
    // A queued response can still be rejected by the consumption receipt (for
    // example when the call is cancelled before native consumption). Restore
    // the pending decision before surfacing the receipt so a corrected retry
    // is not refused as "not awaiting a decision". A dropped receipt means
    // the native side is gone: keep the execution binding so the shutdown
    // cleanup can still emit the interrupted ToolEnd that closes the exposed
    // tool lifecycle.
    let rejection_rollback = accepted.rollback.clone();
    let rejection_meta = Arc::clone(meta);
    if send_tool_response_with_consumption_ack_using(
        meta,
        tool_tx,
        accepted.agent_response,
        request_id,
        move |request_id, outcome, dropped| {
            if matches!(outcome, ToolResponseConsumption::Rejected { .. }) {
                if dropped {
                    rollback_dropped_tool_response(&rejection_meta, rejection_rollback);
                } else {
                    rollback_accepted_tool_response(&rejection_meta, rejection_rollback);
                }
            }
            acknowledge(request_id, outcome, dropped);
        },
    ) {
        return Ok(());
    }
    rollback_accepted_tool_response(meta, accepted.rollback);
    Err("native tool response channel is closed".to_string())
}

fn rollback_accepted_tool_response(meta: &Arc<Mutex<RuntimeMeta>>, rollback: ToolResponseRollback) {
    if !rollback.restore_pending {
        return;
    }
    let mut meta = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    meta.pending_tool_calls.insert(rollback.call_id.clone());
    if let Some(pending) = rollback.pending_client_tool {
        meta.pending_client_tools
            .insert(rollback.call_id.clone(), pending);
    }
    meta.tool_execution_ids.remove(&rollback.call_id);
    if let Some(tool_execution_id) = rollback.tool_execution_id {
        meta.decided_tool_execution_ids.remove(&tool_execution_id);
    }
}

/// Rollback for a response whose consumption receipt was dropped: the native
/// side shut down without consuming the queued message. The pending decision
/// and the reserved governed decision are restored like
/// [`rollback_accepted_tool_response`], but the `tool_execution_ids` binding
/// is deliberately preserved so [`take_interrupted_tool_terminal_messages`]
/// can still emit the interrupted `ToolEnd` for the lifecycle that was
/// already exposed to clients.
fn rollback_dropped_tool_response(meta: &Arc<Mutex<RuntimeMeta>>, rollback: ToolResponseRollback) {
    if !rollback.restore_pending {
        return;
    }
    let mut meta = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    meta.pending_tool_calls.insert(rollback.call_id.clone());
    if let Some(pending) = rollback.pending_client_tool {
        meta.pending_client_tools
            .insert(rollback.call_id.clone(), pending);
    }
    if let Some(tool_execution_id) = rollback.tool_execution_id {
        meta.decided_tool_execution_ids.remove(&tool_execution_id);
    }
}

fn send_tool_response_with_consumption_ack(
    meta: &Arc<Mutex<RuntimeMeta>>,
    tool_tx: &mpsc::UnboundedSender<ToolResponseMessage>,
    response: ToolResponseMessage,
    request_id: String,
) -> bool {
    send_tool_response_with_consumption_ack_using(
        meta,
        tool_tx,
        response,
        request_id,
        |request_id, outcome, _dropped| {
            let _ = emit(&response_consumption_message(request_id, outcome));
        },
    )
}

/// Record a detached consumption-receipt acknowledgement task so shutdown can
/// drain it before the process exits. Completed handles are pruned in place.
fn record_receipt_task(meta: &Arc<Mutex<RuntimeMeta>>, task: tokio::task::JoinHandle<()>) {
    let registry = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .receipt_tasks
        .clone();
    let mut tasks = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tasks.retain(|task| !task.is_finished());
    tasks.push(task);
}

/// Take the outstanding receipt acknowledgement tasks for draining. After the
/// native agent shuts down, every receipt sender is either consumed or
/// dropped, so awaiting these completes promptly and guarantees the dropped
/// receipts' protocol errors and rollbacks are emitted before exit.
fn take_receipt_tasks(meta: &Arc<Mutex<RuntimeMeta>>) -> Vec<tokio::task::JoinHandle<()>> {
    let registry = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .receipt_tasks
        .clone();
    let mut tasks = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *tasks)
}

fn response_consumption_message(
    request_id: String,
    outcome: ToolResponseConsumption,
) -> FromAgentMessage {
    match outcome {
        ToolResponseConsumption::Accepted => {
            FromAgentMessage::ResponseAccepted(tool_wire::ResponseAccepted { request_id })
        }
        ToolResponseConsumption::Rejected { reason } => FromAgentMessage::Error {
            request_id: Some(request_id),
            message: reason,
            fatal: false,
            terminal: false,
            error_type: Some(HeadlessErrorType::Protocol),
        },
    }
}

fn send_tool_response_with_consumption_ack_using<Acknowledge>(
    meta: &Arc<Mutex<RuntimeMeta>>,
    tool_tx: &mpsc::UnboundedSender<ToolResponseMessage>,
    mut response: ToolResponseMessage,
    request_id: String,
    acknowledge: Acknowledge,
) -> bool
where
    Acknowledge: FnOnce(String, ToolResponseConsumption, bool) + Send + 'static,
{
    let (consumed_tx, consumed_rx) = tokio::sync::oneshot::channel();
    response.4 = Some(consumed_tx);
    if tool_tx.send(response).is_err() {
        return false;
    }
    let task = tokio::spawn(async move {
        // A dropped receipt sender means the native agent shut down or the
        // receiver was dropped before consuming the queued message. Treat it
        // as a rejection so the acknowledgement/rollback path restores the
        // pending decision instead of leaving the request stuck; the flag
        // lets the caller distinguish the dropped case from an explicit
        // rejection.
        let (outcome, dropped) = match consumed_rx.await {
            Ok(outcome) => (outcome, false),
            Err(_) => (
                ToolResponseConsumption::Rejected {
                    reason: "native agent dropped the response before consuming it".to_string(),
                },
                true,
            ),
        };
        acknowledge(request_id, outcome, dropped);
    });
    record_receipt_task(meta, task);
    true
}

fn prepare_tool_response(
    meta: &Arc<Mutex<RuntimeMeta>>,
    call_id: String,
    tool_execution_id: Option<String>,
    approved: bool,
    result: Option<HeadlessToolResult>,
) -> std::result::Result<AcceptedToolResponse, String> {
    let mut meta = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(active_execution_id) = meta.tool_execution_ids.get(&call_id) {
        return Err(format!(
            "tool call {call_id} already has an active decision for execution {active_execution_id}"
        ));
    }
    if !meta.pending_tool_calls.contains(&call_id) {
        return Err(format!("tool call {call_id} is not awaiting a decision"));
    }
    if !meta.reserve_tool_decision(tool_execution_id.as_deref()) {
        return Err(format!(
            "tool execution {} already has a decision",
            tool_execution_id
                .as_deref()
                .expect("only governed decisions can be duplicates")
        ));
    }
    meta.pending_tool_calls.remove(&call_id);

    let agent_result = result.map(headless_tool_result_to_agent);
    // When the client supplies a completed result, surface the full lifecycle.
    // When only an approval is returned, bind the durable id to the native end.
    let messages = if approved {
        if let Some(ref tool_result) = agent_result {
            tool_lifecycle_messages(&call_id, tool_execution_id.as_deref(), None, tool_result)
        } else {
            if let Some(ref tool_execution_id) = tool_execution_id {
                meta.tool_execution_ids
                    .insert(call_id.clone(), tool_execution_id.clone());
            }
            Vec::new()
        }
    } else {
        denied_tool_terminal_message(
            &call_id,
            tool_execution_id.as_deref(),
            agent_result.as_ref(),
        )
        .into_iter()
        .collect()
    };
    drop(meta);

    let rollback = ToolResponseRollback {
        call_id: call_id.clone(),
        tool_execution_id: tool_execution_id.clone(),
        pending_client_tool: None,
        restore_pending: true,
    };
    Ok(AcceptedToolResponse {
        messages,
        agent_response: (
            call_id,
            approved,
            agent_result,
            ExecutionSource::RemoteClient,
            None,
        ),
        rollback,
    })
}

#[derive(Debug, Clone, Default)]
struct ClientToolResultBinding {
    tool_execution_id: Option<String>,
    client_instance_id: Option<String>,
    grant_id: Option<String>,
    grant_version: Option<u64>,
    grant_hash: Option<String>,
    turn_digest: Option<String>,
    definition_digest: Option<String>,
    args_digest: Option<String>,
    owner_lease_epoch: Option<u64>,
    idempotency_key: Option<String>,
}

#[derive(Debug)]
enum ClientToolResultPreparationError {
    Protocol(String),
    Expired {
        message: String,
        resolution: Box<AcceptedToolResponse>,
    },
}

impl ClientToolResultPreparationError {
    #[cfg(test)]
    fn contains(&self, needle: &str) -> bool {
        match self {
            Self::Protocol(message) | Self::Expired { message, .. } => message.contains(needle),
        }
    }

    fn into_parts(self) -> (String, Option<AcceptedToolResponse>) {
        match self {
            Self::Protocol(message) => (message, None),
            Self::Expired {
                message,
                resolution,
            } => (message, Some(*resolution)),
        }
    }
}

fn dispatch_client_tool_preparation_error(
    meta: &Arc<Mutex<RuntimeMeta>>,
    tool_tx: &mpsc::UnboundedSender<ToolResponseMessage>,
    call_id: String,
    error: ClientToolResultPreparationError,
) -> Result<()> {
    let (message, resolution) = error.into_parts();
    if let Some(resolution) = resolution {
        if let Err(dispatch_error) =
            dispatch_accepted_tool_response(meta, tool_tx, resolution, call_id.clone())
        {
            return protocol_error(
                Some(call_id),
                format!("{message}; failed to terminalize expired tool: {dispatch_error}"),
            );
        }
    }
    protocol_error(Some(call_id), message)
}

fn handle_governed_client_tool_result(
    state: &HeadlessState,
    message: ToAgentMessage,
) -> Result<()> {
    let ToAgentMessage::GovernedClientToolResult(tool_wire::GovernedClientToolResult {
        process_tool_cost_micros,
        call_id,
        content,
        is_error,
        tool_execution_id,
        client_instance_id,
        grant_id,
        grant_version,
        grant_hash,
        turn_digest,
        definition_digest,
        args_digest,
        owner_lease_epoch,
        idempotency_key,
    }) = message
    else {
        anyhow::bail!("expected governed client tool result");
    };
    let Some(tool_tx) = state.tool_tx.as_ref() else {
        protocol_error(Some(call_id), "no pending native client-tool request")?;
        return Ok(());
    };
    let process_budget = state
        .meta
        .lock()
        .map_err(|_| anyhow::anyhow!("runtime metadata poisoned"))?
        .process_budget
        .clone();
    if process_budget.is_some() != process_tool_cost_micros.is_some() {
        protocol_error(
            Some(call_id),
            "process tool result requires its usage charge",
        )?;
        return Ok(());
    }
    let process_execution_id = tool_execution_id.clone();
    match prepare_client_tool_result(
        &state.meta,
        call_id.clone(),
        content,
        is_error,
        ClientToolResultBinding {
            tool_execution_id: Some(tool_execution_id),
            client_instance_id: Some(client_instance_id),
            grant_id: Some(grant_id),
            grant_version: Some(grant_version),
            grant_hash: Some(grant_hash),
            turn_digest: Some(turn_digest),
            definition_digest: Some(definition_digest),
            args_digest: Some(args_digest),
            owner_lease_epoch: Some(owner_lease_epoch),
            idempotency_key: Some(idempotency_key),
        },
    ) {
        Ok(accepted) => {
            if let (Some(budget), Some(cost)) = (process_budget, process_tool_cost_micros) {
                budget
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process budget poisoned"))?
                    .charge_tool(&process_execution_id, cost)
                    .map_err(anyhow::Error::msg)?;
            }
            match dispatch_accepted_tool_response(&state.meta, tool_tx, accepted, call_id.clone()) {
                Ok(()) => {}
                Err(message) => protocol_error(Some(call_id), message)?,
            }
        }
        Err(error) => dispatch_client_tool_preparation_error(&state.meta, tool_tx, call_id, error)?,
    }
    Ok(())
}

fn prepare_client_tool_result(
    meta: &Arc<Mutex<RuntimeMeta>>,
    call_id: String,
    content: Vec<ClientToolResultContent>,
    is_error: bool,
    supplied: ClientToolResultBinding,
) -> std::result::Result<AcceptedToolResponse, ClientToolResultPreparationError> {
    let mut meta = meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !meta.pending_tool_calls.contains(&call_id) {
        return Err(ClientToolResultPreparationError::Protocol(format!(
            "tool call {call_id} is not awaiting a decision"
        )));
    }
    let mut pending = meta.pending_client_tools.get(&call_id).cloned();
    if pending.is_none() && supplied.tool_execution_id.is_some() {
        return Err(ClientToolResultPreparationError::Protocol(format!(
            "governed client tool result has no governed request binding for {call_id}"
        )));
    }
    if let Some(expected) = pending.as_mut() {
        let matches = supplied.tool_execution_id.as_deref()
            == Some(expected.tool_execution_id.as_str())
            && supplied.client_instance_id.as_deref()
                == Some(expected.binding.owner.client_instance_id.as_str())
            && supplied.grant_id.as_deref() == Some(expected.binding.grant_id.as_str())
            && supplied.grant_version == Some(expected.binding.grant_version)
            && supplied.grant_hash.as_deref() == Some(expected.binding.grant_hash.as_str())
            && supplied.turn_digest.as_deref() == Some(expected.binding.turn_digest.as_str())
            && supplied.definition_digest.as_deref()
                == Some(expected.binding.definition_digest.as_str())
            && supplied.args_digest.as_deref() == Some(expected.args_digest.as_str())
            && supplied.owner_lease_epoch == Some(expected.binding.owner.lease_epoch)
            && supplied.idempotency_key.as_deref() == Some(expected.idempotency_key.as_str());
        if !matches {
            return Err(ClientToolResultPreparationError::Protocol(format!(
                "governed client tool result binding mismatch for {call_id}"
            )));
        }
        if chrono::Utc::now().timestamp_millis() > expected.binding.expires_at_ms {
            let message = format!("governed client tool execution lease expired for {call_id}");
            let pending = pending
                .take()
                .expect("expired governed client tool is pending");
            meta.pending_tool_calls.remove(&call_id);
            meta.pending_client_tools.remove(&call_id);
            drop(meta);
            let result = ToolResult::failure(message.clone());
            return Err(ClientToolResultPreparationError::Expired {
                message,
                resolution: Box::new(AcceptedToolResponse {
                    messages: tool_lifecycle_messages(
                        &call_id,
                        Some(&pending.tool_execution_id),
                        Some(pending.binding.logical_name.clone()),
                        &result,
                    ),
                    agent_response: (
                        call_id.clone(),
                        true,
                        Some(result),
                        ExecutionSource::RemoteClient,
                        None,
                    ),
                    rollback: ToolResponseRollback {
                        call_id,
                        tool_execution_id: Some(pending.tool_execution_id),
                        pending_client_tool: None,
                        restore_pending: false,
                    },
                }),
            });
        }
        let result_digest = canonical_json_digest(&serde_json::json!({
            "content": &content,
            "is_error": is_error,
        }))
        .map_err(|error| {
            ClientToolResultPreparationError::Protocol(format!(
                "digest governed client tool result: {error:#}"
            ))
        })?;
        if expected
            .result_digest
            .as_ref()
            .is_some_and(|prior| prior != &result_digest)
        {
            return Err(ClientToolResultPreparationError::Protocol(format!(
                "governed client tool result changed across retry for {call_id}"
            )));
        }
        expected.result_digest = Some(result_digest);
    }
    meta.pending_tool_calls.remove(&call_id);
    meta.pending_client_tools.remove(&call_id);
    drop(meta);

    let result = client_content_to_agent_result(content, is_error);
    let tool_execution_id = pending
        .as_ref()
        .map(|pending| pending.tool_execution_id.as_str());
    let tool = pending
        .as_ref()
        .map(|pending| pending.binding.logical_name.clone());
    Ok(AcceptedToolResponse {
        messages: tool_lifecycle_messages(&call_id, tool_execution_id, tool, &result),
        agent_response: (
            call_id.clone(),
            true,
            Some(result),
            ExecutionSource::RemoteClient,
            None,
        ),
        rollback: ToolResponseRollback {
            call_id,
            tool_execution_id: pending
                .as_ref()
                .map(|pending| pending.tool_execution_id.clone()),
            pending_client_tool: pending,
            restore_pending: true,
        },
    })
}

/// Decide whether headless should immediately resolve an approval-gated tool.
///
/// - `None` → leave to the native agent (auto-exec) or wait for the client
/// - `Some(true)` → approve; native agent executes and streams tool events
/// - `Some(false)` → deny
fn resolve_tool_approval(
    requires_approval: bool,
    approval_mode: Option<ApprovalMode>,
) -> Option<bool> {
    if !requires_approval {
        // Native agent auto-executes; headless must not inject a tool_response.
        return None;
    }
    match approval_mode {
        Some(ApprovalMode::Auto) => Some(true),
        Some(ApprovalMode::Fail) => Some(false),
        Some(ApprovalMode::Prompt) | None => None,
    }
}

/// Content for a `tool_output` event from a completed tool result.
fn tool_output_content(result: &ToolResult) -> Option<String> {
    if !result.output.is_empty() {
        return Some(result.output.clone());
    }
    if !result.success {
        return Some(format!(
            "Error: {}",
            result.error.as_deref().unwrap_or("tool failed")
        ));
    }
    None
}

/// A governed denial never executes natively, so it has no native `ToolEnd`.
/// Emit the correlated terminal failure directly when the controller supplied
/// a durable execution id.
fn denied_tool_terminal_message(
    call_id: &str,
    tool_execution_id: Option<&str>,
    result: Option<&ToolResult>,
) -> Option<FromAgentMessage> {
    Some(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: call_id.to_string(),
        tool_execution_id: Some(tool_execution_id?.to_string()),
        success: false,
        tool: None,
        details: result.and_then(|result| result.details.clone()),
        receipt: None,
    }))
}

/// Protocol messages for a completed tool run: start → output? → end.
fn tool_lifecycle_messages(
    call_id: &str,
    tool_execution_id: Option<&str>,
    tool: Option<String>,
    result: &ToolResult,
) -> Vec<FromAgentMessage> {
    let mut msgs = Vec::with_capacity(3);
    msgs.push(FromAgentMessage::ToolStart {
        call_id: call_id.to_string(),
    });
    if let Some(content) = tool_output_content(result) {
        msgs.push(FromAgentMessage::ToolOutput {
            call_id: call_id.to_string(),
            content,
        });
    }
    msgs.push(FromAgentMessage::ToolEnd(tool_wire::ToolEnd {
        call_id: call_id.to_string(),
        tool_execution_id: tool_execution_id.map(str::to_string),
        success: result.success,
        tool,
        details: result.details.clone(),
        receipt: None,
    }));
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    const TEST_GRANT_KEY_ID: &str = "test-key";
    const TEST_GRANT_KEY_SEED: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn terminal_retryability_survives_the_headless_boundary() {
        assert_eq!(
            headless_error_type(false, true, true),
            HeadlessErrorType::Transient
        );
        assert_eq!(
            headless_error_type(false, true, false),
            HeadlessErrorType::Protocol
        );
        assert_eq!(
            headless_error_type(true, true, true),
            HeadlessErrorType::Fatal
        );
    }

    fn test_grant_key_pair() -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(TEST_GRANT_KEY_SEED).expect("test signing key")
    }

    fn test_grant_context() -> GovernedGrantVerificationContext<'static> {
        GovernedGrantVerificationContext {
            organization_id: "org-1",
            workspace_id: "workspace-1",
            thread_id: "thread-1",
            turn_id: "turn-1",
            run_id: "run-1",
            runtime_generation: 7,
        }
    }

    fn sign_test_grant(grant: &mut GovernedToolGrant) {
        let canonical = serde_json::to_vec(&governed_grant_canonical_value(grant)).unwrap();
        grant.grant_hash = format!("sha256:{}", sha256_hex(&canonical));
        grant.grant_signature = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD
                .encode(test_grant_key_pair().sign(&canonical).as_ref())
        );
    }

    /// A field Runner Host signs and this resident cannot represent must fail
    /// deserialization and name itself.
    ///
    /// Without `deny_unknown_fields` serde drops it, `governed_grant_canonical_value`
    /// rebuilds the grant without it, the recomputed hash no longer matches the
    /// one Runner Host signed, and the resident answers `thread.append` with an
    /// opaque `bad_request`. That is how mono #7674 reached production: the
    /// deployed resident predated the `identity_authorization` field, dropped
    /// it, and rejected every governed Dex turn without naming a cause.
    #[test]
    fn unknown_signed_grant_field_fails_loudly_instead_of_silently_rehashing() {
        let mut value = serde_json::to_value(test_grant()).expect("serialize a governed grant");
        value["future_authority_binding"] = serde_json::json!("sha256:unknown-to-this-resident");
        let error = serde_json::from_value::<GovernedToolGrant>(value)
            .expect_err("a signed field this resident cannot represent must not parse");
        assert!(
            error.to_string().contains("future_authority_binding"),
            "deserialization error must name the unknown signed field: {error}"
        );
    }

    fn test_grant() -> GovernedToolGrant {
        let mut grant = GovernedToolGrant {
            process_budget: None,
            process_system_prompt: None,
            envelope_version: 2,
            grant_id: "grant-1".to_string(),
            grant_version: 1,
            issuer: GOVERNED_GRANT_ISSUER.to_string(),
            audience: GOVERNED_GRANT_AUDIENCE.to_string(),
            organization_id: "org-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            run_id: "run-1".to_string(),
            runtime_generation: 7,
            grant_epoch: 7,
            issued_at_ms: 900,
            not_before_ms: 900,
            expires_at_ms: 2_000,
            grant_hash: String::new(),
            signing_key_id: TEST_GRANT_KEY_ID.to_string(),
            grant_signature: String::new(),
            identity_authorization: Some(
                crate::headless::messages::IdentityToolAuthorizationEvidence {
                    schema_version: "identity.tool_authorization.v1".into(),
                    organization_id: "org-1".into(),
                    workspace_id: "workspace-1".into(),
                    application_id: "deixic".into(),
                    subject_id: "user-1".into(),
                    actor_chain_digest: format!("sha256:{}", "a".repeat(64)),
                    decision_id: "decision-1".into(),
                    authorization_lineage_id: "lineage-1".into(),
                    policy_id: "policy-1".into(),
                    policy_version: "v1".into(),
                    policy_digest: "b".repeat(64),
                    authorization_fingerprint: format!("authz_fingerprint_v1_{}", "c".repeat(64)),
                    capability_digest: format!("sha256:{}", "d".repeat(64)),
                    action_digest: format!("sha256:{}", "e".repeat(64)),
                    audience: GOVERNED_GRANT_AUDIENCE.into(),
                    issued_at_ms: 900,
                    expires_at_ms: 2_000,
                    revocation_epoch: 7,
                },
            ),
            native_tool_ids: vec!["bash".to_string()],
            external_tools: Vec::new(),
            connection_bindings: Vec::new(),
        };
        sign_test_grant(&mut grant);
        grant
    }

    #[test]
    fn process_budget_is_signed_and_cannot_be_widened_by_grant_replay() {
        let mut grant = test_grant();
        assert!(
            governed_grant_canonical_value(&grant)
                .get("process_budget")
                .is_none()
        );
        grant.process_system_prompt = Some("Handle the admitted process event.".into());
        grant.process_budget = Some(crate::agent::process_budget::ProcessBudgetLimits {
            event_id: "turn-1".into(),
            max_requests: 2,
            max_total_tokens: 10,
            max_cost_micros: 20,
            cost_micros_per_token: 2,
        });
        sign_test_grant(&mut grant);
        verify_governed_tool_grant_with_keys(
            &grant,
            &test_grant_context(),
            1_000,
            &test_grant_keys(),
        )
        .unwrap();
        let signed = grant.clone();
        grant.process_system_prompt = Some("Replace the admitted instructions.".into());
        grant.grant_hash = format!(
            "sha256:{}",
            sha256_hex(&governed_grant_canonical_bytes(&grant).unwrap())
        );
        assert!(
            verify_governed_tool_grant_with_keys(
                &grant,
                &test_grant_context(),
                1_000,
                &test_grant_keys()
            )
            .is_err()
        );
        grant = signed.clone();
        grant.process_budget = None;
        sign_test_grant(&mut grant);
        assert!(
            verify_governed_tool_grant_with_keys(
                &grant,
                &test_grant_context(),
                1_000,
                &test_grant_keys()
            )
            .is_err()
        );
        grant = signed;
        grant.process_budget.as_mut().unwrap().max_total_tokens = 100;
        let canonical = governed_grant_canonical_bytes(&grant).unwrap();
        grant.grant_hash = format!("sha256:{}", sha256_hex(&canonical));
        assert!(
            verify_governed_tool_grant_with_keys(
                &grant,
                &test_grant_context(),
                1_000,
                &test_grant_keys()
            )
            .is_err()
        );
        grant.process_budget.as_mut().unwrap().max_requests = 0;
        sign_test_grant(&mut grant);
        assert!(
            verify_governed_tool_grant_with_keys(
                &grant,
                &test_grant_context(),
                1_000,
                &test_grant_keys()
            )
            .is_err()
        );
    }

    #[test]
    fn managed_lineage_is_stable_within_and_distinct_across_authenticated_threads() {
        let first = test_grant();
        let replay = first.clone();
        let mut other_thread = first.clone();
        other_thread.thread_id = "thread-2".to_string();

        let first_lineage = managed_request_lineage_id(&first);
        assert_eq!(first_lineage, managed_request_lineage_id(&replay));
        assert_ne!(first_lineage, managed_request_lineage_id(&other_thread));
        assert!(first_lineage.starts_with("maestro-turn-v2:"));
    }

    fn test_grant_keys() -> HashMap<String, GovernedGrantPublicKey> {
        HashMap::from([(
            TEST_GRANT_KEY_ID.to_string(),
            GovernedGrantPublicKey {
                algorithm: GovernedGrantPublicKeyAlgorithm::Ed25519,
                public_key: base64::engine::general_purpose::STANDARD
                    .encode(test_grant_key_pair().public_key().as_ref()),
                state: GovernedGrantPublicKeyState::Active,
            },
        )])
    }

    #[test]
    fn governed_grant_verification_rejects_tampering_scope_expiry_and_unknown_versions() {
        let grant = test_grant();
        assert_eq!(
            grant.grant_hash,
            "sha256:69ae15d769fad4ef89358b3d90f648372c12b30c09d9fa9f8b3465ebcca0f046"
        );
        assert!(grant.grant_signature.starts_with("ed25519:"));
        let context = test_grant_context();
        let keys = test_grant_keys();
        verify_governed_tool_grant_with_keys(&grant, &context, 1_000, &keys)
            .expect("valid signed grant");

        let mut tampered = grant.clone();
        tampered.native_tool_ids = vec!["read".to_string()];
        let canonical = serde_json::to_vec(&governed_grant_canonical_value(&tampered)).unwrap();
        tampered.grant_hash = format!("sha256:{}", sha256_hex(&canonical));
        assert!(
            verify_governed_tool_grant_with_keys(&tampered, &context, 1_000, &keys)
                .unwrap_err()
                .to_string()
                .contains("signature mismatch"),
            "rehashing a tampered payload must not forge Platform authority"
        );

        let mut wrong_audience = grant.clone();
        wrong_audience.audience = "another-runtime".to_string();
        sign_test_grant(&mut wrong_audience);
        assert!(
            verify_governed_tool_grant_with_keys(&wrong_audience, &context, 1_000, &keys).is_err()
        );

        let mut wrong_application = grant.clone();
        wrong_application
            .identity_authorization
            .as_mut()
            .expect("Identity authority")
            .application_id = "browser-selected-app".into();
        sign_test_grant(&mut wrong_application);
        assert!(
            verify_governed_tool_grant_with_keys(&wrong_application, &context, 1_000, &keys)
                .is_err()
        );

        let mut stale_epoch = grant.clone();
        stale_epoch
            .identity_authorization
            .as_mut()
            .expect("Identity authority")
            .revocation_epoch = 6;
        sign_test_grant(&mut stale_epoch);
        assert!(
            verify_governed_tool_grant_with_keys(&stale_epoch, &context, 1_000, &keys).is_err()
        );

        let mut tampered_subject = grant.clone();
        tampered_subject
            .identity_authorization
            .as_mut()
            .expect("Identity authority")
            .subject_id = "user-2".into();
        assert!(
            verify_governed_tool_grant_with_keys(&tampered_subject, &context, 1_000, &keys)
                .is_err()
        );

        let wrong_scope = GovernedGrantVerificationContext {
            workspace_id: "workspace-2",
            ..context.clone()
        };
        assert!(verify_governed_tool_grant_with_keys(&grant, &wrong_scope, 1_000, &keys).is_err());
        assert!(verify_governed_tool_grant_with_keys(&grant, &context, 2_001, &keys).is_err());
        assert!(
            verify_governed_tool_grant_with_keys(&grant, &context, 1_000, &HashMap::new()).is_err()
        );

        let mut inactive_keys = keys.clone();
        inactive_keys.get_mut(TEST_GRANT_KEY_ID).unwrap().state =
            GovernedGrantPublicKeyState::Inactive;
        assert!(
            verify_governed_tool_grant_with_keys(&grant, &context, 1_000, &inactive_keys).is_err()
        );

        let mut unknown_version = grant;
        unknown_version.envelope_version = 3;
        sign_test_grant(&mut unknown_version);
        assert!(
            verify_governed_tool_grant_with_keys(&unknown_version, &context, 1_000, &keys).is_err()
        );
    }

    #[test]
    fn signed_connection_binding_is_scoped_and_tamper_evident() {
        let mut grant = test_grant();
        grant.connection_bindings = vec![crate::headless::messages::ConnectionGrantBinding {
            binding_id: "github-release".into(),
            connection_id: "github-work".into(),
            provider_id: "github".into(),
            generation: 4,
            placement: crate::service_connections::ConnectionPlacement::Local,
            capabilities: vec!["releases.read".into(), "releases.write".into()],
            resources: vec!["repo:evalops/maestro-internal".into()],
            policy_hash: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
        }];
        grant.external_tools = vec![ExternalToolDefinition {
            tool_id: "publish-release".into(),
            name: "publish_release".into(),
            description: "Publish an approved release".into(),
            input_schema: json!({"type":"object"}),
            execution_owner: ClientToolExecutionOwner {
                client_instance_id: "client-1".into(),
                lease_epoch: 1,
            },
            connection_binding_id: Some("github-release".into()),
            metadata: None,
        }];
        sign_test_grant(&mut grant);
        verify_governed_tool_grant_with_keys(
            &grant,
            &test_grant_context(),
            1_000,
            &test_grant_keys(),
        )
        .expect("connection authority is part of the signed grant");
        let (_, _, bindings) = governed_agent_inputs(&grant).unwrap();
        assert_eq!(
            bindings
                .values()
                .next()
                .unwrap()
                .connection_binding_id
                .as_deref(),
            Some("github-release")
        );

        let mut tampered = grant.clone();
        tampered.connection_bindings[0].capabilities[1] = "releases.admin".into();
        assert!(
            verify_governed_tool_grant_with_keys(
                &tampered,
                &test_grant_context(),
                1_000,
                &test_grant_keys(),
            )
            .is_err()
        );

        let mut unknown = grant.clone();
        unknown.external_tools[0].connection_binding_id = Some("unknown".into());
        sign_test_grant(&mut unknown);
        let error = match governed_agent_inputs(&unknown) {
            Ok(_) => panic!("unknown connection binding must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unknown connection binding"));
    }

    #[test]
    fn governed_grant_public_key_set_rejects_secret_fields_and_inactive_signers() {
        let public_key = base64::engine::general_purpose::STANDARD
            .encode(test_grant_key_pair().public_key().as_ref());
        let parsed = governed_grant_keys_from_json(
            &json!({
                "active": {
                    "algorithm": "ed25519",
                    "public_key": public_key,
                    "state": "active"
                },
                "previous": {
                    "algorithm": "ed25519",
                    "public_key": public_key,
                    "state": "retiring"
                }
            })
            .to_string(),
        )
        .expect("public-only rotation set");
        assert_eq!(parsed.len(), 2);

        for secret_field in ["private_key", "private_key_pkcs8", "secret", "hmac_key"] {
            let mut entry = json!({
                "algorithm": "ed25519",
                "public_key": public_key,
                "state": "active"
            });
            entry[secret_field] = json!("must-never-enter-the-resident");
            assert!(
                governed_grant_keys_from_json(&json!({"active": entry}).to_string()).is_err(),
                "secret-shaped field {secret_field} must fail closed"
            );
        }

        let mut grant = test_grant();
        grant.signing_key_id = "inactive".into();
        sign_test_grant(&mut grant);
        let inactive = HashMap::from([(
            "inactive".to_string(),
            GovernedGrantPublicKey {
                algorithm: GovernedGrantPublicKeyAlgorithm::Ed25519,
                public_key,
                state: GovernedGrantPublicKeyState::Inactive,
            },
        )]);
        assert!(
            verify_governed_tool_grant_with_keys(&grant, &test_grant_context(), 1_000, &inactive)
                .is_err()
        );
    }

    #[test]
    fn governed_grant_environment_accepts_only_public_rotation_material() {
        let _guard = GOVERNED_GRANT_ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os(GOVERNED_GRANT_PUBLIC_KEYS_ENV);
        let public_key = base64::engine::general_purpose::STANDARD
            .encode(test_grant_key_pair().public_key().as_ref());
        let value = json!({
            "active": {
                "algorithm": "ed25519",
                "public_key": public_key,
                "state": "active"
            },
            "previous": {
                "algorithm": "ed25519",
                "public_key": public_key,
                "state": "retiring"
            }
        })
        .to_string();
        // SAFETY: this test holds the module-local lock for this dedicated env key
        // and restores its prior value before releasing the lock.
        unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, value) };
        let parsed = governed_grant_keys_from_env().expect("public rotation env");
        assert_eq!(parsed.len(), 2);
        assert_eq!(governed_grant_verifier_algorithms(), ["ed25519"]);

        let secret = json!({
            "active": {
                "algorithm": "ed25519",
                "public_key": public_key,
                "state": "active",
                "private_key_pkcs8": "forbidden"
            }
        })
        .to_string();
        // SAFETY: guarded and restored as above.
        unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, secret) };
        assert!(governed_grant_keys_from_env().is_err());
        assert!(governed_grant_verifier_algorithms().is_empty());

        let inactive = json!({
            "retired": {
                "algorithm": "ed25519",
                "public_key": public_key,
                "state": "inactive"
            }
        })
        .to_string();
        // SAFETY: guarded and restored as above.
        unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, inactive) };
        assert!(governed_grant_keys_from_env().is_err());
        assert!(governed_grant_verifier_algorithms().is_empty());

        let blank_key_id = json!({
            "  ": {
                "algorithm": "ed25519",
                "public_key": base64::engine::general_purpose::STANDARD
                    .encode(test_grant_key_pair().public_key().as_ref()),
                "state": "active"
            }
        })
        .to_string();
        // SAFETY: guarded and restored as above.
        unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, blank_key_id) };
        assert!(governed_grant_keys_from_env().is_err());
        assert!(governed_grant_verifier_algorithms().is_empty());

        for malformed in ["not-base64", "c2hvcnQ="] {
            let malformed_retiring = json!({
                "previous": {
                    "algorithm": "ed25519",
                    "public_key": malformed,
                    "state": "retiring"
                }
            })
            .to_string();
            // SAFETY: guarded and restored as above.
            unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, malformed_retiring) };
            assert!(governed_grant_keys_from_env().is_err());
        }

        let invalid_point = json!({
            "active": {
                "algorithm": "ed25519",
                "public_key": base64::engine::general_purpose::STANDARD.encode([7_u8; 32]),
                "state": "active"
            }
        })
        .to_string();
        // SAFETY: guarded and restored as above.
        unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, invalid_point) };
        assert!(governed_grant_keys_from_env().is_err());
        assert!(governed_grant_verifier_algorithms().is_empty());

        let mut weak_public_key = [0_u8; 32];
        weak_public_key[0] = 1;
        let weak_point = json!({
            "active": {
                "algorithm": "ed25519",
                "public_key": base64::engine::general_purpose::STANDARD.encode(weak_public_key),
                "state": "active"
            }
        })
        .to_string();
        // SAFETY: guarded and restored as above.
        unsafe { std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, weak_point) };
        assert!(governed_grant_keys_from_env().is_err());
        assert!(governed_grant_verifier_algorithms().is_empty());

        // SAFETY: guarded and restored as above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV, value),
                None => std::env::remove_var(GOVERNED_GRANT_PUBLIC_KEYS_ENV),
            }
        }
    }

    #[test]
    fn authenticated_governed_grant_can_explicitly_deny_all_tools() {
        let mut grant = test_grant();
        grant.native_tool_ids.clear();
        grant.external_tools.clear();
        sign_test_grant(&mut grant);

        verify_governed_tool_grant_with_keys(
            &grant,
            &test_grant_context(),
            1_000,
            &test_grant_keys(),
        )
        .expect("signed zero-capability grant remains authenticated");
        let (allowed, external, bindings) = governed_agent_inputs(&grant)
            .expect("ordinary governed chat must not require ambient tool authority");
        assert!(allowed.is_empty());
        assert!(external.is_empty());
        assert!(bindings.is_empty());
    }

    #[test]
    fn governed_turn_digest_matches_platform_consumer_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tui-rs/tests/fixtures/governed-turn-digest.json"
        ))
        .unwrap();
        let context = &fixture["turn_context"];
        let mut grant = test_grant();
        grant.organization_id = context["organization_id"].as_str().unwrap().into();
        grant.workspace_id = context["workspace_id"].as_str().unwrap().into();
        grant.thread_id = context["thread_id"].as_str().unwrap().into();
        grant.turn_id = context["turn_id"].as_str().unwrap().into();
        grant.run_id = context["run_id"].as_str().unwrap().into();
        grant.identity_authorization =
            Some(serde_json::from_value(context["identity_authorization"].clone()).unwrap());
        grant.external_tools = vec![ExternalToolDefinition {
            tool_id: "tool-1".into(),
            name: "read_file".into(),
            description: "Read an admitted file".into(),
            input_schema: json!({"type": "object"}),
            execution_owner: ClientToolExecutionOwner {
                client_instance_id: "client-1".into(),
                lease_epoch: 7,
            },
            connection_binding_id: None,
            metadata: None,
        }];
        sign_test_grant(&mut grant);
        let (_, _, bindings) = governed_agent_inputs(&grant).unwrap();
        assert_eq!(
            bindings.values().next().unwrap().turn_digest,
            fixture["turn_digest"].as_str().unwrap()
        );
    }

    #[test]
    fn arbitrary_client_tool_names_are_qualified_only_at_the_provider_boundary() {
        let mut grant = test_grant();
        grant.external_tools = vec![ExternalToolDefinition {
            tool_id: "tool-1".to_string(),
            name: "bash".to_string(),
            description: "caller-owned bash-shaped tool".to_string(),
            input_schema: json!({"type": "object"}),
            execution_owner: ClientToolExecutionOwner {
                client_instance_id: "client-1".to_string(),
                lease_epoch: 4,
            },
            connection_binding_id: None,
            metadata: None,
        }];
        sign_test_grant(&mut grant);

        let (allowed, external, bindings) = governed_agent_inputs(&grant).unwrap();
        assert_eq!(allowed, HashSet::from(["bash".to_string()]));
        assert_eq!(external.len(), 1);
        let provider_name = external[0].tool.name.clone();
        assert!(provider_name.starts_with("client_"));
        assert_ne!(provider_name, "bash");
        let binding = bindings
            .get(&provider_name)
            .expect("owner-qualified binding");
        assert_eq!(binding.logical_name, "bash");
        assert_eq!(binding.owner.client_instance_id, "client-1");
    }

    #[test]
    fn governed_client_tool_result_requires_the_exact_owner_and_execution_binding() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        let binding = ClientToolBinding {
            provider_tool_name: "client_provider_id".to_string(),
            tool_id: "tool-1".to_string(),
            connection_binding_id: None,
            logical_name: "deploy".to_string(),
            owner: ClientToolExecutionOwner {
                client_instance_id: "client-1".to_string(),
                lease_epoch: 9,
            },
            grant_id: "grant-1".to_string(),
            grant_version: 2,
            grant_hash: "sha256:grant".to_string(),
            turn_digest: "sha256:turn".to_string(),
            definition_digest: "sha256:def".to_string(),
            expires_at_ms: i64::MAX,
        };
        {
            let mut runtime = meta.lock().unwrap();
            runtime.pending_tool_calls.insert("call-1".to_string());
            runtime.pending_client_tools.insert(
                "call-1".to_string(),
                PendingClientTool {
                    binding,
                    tool_execution_id: "execution-1".to_string(),
                    args_digest: "sha256:args".to_string(),
                    idempotency_key: "client-tool:execution-1".to_string(),
                    result_digest: None,
                },
            );
        }
        let exact = ClientToolResultBinding {
            tool_execution_id: Some("execution-1".to_string()),
            client_instance_id: Some("client-1".to_string()),
            grant_id: Some("grant-1".to_string()),
            grant_version: Some(2),
            grant_hash: Some("sha256:grant".to_string()),
            turn_digest: Some("sha256:turn".to_string()),
            definition_digest: Some("sha256:def".to_string()),
            args_digest: Some("sha256:args".to_string()),
            owner_lease_epoch: Some(9),
            idempotency_key: Some("client-tool:execution-1".to_string()),
        };
        let mut wrong_owner = ClientToolResultBinding {
            client_instance_id: Some("client-2".to_string()),
            ..exact
        };
        assert!(
            prepare_client_tool_result(
                &meta,
                "call-1".to_string(),
                vec![],
                false,
                wrong_owner.clone()
            )
            .is_err()
        );
        wrong_owner.client_instance_id = Some("client-1".to_string());
        prepare_client_tool_result(&meta, "call-1".to_string(), vec![], false, wrong_owner)
            .expect("exact owner/execution binding accepted once");
        assert!(
            prepare_client_tool_result(
                &meta,
                "call-1".to_string(),
                vec![],
                false,
                ClientToolResultBinding::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn governed_client_tool_result_cannot_target_a_legacy_pending_call() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("legacy-call".to_string());

        let error = prepare_client_tool_result(
            &meta,
            "legacy-call".to_string(),
            vec![],
            false,
            ClientToolResultBinding {
                tool_execution_id: Some("governed-execution".to_string()),
                ..ClientToolResultBinding::default()
            },
        )
        .expect_err("governed result fields require a governed request binding");

        assert!(error.contains("no governed request binding"));
        assert!(
            meta.lock()
                .expect("runtime metadata")
                .pending_tool_calls
                .contains("legacy-call")
        );
    }

    #[test]
    fn expired_governed_client_tool_is_terminalized_without_becoming_pending_again() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        let pending = PendingClientTool {
            binding: ClientToolBinding {
                provider_tool_name: "client_provider_id".to_string(),
                tool_id: "tool-1".to_string(),
                connection_binding_id: None,
                logical_name: "deploy".to_string(),
                owner: ClientToolExecutionOwner {
                    client_instance_id: "client-1".to_string(),
                    lease_epoch: 9,
                },
                grant_id: "grant-1".to_string(),
                grant_version: 2,
                grant_hash: "sha256:grant".to_string(),
                turn_digest: "sha256:turn".to_string(),
                definition_digest: "sha256:def".to_string(),
                expires_at_ms: 0,
            },
            tool_execution_id: "expired-execution".to_string(),
            args_digest: "sha256:args".to_string(),
            idempotency_key: "client-tool:expired-execution".to_string(),
            result_digest: None,
        };
        {
            let mut runtime = meta.lock().expect("runtime metadata");
            runtime
                .pending_tool_calls
                .insert("expired-call".to_string());
            runtime
                .pending_client_tools
                .insert("expired-call".to_string(), pending);
        }
        let supplied = ClientToolResultBinding {
            tool_execution_id: Some("expired-execution".to_string()),
            client_instance_id: Some("client-1".to_string()),
            grant_id: Some("grant-1".to_string()),
            grant_version: Some(2),
            grant_hash: Some("sha256:grant".to_string()),
            turn_digest: Some("sha256:turn".to_string()),
            definition_digest: Some("sha256:def".to_string()),
            args_digest: Some("sha256:args".to_string()),
            owner_lease_epoch: Some(9),
            idempotency_key: Some("client-tool:expired-execution".to_string()),
        };

        let error =
            prepare_client_tool_result(&meta, "expired-call".to_string(), vec![], false, supplied)
                .expect_err("an expired execution must fail closed");
        let (message, resolution) = error.into_parts();
        assert!(message.contains("execution lease expired"));
        let resolution = resolution.expect("expiry includes terminal failure resolution");
        assert!(resolution.messages.iter().any(|message| matches!(
            message,
            FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                success: false,
                ..
            }) if call_id == "expired-call" && tool_execution_id == "expired-execution"
        )));
        assert!(!resolution.rollback.restore_pending);
        rollback_accepted_tool_response(&meta, resolution.rollback.clone());
        rollback_dropped_tool_response(&meta, resolution.rollback);

        let runtime = meta.lock().expect("runtime metadata");
        assert!(!runtime.pending_tool_calls.contains("expired-call"));
        assert!(!runtime.pending_client_tools.contains_key("expired-call"));
    }

    #[tokio::test]
    async fn governed_client_tool_retry_cannot_change_result_or_emit_a_second_terminal() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        {
            let mut runtime = meta.lock().unwrap();
            runtime.pending_tool_calls.insert("call-retry".to_string());
            runtime.pending_client_tools.insert(
                "call-retry".to_string(),
                PendingClientTool {
                    binding: ClientToolBinding {
                        provider_tool_name: "client_provider_id".to_string(),
                        tool_id: "tool-1".to_string(),
                        connection_binding_id: None,
                        logical_name: "deploy".to_string(),
                        owner: ClientToolExecutionOwner {
                            client_instance_id: "client-1".to_string(),
                            lease_epoch: 9,
                        },
                        grant_id: "grant-1".to_string(),
                        grant_version: 2,
                        grant_hash: "sha256:grant".to_string(),
                        turn_digest: "sha256:turn".to_string(),
                        definition_digest: "sha256:def".to_string(),
                        expires_at_ms: i64::MAX,
                    },
                    tool_execution_id: "execution-retry".to_string(),
                    args_digest: "sha256:args".to_string(),
                    idempotency_key: "client-tool:execution-retry".to_string(),
                    result_digest: None,
                },
            );
        }
        let supplied = ClientToolResultBinding {
            tool_execution_id: Some("execution-retry".to_string()),
            client_instance_id: Some("client-1".to_string()),
            grant_id: Some("grant-1".to_string()),
            grant_version: Some(2),
            grant_hash: Some("sha256:grant".to_string()),
            turn_digest: Some("sha256:turn".to_string()),
            definition_digest: Some("sha256:def".to_string()),
            args_digest: Some("sha256:args".to_string()),
            owner_lease_epoch: Some(9),
            idempotency_key: Some("client-tool:execution-retry".to_string()),
        };
        let content = vec![ClientToolResultContent::Text {
            text: "stable result".to_string(),
        }];
        let accepted = prepare_client_tool_result(
            &meta,
            "call-retry".to_string(),
            content.clone(),
            false,
            supplied.clone(),
        )
        .unwrap();
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        drop(closed_rx);
        let mut first_terminal_count = 0;
        dispatch_accepted_tool_response_with(
            &meta,
            &closed_tx,
            accepted,
            "call-retry".to_string(),
            |message| {
                if matches!(
                    message,
                    FromAgentMessage::ToolEnd(tool_wire::ToolEnd { .. })
                ) {
                    first_terminal_count += 1;
                }
                Ok(())
            },
            |_, _, _| {},
        )
        .expect_err("closed native channel restores the exact result for retry");
        assert_eq!(first_terminal_count, 1);

        assert!(
            prepare_client_tool_result(
                &meta,
                "call-retry".to_string(),
                vec![ClientToolResultContent::Text {
                    text: "changed result".to_string(),
                }],
                false,
                supplied.clone(),
            )
            .unwrap_err()
            .contains("changed across retry")
        );

        let accepted =
            prepare_client_tool_result(&meta, "call-retry".to_string(), content, false, supplied)
                .unwrap();
        let (retry_tx, mut retry_rx) = mpsc::unbounded_channel();
        let mut retry_terminal_count = 0;
        dispatch_accepted_tool_response_with(
            &meta,
            &retry_tx,
            accepted,
            "call-retry".to_string(),
            |message| {
                if matches!(
                    message,
                    FromAgentMessage::ToolEnd(tool_wire::ToolEnd { .. })
                ) {
                    retry_terminal_count += 1;
                }
                Ok(())
            },
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(retry_terminal_count, 0);
        assert!(retry_rx.try_recv().is_ok());
    }

    #[test]
    fn headless_model_resolution_uses_canonical_precedence_without_rewriting_managed_ids() {
        let env = HashMap::from([
            ("MAESTRO_MODEL".to_string(), " evalops/gpt-5.6 ".to_string()),
            (
                "MAESTRO_DEFAULT_MODEL".to_string(),
                "evalops/gpt-5.5".to_string(),
            ),
        ]);

        assert_eq!(
            resolve_headless_model(Some(" maestro-managed/gpt-5.7 ".to_string()), &env),
            "maestro-managed/gpt-5.7"
        );
        assert_eq!(resolve_headless_model(None, &env), "evalops/gpt-5.6");
        assert_eq!(
            resolve_headless_model(
                None,
                &HashMap::from([(
                    "MAESTRO_DEFAULT_MODEL".to_string(),
                    " evalops/gpt-5.5 ".to_string(),
                )]),
            ),
            "evalops/gpt-5.5"
        );
        assert_eq!(
            resolve_headless_model(None, &HashMap::new()),
            "evalops/accounts/fireworks/models/glm-5p3"
        );
    }

    #[tokio::test]
    async fn headless_publishes_selected_local_limits_before_agent_creation() {
        let discovered = crate::model_catalog::ModelInfo {
            id: "headless-live-limit-test".to_owned(),
            name: "headless-live-limit-test".to_owned(),
            provider: "llamacpp".to_owned(),
            description: "test local model".to_owned(),
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
                source: "test".to_owned(),
                detail: None,
            },
        };
        let mut published = None;

        prepare_headless_local_model_with(
            "llamacpp/headless-live-limit-test",
            async { Ok(Some(discovered)) },
            |route, models| {
                published = Some((route.to_owned(), models.to_vec()));
            },
        )
        .await
        .unwrap();

        let (route, models) = published.expect("headless discovery must publish live limits");
        assert_eq!(route, "llamacpp/headless-live-limit-test");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].capabilities.context_tokens, 8_192);
    }

    #[test]
    fn managed_headless_identity_reports_the_platform_route() {
        assert_eq!(
            reported_identity("evalops/openai/gpt-5.6", "OpenAI", Some("openrouter"),),
            (
                "evalops/openai/gpt-5.6".to_string(),
                "openrouter".to_string()
            )
        );
        assert_eq!(
            reported_identity(
                "MAESTRO-MANAGED/openai/gpt-5.6",
                "OpenAI",
                Some("openrouter"),
            ),
            (
                "MAESTRO-MANAGED/openai/gpt-5.6".to_string(),
                "openrouter".to_string()
            )
        );

        let usage = to_headless_usage(
            crate::agent::TokenUsage::default(),
            "evalops/openai/gpt-5.6",
            Some("openrouter"),
        );
        assert_eq!(usage.model_id.as_deref(), Some("evalops/openai/gpt-5.6"));
        assert_eq!(usage.provider.as_deref(), Some("openrouter"));
    }

    #[test]
    fn unbound_headless_identity_preserves_native_provider_label() {
        assert_eq!(
            reported_identity("gpt-5.5", "OpenAI", Some("openrouter")),
            ("gpt-5.5".to_string(), "OpenAI".to_string())
        );
    }

    #[test]
    fn openrouter_headless_identity_uses_route_not_nested_vendor() {
        assert_eq!(
            reported_identity("openrouter/anthropic/claude-sonnet-4.5", "OpenAI", None,),
            (
                "openrouter/anthropic/claude-sonnet-4.5".to_string(),
                "OpenRouter".to_string(),
            )
        );
        assert_eq!(
            reported_identity("openrouter/google/gemini-2.5-pro", "OpenAI", None,).1,
            "OpenRouter"
        );
        assert_eq!(
            reported_identity("openrouter/meta-llama/llama-4-maverick", "OpenAI", None,).1,
            "OpenRouter"
        );
    }

    #[test]
    fn observed_ready_identity_preserves_native_values_before_normalizing_route() {
        assert_eq!(
            observed_ready_identity("evalops/openai/gpt-5.6", "OpenAI", Some("openrouter"),),
            (
                "evalops/openai/gpt-5.6".to_string(),
                "OpenAI".to_string(),
                "evalops/openai/gpt-5.6".to_string(),
                "openrouter".to_string(),
            )
        );
    }

    #[tokio::test]
    async fn pod_shaped_managed_default_validates_before_qualified_ready() {
        if std::env::var_os("MAESTRO_HEADLESS_MANAGED_READY_FIXTURE").is_some() {
            assert_eq!(
                run_headless_server(None).await.expect("headless fixture"),
                0
            );
            return;
        }

        let root = tempfile::tempdir().expect("fixture root");
        let token = root.path().join("gateway-token");
        std::fs::write(&token, "tenant-token\n").expect("gateway token");
        let current = std::env::current_exe().expect("current test binary");
        let mut child = std::process::Command::new(current)
            .arg("headless_server::tests::pod_shaped_managed_default_validates_before_qualified_ready")
            .arg("--exact")
            .arg("--nocapture")
            .arg("--format")
            .arg("terse")
            .env("MAESTRO_HEADLESS_MANAGED_READY_FIXTURE", "1")
            .env(
                "MAESTRO_IDENTITY_URL",
                crate::credential_mode::test_identity_base_url(),
            )
            .env_remove("MAESTRO_MODEL")
            .env_remove("OPENAI_API_KEY")
            .env("MAESTRO_DEFAULT_MODEL", "evalops/gpt-5.6")
            // Parallel tests can install a direct token in the parent process.
            // This fixture must exercise its explicit token-file credential.
            .env_remove(crate::credential_mode::ACCESS_TOKEN_ENV)
            .env("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE", &token)
            .env("MAESTRO_EVALOPS_BASE_URL", "https://gateway.example/v1")
            .env("MAESTRO_EVALOPS_ORG_ID", "org_1")
            .env("MAESTRO_EVALOPS_WORKSPACE_ID", "ws_1")
            .env("MAESTRO_EVALOPS_PROVIDER", "openrouter")
            .env("MAESTRO_EVALOPS_ENVIRONMENT", "production")
            .env("MAESTRO_EVALOPS_CREDENTIAL_NAME", "platform-default")
            .env("MAESTRO_EVALOPS_TEAM_ID", "team_1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn managed headless fixture");
        writeln!(
            child.stdin.take().expect("fixture stdin"),
            "{}",
            json!({"type":"shutdown"})
        )
        .expect("write shutdown");
        let output = child.wait_with_output().expect("headless fixture output");

        assert!(
            output.status.success(),
            "fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ready = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .expect("first protocol message");
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["model"], "evalops/gpt-5.6");
        assert_eq!(ready["provider"], "openrouter");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("OPENAI_API_KEY"));
    }

    fn spawn_headless_identity_fixture(
        test_name: &str,
        fixture_env: &str,
        extra_env: &[(&str, &std::ffi::OsStr)],
    ) -> std::process::Output {
        let home = tempfile::tempdir().expect("fixture Maestro home");
        let current = std::env::current_exe().expect("current test binary");
        let mut command = std::process::Command::new(current);
        command
            .arg(test_name)
            .arg("--exact")
            .arg("--nocapture")
            .arg("--format")
            .arg("terse")
            .env(fixture_env, "1")
            .env("MAESTRO_HOME", home.path())
            .env("MAESTRO_OAUTH_STORAGE_MODE", "file")
            .env("MAESTRO_DISABLE_KEYCHAIN", "1")
            .env_remove("MAESTRO_MODEL")
            .env_remove("MAESTRO_DEFAULT_MODEL")
            .env_remove("MAESTRO_IDENTITY_URL")
            .env_remove(crate::credential_mode::ACCESS_TOKEN_ENV)
            .env_remove(crate::credential_mode::ACCESS_TOKEN_FILE_ENV)
            .env_remove(crate::credential_mode::ORG_ID_ENV)
            .env_remove(crate::credential_mode::WORKSPACE_ID_ENV)
            .env_remove("MAESTRO_HOSTED_RUNNER_MODE")
            .env_remove(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("run headless identity fixture")
    }

    #[tokio::test]
    async fn direct_openai_key_reaches_headless_ready_without_identity() {
        if std::env::var_os("MAESTRO_HEADLESS_DIRECT_OPENAI_READY_FIXTURE").is_some() {
            assert_eq!(
                run_headless_server(Some("openai/gpt-5.4".to_string()))
                    .await
                    .expect("direct OpenAI headless fixture"),
                0
            );
            return;
        }

        let home = tempfile::tempdir().expect("fixture Maestro home");
        let current = std::env::current_exe().expect("current test binary");
        let mut child = std::process::Command::new(current)
            .arg(
                "headless_server::tests::direct_openai_key_reaches_headless_ready_without_identity",
            )
            .arg("--exact")
            .arg("--nocapture")
            .arg("--format")
            .arg("terse")
            .env("MAESTRO_HEADLESS_DIRECT_OPENAI_READY_FIXTURE", "1")
            .env("MAESTRO_HOME", home.path())
            .env("MAESTRO_OAUTH_STORAGE_MODE", "file")
            .env("MAESTRO_DISABLE_KEYCHAIN", "1")
            .env("OPENAI_API_KEY", "synthetic-maestro-smoke-key")
            .env("OPENAI_BASE_URL", "http://127.0.0.1:9/v1")
            .env_remove("MAESTRO_MODEL")
            .env_remove("MAESTRO_DEFAULT_MODEL")
            .env_remove("MAESTRO_IDENTITY_URL")
            .env_remove(crate::credential_mode::ACCESS_TOKEN_ENV)
            .env_remove(crate::credential_mode::ACCESS_TOKEN_FILE_ENV)
            .env_remove(crate::credential_mode::ORG_ID_ENV)
            .env_remove(crate::credential_mode::WORKSPACE_ID_ENV)
            .env_remove("MAESTRO_HOSTED_RUNNER_MODE")
            .env_remove(crate::init_cli::TEST_IDENTITY_AUTHORITY_ENV)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn direct OpenAI headless fixture");
        writeln!(
            child.stdin.take().expect("fixture stdin"),
            "{}",
            json!({"type":"shutdown"})
        )
        .expect("write shutdown");
        let output = child.wait_with_output().expect("headless fixture output");
        assert!(
            output.status.success(),
            "fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ready = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .expect("first protocol message");
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["model"], "openai/gpt-5.4");
        assert_eq!(ready["provider"], "OpenAI");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains(crate::credential_mode::IDENTITY_REQUIRED_MESSAGE),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn managed_evalops_headless_route_requires_identity() {
        if std::env::var_os("MAESTRO_HEADLESS_MANAGED_IDENTITY_REQUIRED_FIXTURE").is_some() {
            let error = run_headless_server(Some("evalops/gpt-5.5".to_string()))
                .await
                .expect_err("managed evalops headless must not become ready without Identity");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains(crate::credential_mode::IDENTITY_REQUIRED_MESSAGE),
                "{rendered}"
            );
            eprintln!("{rendered}");
            return;
        }

        let output = spawn_headless_identity_fixture(
            "headless_server::tests::managed_evalops_headless_route_requires_identity",
            "MAESTRO_HEADLESS_MANAGED_IDENTITY_REQUIRED_FIXTURE",
            &[
                (
                    "OPENAI_API_KEY",
                    std::ffi::OsStr::new("synthetic-maestro-smoke-key"),
                ),
                (
                    "OPENAI_BASE_URL",
                    std::ffi::OsStr::new("http://127.0.0.1:9/v1"),
                ),
            ],
        );
        assert!(
            output.status.success(),
            "fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(crate::credential_mode::IDENTITY_REQUIRED_MESSAGE),
            "stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    struct HeadlessFixtureChild {
        child: std::process::Child,
        #[cfg(unix)]
        group: crate::tools::process_utils::ProcessGroupGuard,
    }

    impl Drop for HeadlessFixtureChild {
        fn drop(&mut self) {
            #[cfg(unix)]
            self.group.terminate();
            #[cfg(not(unix))]
            crate::tools::process_utils::kill_process_tree(self.child.id());
            if let Err(error) = self.child.wait() {
                eprintln!("failed to reap headless fixture: {error}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn headless_fixture_drop_reaps_its_child() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        crate::tools::process_utils::set_std_process_group(&mut command);
        let child = command.spawn().expect("spawn fixture");
        let pid = i32::try_from(child.id()).expect("fixture pid");
        let fixture = HeadlessFixtureChild {
            group: crate::tools::process_utils::ProcessGroupGuard::new(Some(child.id())),
            child,
        };
        drop(fixture);
        // SAFETY: this PID identifies only the child owned above. WNOHANG
        // checks that Drop already reaped it without blocking the test.
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(result, -1, "fixture child must already be reaped");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn steer_message_reaches_codex_as_turn_steer_not_turn_start() {
        if std::env::var_os("MAESTRO_HEADLESS_STEER_FIXTURE").is_some() {
            assert_eq!(
                run_headless_server(None).await.expect("headless fixture"),
                0
            );
            return;
        }

        let root = tempfile::tempdir().expect("fixture root");
        let script = root.path().join("app-server.js");
        let log = root.path().join("app-server.log");
        let output = root.path().join("headless.log");
        // This fixture uses only loopback HTTP. Do not enumerate the developer's
        // macOS certificate Keychain while building its Identity client.
        let roots = root.path().join("roots.pem");
        std::fs::write(&roots, "").expect("isolated certificate roots");
        let home = root.path().join("maestro-home");
        std::fs::write(
            &script,
            r"const rl=require('readline').createInterface({input:process.stdin});
const fs=require('fs');
const log=process.env.MAESTRO_HEADLESS_STEER_LOG;
function send(x){process.stdout.write(JSON.stringify(x)+'\n')}
rl.on('line',line=>{const x=JSON.parse(line);fs.appendFileSync(log,JSON.stringify(x)+'\n');
if(x.method==='initialize'){send({id:x.id,result:{protocolVersion:'2025-01-01',capabilities:{}}})}
else if(x.method==='model/list'){send({id:x.id,result:{data:[{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'medium'},{reasoningEffort:'high'},{reasoningEffort:'xhigh'}]}],nextCursor:null}})}
else if(x.method==='thread/start'){send({id:x.id,result:{thread:{id:'thread-headless'}}})}
else if(x.method==='thread/resume'){send({id:x.id,result:{thread:{id:x.params.threadId}}})}
else if(x.method==='turn/start'){send({id:x.id,result:{turn:{id:'turn-active'}}})}
else if(x.method==='turn/steer'){send({id:x.id,result:{turn:{id:'turn-active'}}});setTimeout(()=>send({method:'turn/completed',params:{turnId:'turn-active'}}),10)}
});",
        )
        .expect("app-server script");
        let current = std::env::current_exe().expect("current test binary");
        let mut command = std::process::Command::new(current);
        command
            .arg("headless_server::tests::steer_message_reaches_codex_as_turn_steer_not_turn_start")
            .arg("--exact")
            .arg("--nocapture")
            .current_dir(root.path())
            .env("SSL_CERT_FILE", &roots)
            .env_remove("SSL_CERT_DIR")
            .env("MAESTRO_DISABLE_KEYCHAIN", "1")
            .env("MAESTRO_HEADLESS_STEER_FIXTURE", "1")
            .env("MAESTRO_HEADLESS_STEER_LOG", &log)
            .env(
                "MAESTRO_IDENTITY_URL",
                crate::credential_mode::test_identity_base_url(),
            )
            .env("MAESTRO_HOME", &home)
            .env(
                crate::credential_mode::ACCESS_TOKEN_ENV,
                "fixture-evalops-access-token",
            )
            .env(crate::credential_mode::ORG_ID_ENV, "fixture-evalops-org")
            .env("MAESTRO_MODEL", "openai-codex/gpt-5.5")
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env(
                "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                serde_json::to_string(&vec![script.display().to_string()])
                    .expect("app-server args"),
            )
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .stdin(std::process::Stdio::piped())
            .stdout(std::fs::File::create(&output).expect("headless log"));
        #[cfg(unix)]
        crate::tools::process_utils::set_std_process_group(&mut command);
        let child = command.spawn().expect("spawn headless fixture");
        let mut fixture = HeadlessFixtureChild {
            #[cfg(unix)]
            group: crate::tools::process_utils::ProcessGroupGuard::new(Some(child.id())),
            child,
        };
        let mut stdin = fixture.child.stdin.take().expect("fixture stdin");
        use std::io::Write as _;
        writeln!(
            stdin,
            "{}",
            json!({"type":"prompt","content":"start work","attachments":null})
        )
        .expect("write prompt");

        let turn_started = std::time::Instant::now();
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains(r#""method":"turn/start""#)
        {
            assert!(
                turn_started.elapsed() < std::time::Duration::from_secs(5),
                "headless prompt never reached turn/start: {}\nheadless output: {}",
                std::fs::read_to_string(&log).unwrap_or_default(),
                std::fs::read_to_string(&output).unwrap_or_default()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        writeln!(
            stdin,
            "{}",
            json!({"type":"steer","content":"inspect the logs too","attachments":null})
        )
        .expect("write steer");
        let steer_started = std::time::Instant::now();
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains(r#""method":"turn/steer""#)
        {
            assert!(
                steer_started.elapsed() < std::time::Duration::from_secs(5),
                "headless steer did not reach turn/steer: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        writeln!(stdin, "{}", json!({"type":"shutdown"})).expect("write shutdown");
        drop(stdin);
        let status = fixture.child.wait().expect("headless fixture status");
        assert!(status.success(), "headless fixture failed: {status}");

        let requests = std::fs::read_to_string(log).expect("app-server log");
        assert_eq!(requests.matches(r#""method":"turn/start""#).count(), 1);
        assert_eq!(requests.matches(r#""method":"turn/steer""#).count(), 1);
    }

    #[tokio::test]
    async fn staged_provider_rotation_failure_preserves_generation_for_retry() {
        if std::env::var_os("MAESTRO_STAGED_ROTATION_RETRY_FIXTURE").is_some() {
            let home = std::path::PathBuf::from(
                std::env::var("MAESTRO_HOME").expect("fixture Maestro home"),
            );
            let workspace = std::path::PathBuf::from(
                std::env::var("MAESTRO_STAGED_ROTATION_WORKSPACE").expect("fixture workspace"),
            );
            let log = std::path::PathBuf::from(
                std::env::var("MAESTRO_STAGED_ROTATION_LOG").expect("fixture provider log"),
            );
            let mut state = HeadlessState::new(Some("openai-codex/gpt-5.5".to_string()));
            state.cwd = workspace.to_string_lossy().into_owned();
            state.system_prompt = "base prompt".to_string();
            state
                .workspace_capabilities
                .set_base_prompt(state.system_prompt.clone());
            state.ensure_agent().expect("create initial native agent");
            state
                .agent
                .as_ref()
                .expect("initial native agent")
                .ensure_provider_prompt_installed()
                .await
                .expect("install initial provider prompt");

            let context = crate::headless::controller_binding::ControllerContext {
                schema_version:
                    crate::headless::controller_binding::CONTROLLER_CONTEXT_SCHEMA_VERSION
                        .to_string(),
                controller_id: "evalops.platform".to_string(),
                organization_id: "org-1".to_string(),
                workspace_id: "workspace-1".to_string(),
                thread_id: "thread-1".to_string(),
                channel_id: None,
                request_id: None,
                lifetime_profile:
                    crate::headless::controller_binding::ControllerLifetimeProfile::Resident,
                runtime_generation: Some(7),
            };
            let binding = crate::headless::controller_binding::ControllerBindingReceipt {
                binding_version: crate::headless::controller_binding::CONTROLLER_BINDING_VERSION
                    .to_string(),
                binding_sha256: "sha256:binding".to_string(),
                controller_context: context.clone(),
            };
            let body = "Always apply the retry checklist.";
            let mut request =
                crate::headless::workspace_capabilities::ApplyWorkspaceCapabilitySet {
                    organization_id: "org-1".to_string(),
                    workspace_id: "workspace-1".to_string(),
                    runner_session_id: "runner-1".to_string(),
                    runtime_generation: 7,
                    activation_generation: 2,
                    workspace_snapshot_digest:
                        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string(),
                    workspace_skill_set_digest:
                        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                            .to_string(),
                    capability_set_digest: String::new(),
                    workspace_instructions: vec!["Use generation two.".to_string()],
                    admitted_catalog: vec![
                        crate::headless::workspace_capabilities::WorkspacePromptCapability {
                            qualified_id: "skill.retry".to_string(),
                            name: "retry".to_string(),
                            scope: "workspace".to_string(),
                            revision_digest:
                                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                                    .to_string(),
                            body_digest: format!(
                                "sha256:{:x}",
                                sha2::Sha256::digest(body.as_bytes())
                            ),
                            trigger_patterns: vec!["retry".to_string()],
                            user_invocable: true,
                            pinned_prompt_only: true,
                            title: "Retry".to_string(),
                            description: "Prove staged retry.".to_string(),
                            instructions: vec!["Apply the retry checklist.".to_string()],
                            body: body.to_string(),
                            entry_digest: String::new(),
                        },
                    ],
                    admission_receipt_id: "admission-2".to_string(),
                };
            crate::headless::workspace_capabilities::recompute_request_digests(&mut request)
                .expect("staged request digests");
            let receipt = state
                .workspace_capabilities
                .apply(request, &binding, &context, "runner-1", true)
                .expect("stage capability generation");
            assert!(receipt.staged_for_next_turn);

            let quarantine_blocker = home.join("codex/thread-bindings/quarantine");
            std::fs::write(&quarantine_blocker, b"block retirement")
                .expect("create quarantine blocker");
            let first = submit_prompt_with_kind(
                &mut state,
                "first retry".to_string(),
                None,
                PromptKind::Prompt,
                None,
            )
            .await;
            assert!(first.is_err(), "blocked retirement must fail");
            assert!(
                state.workspace_capabilities.has_staged_set(),
                "failed retirement must preserve the staged generation"
            );

            std::fs::remove_file(&quarantine_blocker).expect("remove quarantine blocker");
            submit_prompt_with_kind(
                &mut state,
                "second retry".to_string(),
                None,
                PromptKind::Prompt,
                None,
            )
            .await
            .expect("same staged generation retries after retirement recovers");
            assert!(
                !state.workspace_capabilities.has_staged_set(),
                "successful provider installation promotes the staged generation"
            );
            assert!(
                std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .matches(r#""method":"thread/start""#)
                    .count()
                    >= 2,
                "retry must install a replacement provider thread"
            );
            if let Some(agent) = state.agent.take() {
                agent.shutdown().await;
            }
            if let Some(task) = state.event_task.take() {
                let _ = task.await;
            }
            return;
        }

        let root = tempfile::tempdir().expect("fixture root");
        let script = root.path().join("app-server.js");
        let log = root.path().join("app-server.log");
        let home = root.path().join("maestro-home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("fixture workspace");
        std::fs::write(
            &script,
            r#"const rl=require("readline").createInterface({input:process.stdin});
const fs=require("fs"); const log=process.env.MAESTRO_STAGED_ROTATION_LOG;
function send(x){process.stdout.write(JSON.stringify(x)+"\n")}
rl.on("line",line=>{const x=JSON.parse(line);fs.appendFileSync(log,JSON.stringify(x)+"\n");
if(x.method==="initialize"){send({id:x.id,result:{protocolVersion:"2025-01-01",capabilities:{}}})}
else if(x.method==='model/list'){send({id:x.id,result:{data:[{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'medium'},{reasoningEffort:'high'},{reasoningEffort:'xhigh'}]}],nextCursor:null}})}
else if(x.method==="thread/start"){const n=(fs.readFileSync(log,"utf8").match(/"method":"thread\/start"/g)||[]).length;send({id:x.id,result:{thread:{id:"provider-thread-"+n}}})}
else if(x.method==="thread/inject_items"){send({id:x.id,result:{}})}
else if(x.method==="turn/start"){const turnId="turn-"+x.id;send({id:x.id,result:{turn:{id:turnId}}});setTimeout(()=>send({method:"turn/completed",params:{turnId}}),5)}
});"#,
        )
        .expect("app-server script");
        let current = std::env::current_exe().expect("current test binary");
        let output = std::process::Command::new(current)
            .arg(
                "headless_server::tests::staged_provider_rotation_failure_preserves_generation_for_retry",
            )
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_STAGED_ROTATION_RETRY_FIXTURE", "1")
            .env("MAESTRO_STAGED_ROTATION_LOG", &log)
            .env("MAESTRO_STAGED_ROTATION_WORKSPACE", &workspace)
            .env(
                "MAESTRO_IDENTITY_URL",
                crate::credential_mode::test_identity_base_url(),
            )
            .env("MAESTRO_HOME", &home)
            .env(
                crate::credential_mode::ACCESS_TOKEN_ENV,
                "fixture-evalops-access-token",
            )
            .env(crate::credential_mode::ORG_ID_ENV, "fixture-evalops-org")
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env(
                "MAESTRO_CODEX_APP_SERVER_ARGS_JSON",
                serde_json::to_string(&vec![script.display().to_string()])
                    .expect("app-server args"),
            )
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .output()
            .expect("run staged rotation retry fixture");
        assert!(
            output.status.success(),
            "fixture stdout: {}; fixture stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(&log).expect("provider log");
        assert_eq!(calls.matches(r#""method":"thread/start""#).count(), 2);
    }

    #[tokio::test]
    async fn resident_workspace_admission_receipt_joins_the_provider_observed_prompt() {
        if std::env::var_os("MAESTRO_HEADLESS_WORKSPACE_PROMPT_FIXTURE").is_some() {
            assert_eq!(
                run_headless_server(None)
                    .await
                    .expect("headless workspace prompt fixture"),
                0
            );
            return;
        }

        let root = tempfile::tempdir().expect("fixture root");
        let script = root.path().join("app-server.js");
        let log = root.path().join("app-server.log");
        let home = root.path().join("maestro-home");
        let provider_ack_barrier = root.path().join("provider-thread-start.ack");
        std::fs::write(
            &script,
            r#"const rl=require("readline").createInterface({input:process.stdin});
const fs=require("fs"); const log=process.env.MAESTRO_HEADLESS_WORKSPACE_PROMPT_LOG;
const barrier=process.env.MAESTRO_HEADLESS_WORKSPACE_PROMPT_BARRIER;
function send(x){process.stdout.write(JSON.stringify(x)+"\n")}
rl.on("line",line=>{const x=JSON.parse(line);fs.appendFileSync(log,JSON.stringify(x)+"\n");
if(x.method==="initialize"){send({id:x.id,result:{protocolVersion:"2025-01-01",capabilities:{}}})}
else if(x.method==='model/list'){send({id:x.id,result:{data:[{id:'gpt-5.5',model:'gpt-5.5',defaultReasoningEffort:'medium',supportedReasoningEfforts:[{reasoningEffort:'low'},{reasoningEffort:'medium'},{reasoningEffort:'high'},{reasoningEffort:'xhigh'}]}],nextCursor:null}})}
else if(x.method==="thread/start"){const threadCount=(fs.readFileSync(log,"utf8").match(/"method":"thread\/start"/g)||[]).length;const ack=()=>send({id:x.id,result:{thread:{id:"provider-thread-"+threadCount}}});if(threadCount===1&&barrier&&!fs.existsSync(barrier)){const timer=setInterval(()=>{if(fs.existsSync(barrier)){clearInterval(timer);ack()}},5)}else{ack()}}
else if(x.method==="thread/inject_items"){send({id:x.id,result:{}})}
else if(x.method==="turn/start"){const turnId="turn-"+x.id;send({id:x.id,result:{turn:{id:turnId}}});setTimeout(()=>{send({method:"item/agentMessage/delta",params:{turnId,delta:"fixture answer"}});send({method:"turn/completed",params:{turnId}})},10)}
});"#,
        )
        .expect("app-server script");

        let mut request = crate::headless::workspace_capabilities::ApplyWorkspaceCapabilitySet {
            organization_id: "org-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            runner_session_id: "runner-1".to_string(),
            runtime_generation: 7,
            activation_generation: 1,
            workspace_snapshot_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            workspace_skill_set_digest:
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
            capability_set_digest: String::new(),
            workspace_instructions: vec!["Use workspace review guidance.".to_string()],
            admitted_catalog: vec![
                crate::headless::workspace_capabilities::WorkspacePromptCapability {
                    qualified_id: "skill.review".to_string(),
                    name: "review".to_string(),
                    scope: "workspace".to_string(),
                    revision_digest:
                        "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                            .to_string(),
                    body_digest: format!(
                        "sha256:{:x}",
                        sha2::Sha256::digest("Always report the review result.".as_bytes())
                    ),
                    trigger_patterns: vec!["review".to_string()],
                    user_invocable: true,
                    pinned_prompt_only: true,
                    title: "Review".to_string(),
                    description: "Review workspace changes.".to_string(),
                    instructions: vec!["Apply the review checklist.".to_string()],
                    body: "Always report the review result.".to_string(),
                    entry_digest: String::new(),
                },
            ],
            admission_receipt_id: "admission-1".to_string(),
        };
        crate::headless::workspace_capabilities::recompute_request_digests(&mut request)
            .expect("fixture request digests");

        let current = std::env::current_exe().expect("current test binary");
        let stdout_path = root.path().join("headless.stdout");
        let stderr_path = root.path().join("headless.stderr");
        let mut child = std::process::Command::new(current)
            .arg("headless_server::tests::resident_workspace_admission_receipt_joins_the_provider_observed_prompt")
            .arg("--exact")
            .arg("--nocapture")
            .env("MAESTRO_HEADLESS_WORKSPACE_PROMPT_FIXTURE", "1")
            .env("MAESTRO_HEADLESS_WORKSPACE_PROMPT_LOG", &log)
            .env(
                "MAESTRO_IDENTITY_URL",
                crate::credential_mode::test_identity_base_url(),
            )
            .env("MAESTRO_HOME", &home)
            .env(
                "MAESTRO_HEADLESS_WORKSPACE_PROMPT_BARRIER",
                &provider_ack_barrier,
            )
            .env("MAESTRO_MODEL", "openai-codex/gpt-5.5")
            .env("MAESTRO_CODEX_APP_SERVER_COMMAND", "node")
            .env("MAESTRO_CODEX_APP_SERVER_ARGS_JSON", serde_json::to_string(&vec![script.display().to_string()]).expect("app server args"))
            .env("OPENAI_CODEX_TOKEN", "fixture-token")
            .env("MAESTRO_RUNNER_SESSION_ID", "runner-1")
            .env("MAESTRO_SESSION_ID", "maestro-session-1")
            .env(
                crate::credential_mode::ACCESS_TOKEN_ENV,
                "fixture-evalops-access-token",
            )
            .env("MAESTRO_EVALOPS_ORG_ID", "org-1")
            .env("MAESTRO_EVALOPS_WORKSPACE_ID", "workspace-1")
            .env("MAESTRO_EVALOPS_THREAD_ID", "thread-1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::from(std::fs::File::create(&stdout_path).expect("fixture stdout")))
            .stderr(std::process::Stdio::from(std::fs::File::create(&stderr_path).expect("fixture stderr")))
            .spawn()
            .expect("spawn workspace prompt fixture");
        let mut stdin = child.stdin.take().expect("fixture stdin");
        use std::io::Write as _;
        let wait_for_provider_turns = |expected: usize, child: &mut std::process::Child| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let calls = std::fs::read_to_string(&log).unwrap_or_default();
                if calls.matches(r#""method":"turn/start""#).count() >= expected {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "provider did not observe {expected} turn(s): {calls}; headless stdout: {}; headless stderr: {}",
                        std::fs::read_to_string(&stdout_path).unwrap_or_default(),
                        std::fs::read_to_string(&stderr_path).unwrap_or_default(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let wait_for_headless_messages =
            |message_type: &str, expected: usize, child: &mut std::process::Child| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                loop {
                    let output = std::fs::read_to_string(&stdout_path).unwrap_or_default();
                    let observed = output
                        .lines()
                        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                        .filter(|message| message["type"] == message_type)
                        .count();
                    if observed >= expected {
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "headless did not emit {expected} {message_type} message(s): {output}; provider calls: {}; headless stderr: {}",
                            std::fs::read_to_string(&log).unwrap_or_default(),
                            std::fs::read_to_string(&stderr_path).unwrap_or_default(),
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            };
        let hello = json!({
            "type": "hello", "protocol_version": "2026-08-08",
            "controller_binding_version": crate::headless::controller_binding::CONTROLLER_BINDING_VERSION,
            "controller_context": {
                "schema_version": crate::headless::controller_binding::CONTROLLER_CONTEXT_SCHEMA_VERSION,
                "controller_id": "evalops.platform", "organization_id": "org-1",
                "workspace_id": "workspace-1", "thread_id": "thread-1",
                "lifetime_profile": "resident", "runtime_generation": 7
            },
            "capability_manifest": {
                "schema_version": "evalops.maestro.capability-manifest.v1",
                "engine_kind": "maestro", "protocol_version": "2026-08-08",
                "tool_protocol_version": "evalops.maestro.tool-bridge.v1",
                "supported_tools": [], "native_tool_calls": true, "approvals": true,
                "continuation": false, "cancellation": true, "idempotent_replay": true,
                "streaming": true
            }
        });
        writeln!(stdin, "{hello}").expect("write hello");
        writeln!(
            stdin,
            "{}",
            json!({"type":"apply_workspace_capability_set","request":request})
        )
        .expect("write admission");
        let provider_start_deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::fs::read_to_string(&log)
            .unwrap_or_default()
            .matches(r#""method":"thread/start""#)
            .count()
            < 1
        {
            assert!(
                std::time::Instant::now() < provider_start_deadline,
                "provider did not receive thread/start before acknowledgement barrier"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        let early_receipts = std::fs::read_to_string(&stdout_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["type"] == "workspace_capability_set_applied")
            .count();
        assert_eq!(
            early_receipts, 0,
            "capability receipt must wait for provider thread/start acknowledgement"
        );
        std::fs::write(&provider_ack_barrier, b"ack").expect("release provider ack barrier");
        wait_for_headless_messages("workspace_capability_set_applied", 1, &mut child);
        writeln!(
            stdin,
            "{}",
            json!({"type":"prompt","content":"review this","attachments":null})
        )
        .expect("write first prompt");
        wait_for_provider_turns(1, &mut child);
        wait_for_headless_messages("response_end", 1, &mut child);
        request.activation_generation = 2;
        request.admission_receipt_id = "admission-2".to_string();
        request.workspace_instructions = vec!["Use the generation two guidance.".to_string()];
        request.admitted_catalog[0].revision_digest =
            "sha256:3333333333333333333333333333333333333333333333333333333333333333".to_string();
        request.admitted_catalog[0].instructions =
            vec!["Apply the generation two checklist.".to_string()];
        request.admitted_catalog[0].body = "Always report the generation two result.".to_string();
        request.admitted_catalog[0].body_digest = format!(
            "sha256:{:x}",
            sha2::Sha256::digest(request.admitted_catalog[0].body.as_bytes())
        );
        crate::headless::workspace_capabilities::recompute_request_digests(&mut request)
            .expect("generation two request digests");
        writeln!(
            stdin,
            "{}",
            json!({"type":"apply_workspace_capability_set","request":request})
        )
        .expect("write generation two admission");
        writeln!(
            stdin,
            "{}",
            json!({"type":"prompt","content":"continue review","attachments":null})
        )
        .expect("write second prompt");
        wait_for_provider_turns(2, &mut child);
        wait_for_headless_messages("response_end", 2, &mut child);
        writeln!(stdin, "{}", json!({"type":"shutdown"})).expect("write shutdown");
        drop(stdin);
        let status = child.wait().expect("fixture status");
        assert!(
            status.success(),
            "fixture stderr: {}",
            std::fs::read_to_string(&stderr_path).unwrap_or_default(),
        );

        let messages = std::fs::read_to_string(&stdout_path)
            .expect("headless stdout")
            .lines()
            .filter_map(|line| {
                let json_start = line.find('{')?;
                serde_json::Deserializer::from_str(&line[json_start..])
                    .into_iter::<serde_json::Value>()
                    .next()?
                    .ok()
            })
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|message| message["type"] == "ready"
                && message["session_id"] == "maestro-session-1"),
            "unexpected headless messages: {messages:?}"
        );
        let receipts = messages
            .iter()
            .filter(|message| message["type"] == "workspace_capability_set_applied")
            .collect::<Vec<_>>();
        assert_eq!(receipts.len(), 2);
        let calls = std::fs::read_to_string(log)
            .expect("provider calls")
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect::<Vec<_>>();
        let thread_starts = calls
            .iter()
            .filter(|call| call["method"] == "thread/start")
            .collect::<Vec<_>>();
        assert_eq!(thread_starts.len(), 2);
        for (receipt, thread_start) in receipts.iter().zip(thread_starts.iter()) {
            let provider_prompt = thread_start["params"]["developerInstructions"]
                .as_str()
                .expect("provider developer instructions");
            assert_eq!(
                receipt["receipt"]["provider_prompt_sha256"],
                format!(
                    "sha256:{:x}",
                    sha2::Sha256::digest(provider_prompt.as_bytes())
                ),
                "provider developer instructions: {provider_prompt}"
            );
        }
        assert_eq!(receipts[1]["receipt"]["activation_generation"], 2);
        let turns = calls
            .iter()
            .filter(|call| call["method"] == "turn/start")
            .collect::<Vec<_>>();
        assert_eq!(turns.len(), 2);
        assert_eq!(
            turns[0]["params"]["threadId"], "provider-thread-1",
            "provider calls: {calls:?}"
        );
        assert_eq!(
            turns[1]["params"]["threadId"], "provider-thread-2",
            "provider calls: {calls:?}"
        );
        let restored = calls
            .iter()
            .find(|call| call["method"] == "thread/inject_items")
            .expect("generation two history injection");
        assert_eq!(restored["params"]["threadId"], "provider-thread-2");
        assert!(
            restored["params"]["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "provider rebind must retain the first Maestro turn"
        );
        let bindings = std::fs::read_dir(home.join("codex/thread-bindings"))
            .expect("thread bindings directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .map(|path| {
                serde_json::from_slice::<crate::codex_session::CodexThreadBinding>(
                    &std::fs::read(&path).expect("thread binding"),
                )
                .expect("valid thread binding")
            })
            .collect::<Vec<_>>();
        assert_eq!(bindings.len(), 1, "active thread bindings: {bindings:?}");
        assert_eq!(bindings[0].thread_id, "provider-thread-2");
        assert_eq!(
            bindings[0].key.session_id.as_deref(),
            Some("maestro-session-1"),
            "replacement binding must remain scoped to the resident Maestro session"
        );
    }

    #[test]
    fn native_server_capabilities_match_the_registry_and_request_surface() {
        let capabilities = crate::headless::native_server_capabilities();
        assert_eq!(
            capabilities.utility_operations,
            vec![
                crate::headless::UtilityOperation::CommandExec,
                crate::headless::UtilityOperation::FileSearch,
                crate::headless::UtilityOperation::FileRead,
                crate::headless::UtilityOperation::FileWatch,
            ]
        );
        assert!(capabilities.raw_agent_events);
        assert_eq!(
            capabilities.server_requests,
            vec![
                crate::headless::ServerRequestType::Approval,
                crate::headless::ServerRequestType::ClientTool,
                crate::headless::ServerRequestType::UserInput,
                crate::headless::ServerRequestType::ToolRetry,
            ]
        );

        let mut expected = crate::tools::ToolRegistry::new()
            .tools()
            .map(|definition| {
                let name = definition.tool.name.clone();
                crate::headless::NativeToolCapability {
                    name: name.clone(),
                    requires_approval: definition.requires_approval,
                    version: crate::tools::versions::is_version_managed(&name)
                        .then(|| "current".to_string()),
                }
            })
            .collect::<Vec<_>>();
        expected.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(capabilities.native_tools, expected);

        let bash = capabilities
            .native_tools
            .iter()
            .find(|tool| tool.name == "bash")
            .expect("registry must advertise bash");
        assert!(bash.requires_approval);
        assert_eq!(bash.version.as_deref(), Some("current"));
    }

    #[test]
    fn coarser_transcripts_coalesce_text_and_drop_thinking() {
        let mut chunks = vec![
            ("reasoning".to_string(), true),
            ("hello ".to_string(), false),
            ("world".to_string(), false),
        ];
        assert_eq!(coalesce_response_chunks(&mut chunks), "hello world");
        assert!(chunks.is_empty());
    }

    #[tokio::test]
    async fn native_semantic_snapshot_keeps_processed_queue_ids_on_headless_wire() {
        const FIXTURE: &str = "MAESTRO_HEADLESS_SNAPSHOT_IDS_FIXTURE";
        if std::env::var_os(FIXTURE).is_some() {
            let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
            let (tool_tx, _tool_rx) = mpsc::unbounded_channel();
            handle_agent_event(
                FromAgent::ConversationSnapshot {
                    protocol_version: crate::headless::messages::SEMANTIC_CONVERSATION_PROTOCOL
                        .to_owned(),
                    messages: vec![],
                    processed_queue_ids: vec![7, 9],
                },
                &meta,
                &tool_tx,
                "test-model",
                None,
            )
            .await
            .expect("emit snapshot");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("headless_server::tests::native_semantic_snapshot_keeps_processed_queue_ids_on_headless_wire")
            .args(["--exact", "--nocapture", "--format", "terse"])
            .env(FIXTURE, "1")
            .output().expect("run snapshot fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| event["type"] == "conversation_snapshot")
            .expect("private snapshot on headless wire");
        assert_eq!(snapshot["processed_queue_ids"], serde_json::json!([7, 9]));
    }

    #[test]
    fn delta_transcript_does_not_buffer_emitted_response_chunks() {
        let mut meta = RuntimeMeta {
            transcript_grade: crate::transcript::TranscriptGrade::Delta,
            ..RuntimeMeta::default()
        };

        for _ in 0..10_000 {
            assert_eq!(
                meta.record_response_chunk("already emitted", false),
                crate::transcript::TranscriptGrade::Delta,
            );
        }

        assert!(meta.response_chunks.is_empty());
    }

    #[test]
    fn client_tool_content_preserves_text_and_images() {
        let result = client_content_to_agent_result(
            vec![
                ClientToolResultContent::Text {
                    text: "done".to_string(),
                },
                ClientToolResultContent::Image {
                    data: "AAAA".to_string(),
                    mime_type: "image/png".to_string(),
                },
            ],
            false,
        );
        assert!(result.success);
        assert_eq!(result.output, "done\ndata:image/png;base64,AAAA");
        assert_eq!(result.error, None);
    }

    #[test]
    fn request_resolution_maps_each_response_shape() {
        assert_eq!(
            server_request_resolution(ServerRequestType::Approval, Some(false), None, None, None,),
            ServerRequestResolutionStatus::Denied
        );
        assert_eq!(
            server_request_resolution(
                ServerRequestType::ToolRetry,
                None,
                None,
                None,
                Some(ToolRetryDecisionAction::Skip),
            ),
            ServerRequestResolutionStatus::Skipped
        );
    }

    #[test]
    fn resolve_tool_approval_leaves_auto_exec_to_native() {
        assert_eq!(resolve_tool_approval(false, None), None);
        assert_eq!(resolve_tool_approval(false, Some(ApprovalMode::Auto)), None);
        assert_eq!(resolve_tool_approval(false, Some(ApprovalMode::Fail)), None);
        assert_eq!(
            resolve_tool_approval(false, Some(ApprovalMode::Prompt)),
            None
        );
    }

    #[test]
    fn resolve_tool_approval_honors_mode_for_gated_tools() {
        assert_eq!(
            resolve_tool_approval(true, Some(ApprovalMode::Auto)),
            Some(true)
        );
        assert_eq!(
            resolve_tool_approval(true, Some(ApprovalMode::Fail)),
            Some(false)
        );
        assert_eq!(
            resolve_tool_approval(true, Some(ApprovalMode::Prompt)),
            None
        );
        assert_eq!(resolve_tool_approval(true, None), None);
    }

    #[test]
    fn tool_output_content_prefers_stdout() {
        let ok = ToolResult::success("hello from tool");
        assert_eq!(tool_output_content(&ok).as_deref(), Some("hello from tool"));

        let empty_ok = ToolResult::success("");
        assert_eq!(tool_output_content(&empty_ok), None);

        let fail = ToolResult::failure("boom");
        assert_eq!(tool_output_content(&fail).as_deref(), Some("Error: boom"));

        let fail_with_partial = ToolResult {
            success: false,
            output: "partial".into(),
            error: Some("exit 1".into()),
            details: None,
        };
        assert_eq!(
            tool_output_content(&fail_with_partial).as_deref(),
            Some("partial")
        );
    }

    #[test]
    fn tool_lifecycle_messages_include_tool_output() {
        let result = ToolResult::success("file contents");
        let msgs = tool_lifecycle_messages(
            "call-1",
            Some("tool-execution-1"),
            Some("read".into()),
            &result,
        );
        assert_eq!(msgs.len(), 3);
        assert!(matches!(
            &msgs[0],
            FromAgentMessage::ToolStart { call_id } if call_id == "call-1"
        ));
        assert!(matches!(
            &msgs[1],
            FromAgentMessage::ToolOutput { call_id, content }
                if call_id == "call-1" && content == "file contents"
        ));
        assert!(matches!(
            &msgs[2],
            FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                success: true,
                tool: Some(t),
                ..
            }) if call_id == "call-1"
                && tool_execution_id == "tool-execution-1"
                && t == "read"
        ));
    }

    #[test]
    fn native_tool_end_wire_message_preserves_correlated_safe_name() {
        let receipt = crate::agent::protocol::ToolExecution::from_legacy(
            "call-write-1",
            "codex_file_change",
            ExecutionSource::Native,
            ToolResult::success("completed"),
        )
        .receipt;

        assert!(matches!(
            tool_end_message(
                "call-write-1".to_owned(),
                None,
                true,
                Some(receipt),
            ),
            FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                tool_execution_id: None,
                success: true,
                tool: Some(tool),
                receipt: Some(receipt),
                ..
            }) if call_id == "call-write-1"
                && tool == "codex_file_change"
                && receipt.call_id == "call-write-1"
                && receipt.tool_name == "codex_file_change"
        ));
    }

    #[test]
    fn tool_lifecycle_messages_omit_empty_success_output() {
        let result = ToolResult::success("");
        let msgs = tool_lifecycle_messages("call-2", None, None, &result);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], FromAgentMessage::ToolStart { .. }));
        assert!(matches!(
            msgs[1],
            FromAgentMessage::ToolEnd(tool_wire::ToolEnd { success: true, .. })
        ));
    }

    #[test]
    fn governed_denial_emits_correlated_terminal_failure() {
        let result = ToolResult::failure("denied").with_details(serde_json::json!({
            "decision": "deny"
        }));
        let message = denied_tool_terminal_message(
            "call-denied",
            Some("tool-execution-denied"),
            Some(&result),
        )
        .expect("governed denial terminal message");

        assert!(matches!(
            message,
            FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                success: false,
                details: Some(details),
                ..
            }) if call_id == "call-denied"
                && tool_execution_id == "tool-execution-denied"
                && details["decision"] == "deny"
        ));
        assert!(
            denied_tool_terminal_message("call-local", None, Some(&result)).is_none(),
            "ungoverned denials have no durable execution to correlate"
        );
    }

    #[test]
    fn governed_tool_decisions_are_single_use_at_the_server_boundary() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        let (tool_tx, mut tool_rx) = mpsc::unbounded_channel::<ToolResponseMessage>();

        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("call-1".to_string());
        let accepted = prepare_tool_response(
            &meta,
            "call-1".to_string(),
            Some("execution-1".to_string()),
            true,
            None,
        )
        .expect("first decision accepted");
        assert!(accepted.messages.is_empty());
        assert!(
            tool_rx.try_recv().is_err(),
            "preparing lifecycle output must not deliver the native decision first"
        );
        tool_tx
            .send(accepted.agent_response)
            .expect("deliver first decision after lifecycle output");
        assert!(matches!(
            tool_rx.try_recv(),
            Ok((call_id, true, None, ExecutionSource::RemoteClient, _)) if call_id == "call-1"
        ));

        let error = prepare_tool_response(
            &meta,
            "call-1".to_string(),
            Some("execution-1".to_string()),
            false,
            None,
        )
        .expect_err("approve then deny must be rejected");
        assert!(error.contains("already has"));
        assert!(tool_rx.try_recv().is_err());

        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("call-2".to_string());
        let accepted = prepare_tool_response(
            &meta,
            "call-2".to_string(),
            Some("execution-2".to_string()),
            false,
            None,
        )
        .expect("first denial accepted");
        assert!(matches!(
            accepted.messages.as_slice(),
            [FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                tool_execution_id: Some(tool_execution_id),
                success: false,
                ..
            })] if tool_execution_id == "execution-2"
        ));
        assert!(
            tool_rx.try_recv().is_err(),
            "the server must emit the correlated denial before native delivery"
        );
        tool_tx
            .send(accepted.agent_response)
            .expect("deliver denial after terminal lifecycle");
        assert!(matches!(
            tool_rx.try_recv(),
            Ok((call_id, false, None, ExecutionSource::RemoteClient, _)) if call_id == "call-2"
        ));
        assert!(
            prepare_tool_response(
                &meta,
                "call-2".to_string(),
                Some("execution-2".to_string()),
                true,
                None,
            )
            .is_err(),
            "deny then approve must be rejected"
        );
        assert!(tool_rx.try_recv().is_err());

        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("call-completed".to_string());
        let accepted = prepare_tool_response(
            &meta,
            "call-completed".to_string(),
            Some("execution-completed".to_string()),
            true,
            Some(HeadlessToolResult {
                success: true,
                output: "completed externally".to_string(),
                error: None,
                details: None,
            }),
        )
        .expect("completed client result accepted");
        assert!(matches!(
            accepted.messages.last(),
            Some(FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                tool_execution_id: Some(tool_execution_id),
                success: true,
                ..
            })) if tool_execution_id == "execution-completed"
        ));
        assert!(
            tool_rx.try_recv().is_err(),
            "completed lifecycle must be emitted before native delivery"
        );
        tool_tx
            .send(accepted.agent_response)
            .expect("deliver completed result after lifecycle output");
        assert!(matches!(
            tool_rx.try_recv(),
            Ok((call_id, true, Some(result), ExecutionSource::RemoteClient, _))
                if call_id == "call-completed" && result.success
        ));

        let error = prepare_tool_response(
            &meta,
            "call-1".to_string(),
            Some("execution-3".to_string()),
            true,
            None,
        )
        .expect_err("an active call id must not be rebound to a new execution");
        assert!(error.contains("already has an active decision"));
        assert!(tool_rx.try_recv().is_err());

        meta.lock()
            .expect("runtime metadata")
            .tool_execution_ids
            .remove("call-1")
            .expect("simulate the prior native ToolEnd lifecycle boundary");
        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("call-1".to_string());
        let accepted = prepare_tool_response(
            &meta,
            "call-1".to_string(),
            Some("execution-3".to_string()),
            true,
            None,
        )
        .expect("call id reuse after the prior terminal boundary remains valid");
        tool_tx
            .send(accepted.agent_response)
            .expect("deliver distinct execution decision");
        assert!(matches!(
            tool_rx.try_recv(),
            Ok((call_id, true, None, ExecutionSource::RemoteClient, _)) if call_id == "call-1"
        ));

        for _ in 0..2 {
            meta.lock()
                .expect("runtime metadata")
                .pending_tool_calls
                .insert("legacy-call".to_string());
            let accepted =
                prepare_tool_response(&meta, "legacy-call".to_string(), None, true, None)
                    .expect("a registered legacy decision remains valid");
            tool_tx
                .send(accepted.agent_response)
                .expect("deliver legacy decision");
            assert!(tool_rx.try_recv().is_ok());
        }
    }

    #[test]
    fn governed_tool_response_rejects_an_unmatched_call_without_lifecycle_output() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));

        let error = prepare_tool_response(
            &meta,
            "mistyped-call".to_string(),
            Some("execution-mistyped".to_string()),
            false,
            Some(HeadlessToolResult {
                success: false,
                output: String::new(),
                error: Some("denied".to_string()),
                details: None,
            }),
        )
        .expect_err("unmatched governed response must be rejected");

        assert!(error.contains("not awaiting a decision"));
        let meta = meta.lock().expect("runtime metadata");
        assert!(meta.tool_execution_ids.is_empty());
        assert!(meta.decided_tool_execution_ids.is_empty());
    }

    #[tokio::test]
    async fn native_channel_send_failure_restores_governed_response_for_retry() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("retry-call".to_string());
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        drop(closed_rx);
        let accepted = prepare_tool_response(
            &meta,
            "retry-call".to_string(),
            Some("retry-execution".to_string()),
            true,
            None,
        )
        .expect("initial governed response");

        let error =
            dispatch_accepted_tool_response(&meta, &closed_tx, accepted, "retry-call".to_string())
                .expect_err("closed native channel must reject delivery");
        assert!(error.contains("channel is closed"));
        {
            let meta = meta.lock().expect("runtime metadata");
            assert!(meta.pending_tool_calls.contains("retry-call"));
            assert!(!meta.decided_tool_execution_ids.contains("retry-execution"));
            assert!(!meta.tool_execution_ids.contains_key("retry-call"));
        }

        let (retry_tx, mut retry_rx) = mpsc::unbounded_channel();
        let corrected = prepare_tool_response(
            &meta,
            "retry-call".to_string(),
            Some("retry-execution".to_string()),
            true,
            None,
        )
        .expect("corrected governed retry");
        dispatch_accepted_tool_response(&meta, &retry_tx, corrected, "retry-call".to_string())
            .expect("retry reaches native channel");
        assert!(matches!(
            retry_rx.try_recv(),
            Ok((call_id, true, None, ExecutionSource::RemoteClient, _))
                if call_id == "retry-call"
        ));
    }

    #[tokio::test]
    async fn rejected_consumption_receipt_restores_governed_response_for_retry() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("receipt-call".to_string());
        let (tool_tx, mut tool_rx) = mpsc::unbounded_channel();
        let accepted = prepare_tool_response(
            &meta,
            "receipt-call".to_string(),
            Some("receipt-execution".to_string()),
            true,
            None,
        )
        .expect("initial governed response");

        dispatch_accepted_tool_response(&meta, &tool_tx, accepted, "receipt-call".to_string())
            .expect("response reaches native channel");
        let (_, _, _, _, consumed) = tool_rx.recv().await.expect("queued native response");
        consumed
            .expect("queued response carries a consumption receipt sender")
            .send(ToolResponseConsumption::Rejected {
                reason: "tool response cancelled before native consumption".to_string(),
            })
            .expect("consumption receipt is delivered");

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let meta = meta.lock().expect("runtime metadata");
                    if meta.pending_tool_calls.contains("receipt-call")
                        && !meta
                            .decided_tool_execution_ids
                            .contains("receipt-execution")
                        && !meta.tool_execution_ids.contains_key("receipt-call")
                    {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("rejected receipt restores the pending decision");

        let corrected = prepare_tool_response(
            &meta,
            "receipt-call".to_string(),
            Some("receipt-execution".to_string()),
            true,
            None,
        )
        .expect("corrected governed retry");
        dispatch_accepted_tool_response(&meta, &tool_tx, corrected, "receipt-call".to_string())
            .expect("retry reaches native channel");
        assert!(matches!(
            tool_rx.try_recv(),
            Ok((call_id, true, None, ExecutionSource::RemoteClient, _))
                if call_id == "receipt-call"
        ));
    }

    #[tokio::test]
    async fn dropped_response_receipt_restores_governed_response_for_retry() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("dropped-call".to_string());
        let (tool_tx, tool_rx) = mpsc::unbounded_channel();
        let accepted = prepare_tool_response(
            &meta,
            "dropped-call".to_string(),
            Some("dropped-execution".to_string()),
            true,
            None,
        )
        .expect("initial governed response");

        dispatch_accepted_tool_response(&meta, &tool_tx, accepted, "dropped-call".to_string())
            .expect("response reaches native channel");
        // Dropping the receiver discards the queued message together with its
        // receipt sender: the native agent never consumed the response.
        drop(tool_rx);

        // Drain the recorded acknowledgement tasks exactly like the shutdown
        // path does, so the rollback is guaranteed to have run without
        // relying on scheduler timing.
        for task in take_receipt_tasks(&meta) {
            task.await.expect("receipt acknowledgement task");
        }
        {
            let meta = meta.lock().expect("runtime metadata");
            assert!(meta.pending_tool_calls.contains("dropped-call"));
            assert!(
                !meta
                    .decided_tool_execution_ids
                    .contains("dropped-execution")
            );
        }

        // The execution binding is preserved for the shutdown cleanup so the
        // exposed tool lifecycle is still closed with an interrupted ToolEnd.
        assert_eq!(
            meta.lock()
                .expect("runtime metadata")
                .tool_execution_ids
                .get("dropped-call")
                .map(String::as_str),
            Some("dropped-execution")
        );
        let terminals = take_interrupted_tool_terminal_messages(&meta);
        assert!(
            terminals.iter().any(|message| matches!(
                message,
                FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                    call_id,
                    tool_execution_id: Some(tool_execution_id),
                    success: false,
                    ..
                }) if call_id == "dropped-call" && tool_execution_id == "dropped-execution"
            )),
            "shutdown cleanup must terminalize the dropped governed response: {terminals:?}"
        );
    }

    #[test]
    fn deferred_consumption_rejection_is_a_correlated_protocol_error() {
        assert!(matches!(
            response_consumption_message(
                "cancelled-call".to_string(),
                ToolResponseConsumption::Rejected {
                    reason: "tool response cancelled before native consumption".to_string(),
                },
            ),
            FromAgentMessage::Error {
                request_id: Some(request_id),
                message,
                fatal: false,
                terminal: false,
                error_type: Some(HeadlessErrorType::Protocol),
            } if request_id == "cancelled-call"
                && message.contains("cancelled before native consumption")
        ));
    }

    async fn assert_lifecycle_precedes_native_acceptance(approved: bool) -> Vec<String> {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("ordered-call".to_string());
        let accepted = prepare_tool_response(
            &meta,
            "ordered-call".to_string(),
            Some("ordered-execution".to_string()),
            approved,
            Some(HeadlessToolResult {
                success: approved,
                output: "ordered output".to_string(),
                error: (!approved).then(|| "denied".to_string()),
                details: None,
            }),
        )
        .expect("ordered response");
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let (tool_tx, mut tool_rx) = mpsc::unbounded_channel::<ToolResponseMessage>();
        let receiver_events = Arc::clone(&events);
        tokio::spawn(async move {
            let (_, _, _, _, consumed) = tool_rx.recv().await.expect("native response");
            receiver_events
                .lock()
                .expect("ordered events")
                .push("native".to_string());
            consumed
                .expect("consumption receipt")
                .send(ToolResponseConsumption::Accepted)
                .ok();
        });
        let lifecycle_events = Arc::clone(&events);
        let acknowledgement_events = Arc::clone(&events);
        dispatch_accepted_tool_response_with(
            &meta,
            &tool_tx,
            accepted,
            "ordered-call".to_string(),
            move |message| {
                let label = match message {
                    FromAgentMessage::ToolStart { .. } => "start",
                    FromAgentMessage::ToolOutput { .. } => "output",
                    FromAgentMessage::ToolEnd(tool_wire::ToolEnd { .. }) => "end",
                    _ => "other",
                };
                lifecycle_events
                    .lock()
                    .expect("ordered events")
                    .push(label.to_string());
                Ok(())
            },
            move |_, outcome, _dropped| {
                assert_eq!(outcome, ToolResponseConsumption::Accepted);
                acknowledgement_events
                    .lock()
                    .expect("ordered events")
                    .push("accepted".to_string());
            },
        )
        .expect("ordered dispatch");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if events
                    .lock()
                    .expect("ordered events")
                    .last()
                    .map(String::as_str)
                    == Some("accepted")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("acceptance event");
        let ordered = events.lock().expect("ordered events").clone();
        ordered
    }

    #[tokio::test]
    async fn completed_response_lifecycle_precedes_native_acceptance() {
        assert_eq!(
            assert_lifecycle_precedes_native_acceptance(true).await,
            ["start", "output", "end", "native", "accepted"]
        );
    }

    #[tokio::test]
    async fn denied_response_terminal_lifecycle_precedes_native_acceptance() {
        assert_eq!(
            assert_lifecycle_precedes_native_acceptance(false).await,
            ["end", "native", "accepted"]
        );
    }

    #[test]
    fn client_tool_result_requires_and_consumes_a_pending_call() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));

        let error = prepare_client_tool_result(
            &meta,
            "mistyped-client-call".to_string(),
            vec![ClientToolResultContent::Text {
                text: "ok".to_string(),
            }],
            false,
            ClientToolResultBinding::default(),
        )
        .expect_err("an unmatched client result must be rejected");
        assert!(error.contains("not awaiting a decision"));

        meta.lock()
            .expect("runtime metadata")
            .pending_tool_calls
            .insert("client-call".to_string());
        let accepted = prepare_client_tool_result(
            &meta,
            "client-call".to_string(),
            vec![ClientToolResultContent::Text {
                text: "ok".to_string(),
            }],
            false,
            ClientToolResultBinding::default(),
        )
        .expect("a registered client result must be accepted");
        assert!(matches!(
            accepted.messages.last(),
            Some(FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                success: true,
                ..
            })) if call_id == "client-call"
        ));
        assert!(
            prepare_client_tool_result(
                &meta,
                "client-call".to_string(),
                vec![ClientToolResultContent::Text {
                    text: "ok".to_string(),
                }],
                false,
                ClientToolResultBinding::default(),
            )
            .is_err(),
            "the pending call must be consumed exactly once"
        );
    }

    #[test]
    fn interrupted_governed_tools_emit_correlated_terminal_failures() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        {
            let mut meta = meta.lock().expect("runtime metadata");
            meta.tool_execution_ids
                .insert("call-b".to_string(), "execution-b".to_string());
            meta.tool_execution_ids
                .insert("call-a".to_string(), "execution-a".to_string());
            meta.pending_tool_calls.insert("call-pending".to_string());
        }

        let messages = take_interrupted_tool_terminal_messages(&meta);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                success: false,
                ..
            }) if call_id == "call-a" && tool_execution_id == "execution-a"
        ));
        assert!(matches!(
            &messages[1],
            FromAgentMessage::ToolEnd (tool_wire::ToolEnd {
                call_id,
                tool_execution_id: Some(tool_execution_id),
                success: false,
                ..
            }) if call_id == "call-b" && tool_execution_id == "execution-b"
        ));
        let meta = meta.lock().expect("runtime metadata");
        assert!(meta.tool_execution_ids.is_empty());
        assert!(meta.pending_tool_calls.is_empty());
    }

    #[test]
    fn interrupted_cleanup_does_not_repeat_an_emitted_governed_terminal() {
        let meta = Arc::new(Mutex::new(RuntimeMeta::default()));
        {
            let mut runtime = meta.lock().expect("runtime metadata");
            runtime
                .tool_execution_ids
                .insert("closed-call".to_string(), "closed-execution".to_string());
            runtime
                .emitted_client_tool_terminals
                .insert("closed-execution".to_string());
            runtime.pending_tool_calls.insert("closed-call".to_string());
            runtime.pending_client_tools.insert(
                "closed-call".to_string(),
                PendingClientTool {
                    binding: ClientToolBinding {
                        provider_tool_name: "client_provider_id".to_string(),
                        tool_id: "tool-1".to_string(),
                        connection_binding_id: None,
                        logical_name: "deploy".to_string(),
                        owner: ClientToolExecutionOwner {
                            client_instance_id: "client-1".to_string(),
                            lease_epoch: 1,
                        },
                        grant_id: "grant-1".to_string(),
                        grant_version: 1,
                        grant_hash: "sha256:grant".to_string(),
                        turn_digest: "sha256:turn".to_string(),
                        definition_digest: "sha256:def".to_string(),
                        expires_at_ms: i64::MAX,
                    },
                    tool_execution_id: "closed-execution".to_string(),
                    args_digest: "sha256:args".to_string(),
                    idempotency_key: "client-tool:closed-execution".to_string(),
                    result_digest: None,
                },
            );
        }

        assert!(take_interrupted_tool_terminal_messages(&meta).is_empty());
    }

    #[test]
    fn ready_message_serializes_session_id_when_present() {
        let msg = FromAgentMessage::Ready {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            model: "gpt-test".into(),
            provider: "OpenAI".into(),
            session_id: Some("sess-abc".into()),
        };
        let json = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(json["type"], "ready");
        assert_eq!(json["session_id"], "sess-abc");
        assert_eq!(json["model"], "gpt-test");
    }

    #[test]
    fn ready_message_omits_null_session_id() {
        let msg = FromAgentMessage::Ready {
            protocol_version: Some(HEADLESS_PROTOCOL_VERSION.to_string()),
            model: "gpt-test".into(),
            provider: "OpenAI".into(),
            session_id: None,
        };
        let json = serde_json::to_value(&msg).expect("serialize");
        assert!(json.get("session_id").is_none());
    }

    #[test]
    fn env_session_id_reads_maestro_session_id() {
        assert_eq!(
            normalize_session_id(Some("  env-session-42  ")).as_deref(),
            Some("env-session-42")
        );
    }

    #[test]
    fn env_session_id_filters_blank_value() {
        assert_eq!(normalize_session_id(Some("   ")), None);
        assert_eq!(normalize_session_id(None), None);
    }

    #[test]
    fn candidate_prompt_experiment_reaches_provider_bound_system_prompt() {
        let artifact_content = "When debugging, state the causal path before proposing a fix.";
        let mut state = HeadlessState::new(Some("gpt-test".to_string()));
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
        let mut state = HeadlessState::new(Some("gpt-test".to_string()));
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

    fn process_cost_wire_result(cost: Option<u64>) -> ToAgentMessage {
        let mut wire = json!({
            "type": "governed_client_tool_result",
            "call_id": "cost-call", "content": [{"type":"text","text":"saved"}],
            "is_error": false, "tool_execution_id":"cost-execution",
            "client_instance_id":"client-1","grant_id":"grant-1","grant_version":2,
            "grant_hash":"sha256:grant","turn_digest":"sha256:turn",
            "definition_digest":"sha256:def","args_digest":"sha256:args",
            "owner_lease_epoch":9,"idempotency_key":"client-tool:cost-execution"
        });
        if let Some(cost) = cost {
            wire["process_tool_cost_micros"] = json!(cost);
        }
        serde_json::from_value(wire).expect("actual governed result wire")
    }

    #[tokio::test]
    async fn process_cost_wire_charges_before_native_continuation_and_rejects_changed_replay() {
        let workspace = tempfile::tempdir().unwrap();
        let scripted = crate::ai::ScriptedClient::new(
            "process-cost-wire",
            vec![crate::ai::ScriptedResponse::text(
                "must not call this model",
            )],
        );
        let (agent, mut events) = NativeAgent::new_with_test_client(
            NativeAgentConfig {
                model: "scripted/process-cost-wire".into(),
                cwd: workspace.path().display().to_string(),
                ..NativeAgentConfig::default()
            },
            crate::ai::UnifiedClient::Scripted(scripted.clone()),
        )
        .unwrap();
        let limits = crate::agent::process_budget::ProcessBudgetLimits {
            event_id: "event-1".into(),
            max_requests: 2,
            max_total_tokens: 100,
            max_cost_micros: 20,
            cost_micros_per_token: 1,
        };
        let budget = agent.install_process_budget(limits, None).await.unwrap();
        let mut state = HeadlessState::new(Some("scripted/process-cost-wire".into()));
        {
            let mut meta = state.meta.lock().unwrap();
            meta.process_budget = Some(Arc::clone(&budget));
            meta.pending_tool_calls.insert("cost-call".into());
            meta.pending_client_tools.insert(
                "cost-call".into(),
                PendingClientTool {
                    binding: ClientToolBinding {
                        provider_tool_name: "client_provider_id".into(),
                        tool_id: "tool-1".into(),
                        connection_binding_id: None,
                        logical_name: "save".into(),
                        owner: ClientToolExecutionOwner {
                            client_instance_id: "client-1".into(),
                            lease_epoch: 9,
                        },
                        grant_id: "grant-1".into(),
                        grant_version: 2,
                        grant_hash: "sha256:grant".into(),
                        turn_digest: "sha256:turn".into(),
                        definition_digest: "sha256:def".into(),
                        expires_at_ms: i64::MAX,
                    },
                    tool_execution_id: "cost-execution".into(),
                    args_digest: "sha256:args".into(),
                    idempotency_key: "client-tool:cost-execution".into(),
                    result_digest: None,
                },
            );
        }

        // An omitted private charge must leave the governed request pending.
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        state.tool_tx = Some(first_tx);
        handle_governed_client_tool_result(&state, process_cost_wire_result(None)).unwrap();
        assert!(first_rx.try_recv().is_err());
        assert_eq!(budget.lock().unwrap().cost_micros, 0);
        assert!(
            state
                .meta
                .lock()
                .unwrap()
                .pending_tool_calls
                .contains("cost-call")
        );

        // A disconnected native receiver models redelivery after an owner handoff.
        // The handler must retain the already-incurred charge while restoring the
        // exact governed request for retry.
        drop(first_rx);
        handle_governed_client_tool_result(&state, process_cost_wire_result(Some(21))).unwrap();
        assert_eq!(budget.lock().unwrap().cost_micros, 21);
        assert!(
            state
                .meta
                .lock()
                .unwrap()
                .pending_tool_calls
                .contains("cost-call")
        );
        handle_governed_client_tool_result(&state, process_cost_wire_result(Some(21))).unwrap();
        assert_eq!(
            budget.lock().unwrap().cost_micros,
            21,
            "redelivery charged twice"
        );

        // Replay cannot lower the owner-reported amount to regain model budget.
        let pending = state.meta.lock().unwrap().pending_client_tools["cost-call"].clone();
        let error = handle_governed_client_tool_result(&state, process_cost_wire_result(Some(1)))
            .expect_err("changed cost must fail closed");
        assert!(error.to_string().contains("changed on replay"), "{error:#}");
        assert_eq!(budget.lock().unwrap().cost_micros, 21);

        // Recreate the pending delivery after that fatal protocol failure using
        // the same owner-bound request and shared accounting checkpoint.
        {
            let mut meta = state.meta.lock().unwrap();
            meta.pending_tool_calls.insert("cost-call".into());
            meta.pending_client_tools
                .insert("cost-call".into(), pending);
        }
        let (tool_tx, mut tool_rx) = mpsc::unbounded_channel();
        state.tool_tx = Some(tool_tx);
        handle_governed_client_tool_result(&state, process_cost_wire_result(Some(21))).unwrap();
        let (_, _, result, _, consumed) = tool_rx.recv().await.expect("native result delivery");
        assert_eq!(result.unwrap().output, "saved");
        assert_eq!(
            budget.lock().unwrap().cost_micros,
            21,
            "native continuation must observe the charge before receiving the result"
        );
        consumed
            .unwrap()
            .send(ToolResponseConsumption::Accepted)
            .unwrap();

        // Run the real native request admission, rather than calling the budget
        // helper in the test: no provider response may be consumed after the tool
        // has exhausted the cost allowance.
        agent
            .prompt("Continue after saving".into(), vec![])
            .await
            .unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(FromAgent::Error {
                        message,
                        terminal: true,
                        ..
                    }) => break message,
                    Some(FromAgent::TurnCompleted { .. }) => panic!("over-budget turn completed"),
                    Some(FromAgent::ToolStart { .. }) => panic!("over-budget tool executed"),
                    Some(_) => {}
                    None => panic!("missing terminal budget refusal"),
                }
            }
        })
        .await
        .expect("bounded native refusal");
        assert!(error.contains("budget exhausted"), "{error}");
        assert_eq!(
            scripted.remaining(),
            1,
            "tool cost arrived after model request"
        );
        assert_eq!(budget.lock().unwrap().requests, 0);
        agent.shutdown().await;
    }
    #[tokio::test]
    async fn process_budget_rotates_only_after_verified_inactive_grant() {
        let workspace = tempfile::tempdir().unwrap();
        let scripted = crate::ai::ScriptedClient::new(
            "process-rotation",
            vec![crate::ai::ScriptedResponse::text("ordinary reply")],
        );
        let (agent, mut events) = NativeAgent::new_with_test_client(
            NativeAgentConfig {
                model: "scripted/process-rotation".into(),
                cwd: workspace.path().display().to_string(),
                system_prompt: Some("PROCESS ONLY".into()),
                ..NativeAgentConfig::default()
            },
            crate::ai::UnifiedClient::Scripted(scripted.clone()),
        )
        .unwrap();
        let mut process = test_grant();
        process.process_system_prompt = Some("PROCESS ONLY".into());
        process.process_budget = Some(crate::agent::process_budget::ProcessBudgetLimits {
            event_id: process.turn_id.clone(),
            max_requests: 1,
            max_total_tokens: 100,
            max_cost_micros: 100,
            cost_micros_per_token: 1,
        });
        sign_test_grant(&mut process);
        verify_governed_tool_grant_with_keys(
            &process,
            &test_grant_context(),
            1_000,
            &test_grant_keys(),
        )
        .unwrap();
        let budget = agent
            .install_process_budget(process.process_budget.clone().unwrap(), None)
            .await
            .unwrap();
        {
            let mut spent = budget.lock().unwrap();
            spent.admit_request().unwrap();
            spent.observe_usage(5, 0, None).unwrap();
        }
        let mut state = HeadlessState::new(Some("scripted/process-rotation".into()));
        state.agent = Some(agent);
        state.governed_grant = Some(process.clone());
        state.system_prompt = "PROCESS ONLY".into();
        state.meta.lock().unwrap().process_budget = Some(Arc::clone(&budget));
        state
            .apply_verified_governed_grant(process.clone())
            .await
            .unwrap();
        assert_eq!(
            budget.lock().unwrap().requests,
            1,
            "same grant must not reset usage"
        );

        let mut next_process = process;
        next_process.grant_id = "grant-process-2".into();
        next_process.turn_id = "turn-process-2".into();
        next_process.run_id = "run-process-2".into();
        next_process.process_budget.as_mut().unwrap().event_id = next_process.turn_id.clone();
        sign_test_grant(&mut next_process);
        let next_context = GovernedGrantVerificationContext {
            organization_id: "org-1",
            workspace_id: "workspace-1",
            thread_id: "thread-1",
            turn_id: "turn-process-2",
            run_id: "run-process-2",
            runtime_generation: 7,
        };
        verify_governed_tool_grant_with_keys(
            &next_process,
            &next_context,
            1_000,
            &test_grant_keys(),
        )
        .unwrap();
        state
            .apply_verified_governed_grant(next_process.clone())
            .await
            .unwrap();
        assert!(state.meta.lock().unwrap().process_budget.is_none());
        let next_budget = state
            .agent
            .as_ref()
            .unwrap()
            .install_process_budget(next_process.process_budget.clone().unwrap(), None)
            .await
            .unwrap();
        assert!(
            !Arc::ptr_eq(&budget, &next_budget),
            "new event needs new allowance"
        );
        assert_eq!(next_budget.lock().unwrap().requests, 0);
        {
            let mut spent = next_budget.lock().unwrap();
            spent.admit_request().unwrap();
            spent.observe_usage(3, 0, None).unwrap();
        }
        state.meta.lock().unwrap().process_budget = Some(Arc::clone(&next_budget));
        state
            .apply_verified_governed_grant(next_process)
            .await
            .unwrap();
        assert_eq!(next_budget.lock().unwrap().requests, 1);
        let mut ordinary = test_grant();
        ordinary.grant_id = "grant-ordinary".into();
        ordinary.turn_id = "turn-ordinary".into();
        ordinary.run_id = "run-ordinary".into();
        sign_test_grant(&mut ordinary);
        let context = GovernedGrantVerificationContext {
            organization_id: "org-1",
            workspace_id: "workspace-1",
            thread_id: "thread-1",
            turn_id: "turn-ordinary",
            run_id: "run-ordinary",
            runtime_generation: 7,
        };
        verify_governed_tool_grant_with_keys(&ordinary, &context, 1_000, &test_grant_keys())
            .unwrap();
        state.meta.lock().unwrap().turn_active = true;
        assert!(
            state
                .apply_verified_governed_grant(ordinary.clone())
                .await
                .is_err()
        );
        assert!(state.meta.lock().unwrap().process_budget.is_some());
        state.meta.lock().unwrap().turn_active = false;
        state.apply_verified_governed_grant(ordinary).await.unwrap();
        assert!(
            state.meta.lock().unwrap().process_budget.is_none(),
            "private cost requirement must be cleared"
        );
        assert!(!state.system_prompt.contains("PROCESS ONLY"));
        let agent = state.agent.as_ref().unwrap();
        assert!(
            !agent
                .runtime_audit_snapshot()
                .system_prompt
                .unwrap_or_default()
                .contains("PROCESS ONLY")
        );
        agent
            .prompt("ordinary prompt".into(), vec![])
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(FromAgent::TurnCompleted { .. }) => break,
                    Some(FromAgent::Error {
                        message,
                        terminal: true,
                        ..
                    }) => panic!("{message}"),
                    Some(_) => {}
                    None => panic!("missing ordinary completion"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            scripted.remaining(),
            0,
            "ordinary prompt reached the provider"
        );
        assert_eq!(
            budget.lock().unwrap().requests,
            1,
            "retired checkpoint is unchanged"
        );
        state.agent.take().unwrap().shutdown().await;
    }
}

#[cfg(test)]
#[test]
fn headless_receipt_model_does_not_automatically_boost_or_fallback() {
    let choice = crate::model_dynamics::ModelChoice {
        model: "openai/gpt-5".to_owned(),
        thinking: Default::default(),
    };
    let config = headless_model_dynamics(crate::model_dynamics::ModelDynamicsConfig {
        auto_boost: true,
        boost: Some(choice.clone()),
        fallbacks: vec![choice.clone()],
        light: Some(choice.clone()),
        summary_model: Some("openai/gpt-5-mini".to_owned()),
        ..Default::default()
    });
    assert!(!config.auto_boost);
    assert!(config.fallbacks.is_empty());
    assert_eq!(config.light, Some(choice));
    assert_eq!(config.summary_model.as_deref(), Some("openai/gpt-5-mini"));
}
