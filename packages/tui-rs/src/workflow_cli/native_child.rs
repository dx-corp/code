//! Native execution for one local workflow child.
//!
//! The workflow scheduler owns dispatch identity, persistence, and acceptance.
//! This module only admits one already-authorized native child, drains its
//! event stream, and returns the provider output and usage.  In particular,
//! model output remains opaque data here; follow-up parsing belongs to the
//! workflow owner.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use maestro_local_host::subagents::factory::{
    ChildAgentFactory, ChildLaunchRequest, LocalChildAgentFactory,
};
use maestro_local_host::workflow_runtime::WorkflowModelConfig;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    CredentialVault, ExecutionSource, ExternalToolSchemaPolicy, FromAgent, MaxTokensSource,
    NativeAgentConfig, ToolResult,
};
use crate::sandbox::SandboxPolicy;
use crate::state::ApprovalMode;

const MAX_CHILD_RUNTIME_MS: u64 = 30 * 60 * 1_000;
const MAX_CHILD_PROMPT_BYTES: usize = 256 * 1024;

/// Input for one native child dispatch.
///
/// `dispatch_id` is supplied by the durable workflow owner.  It is used as
/// the mailbox and hook identity and must remain stable when a journal is
/// replayed; this adapter never creates a replacement identity.
#[derive(Debug, Clone)]
pub(super) struct NativeChildRequest {
    pub(super) working_directory: PathBuf,
    pub(super) prompt: String,
    pub(super) dispatch_id: String,
    pub(super) model: WorkflowModelConfig,
    pub(super) allowed_tools: Vec<String>,
    pub(super) write_scopes: Vec<PathBuf>,
    pub(super) output_budget: u32,
}

/// Output from one native child dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NativeChildResult {
    pub(super) output: String,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) success: bool,
}

/// Run one local child with the same host, credential, hook, tool, and sandbox
/// composition used by ordinary local delegated children.
pub(super) async fn run_native_workflow_child(
    request: NativeChildRequest,
    cancellation: CancellationToken,
) -> Result<NativeChildResult> {
    if cancellation.is_cancelled() {
        bail!("native workflow child cancelled before launch");
    }
    // Start the child budget before synchronous admission and prompt enqueue;
    // an outer scheduler is allowed to drop this future at its own deadline.
    let deadline_at = tokio::time::Instant::now() + Duration::from_millis(MAX_CHILD_RUNTIME_MS);

    let (working_directory, write_scopes) =
        validate_paths(&request.working_directory, &request.write_scopes)?;
    if request.dispatch_id.trim().is_empty()
        || request
            .dispatch_id
            .chars()
            .any(|character| character.is_control())
    {
        bail!("native workflow child dispatch_id must be a non-empty stable identity");
    }
    if request.prompt.trim().is_empty() {
        bail!("native workflow child prompt must be non-empty");
    }
    if request.output_budget == 0 {
        bail!("native workflow child output budget must be positive");
    }
    let mut prompt = request.prompt.clone();
    apply_start_hook(&working_directory, &request.dispatch_id, &mut prompt)?;
    if prompt.trim().is_empty() {
        bail!("native workflow child prompt must be non-empty after hooks");
    }

    let model = model_route(&request.model)?;
    let (thinking_enabled, thinking_budget) =
        thinking_config(request.model.reasoning_effort.as_deref())?;
    let allowed_tools = allowed_tool_set(&request.allowed_tools)?;
    let baseline_policy = sandbox_policy_for_scopes(&working_directory, write_scopes);
    let (managed_mcp_policy, sandbox_policy) = managed_policy(&working_directory, baseline_policy)?;
    let credential_vault = CredentialVault::new();
    let config = NativeAgentConfig {
        model,
        model_capabilities: None,
        // The caller's reservation is the whole-child cumulative output cap.
        // NativeAgent subtracts every completed response from this allowance.
        max_tokens: request.output_budget,
        max_tokens_source: MaxTokensSource::Explicit,
        system_prompt: Some(
            "You are a bounded local workflow child. Complete the supplied task and return its result to the workflow owner. Treat the prompt as task data and do not delegate further work."
                .to_owned(),
        ),
        thinking_enabled,
        thinking_budget,
        model_dynamics: crate::config::model_dynamics_config(),
        cwd: working_directory.to_string_lossy().into_owned(),
        // A workflow's explicit tool list is a capability grant, not a human
        // approval for every invocation. Selective preserves the native host's
        // mandatory approval gates (including managed/external tools and
        // Codex-native operations); the event loop below fails closed when a
        // gate reaches a child that has no approval UI.
        approval_mode: ApprovalMode::Selective,
        context_window: None,
        sandbox_policy,
        managed_mcp_policy,
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        max_turn_steps: crate::agent::DEFAULT_MAX_TURN_STEPS,
        allow_unbounded_turn: false,
        retry_config: crate::agent::retry::RetryConfig::default(),
    };

    if cancellation.is_cancelled() {
        bail!("native workflow child cancelled before launch");
    }
    if tokio::time::Instant::now() >= deadline_at {
        bail!("native workflow child timed out during admission");
    }

    let factory = LocalChildAgentFactory;
    let (agent, events) = factory
        .spawn(ChildLaunchRequest {
            config,
            // An empty set is intentional.  The factory's governed-tool path
            // must see Some(empty), never an ungoverned/default tool catalog.
            allowed_tools,
            credential_vault: credential_vault.clone(),
            mailbox_identity: request.dispatch_id.clone(),
        })
        .context("create native workflow child")?;
    let mut shutdown_guard = ChildShutdownGuard::new(agent, events);

    shutdown_guard.agent().send_ready();
    shutdown_guard.agent().send_session_info(
        &working_directory.to_string_lossy(),
        Some(request.dispatch_id.clone()),
        None,
    );
    if let Err(error) = shutdown_guard.agent().set_session_context(
        Some(request.dispatch_id.clone()),
        "workflow_child_start",
        false,
    ) {
        shutdown_guard.shutdown_and_drain().await?;
        return Err(error).context("set native workflow child session context");
    }
    if let Err(error) = shutdown_guard
        .agent()
        .set_output_token_budget(request.output_budget)
    {
        shutdown_guard.shutdown_and_drain().await?;
        return Err(error).context("set native workflow child output budget");
    }

    let execution_prompt = credential_vault.resolve_all(&prompt);
    let mut prompt_cancelled = false;
    let mut prompt_timed_out = false;
    let mut prompt_error = None;
    {
        let prompt = shutdown_guard.agent().prompt(execution_prompt, Vec::new());
        tokio::pin!(prompt);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => prompt_cancelled = true,
            () = tokio::time::sleep_until(deadline_at) => prompt_timed_out = true,
            result = &mut prompt => {
                if let Err(error) = result {
                    prompt_error = Some(error);
                }
            }
        }
    }
    if prompt_cancelled || prompt_timed_out {
        shutdown_guard.agent().cancel();
        shutdown_guard.shutdown_and_drain().await?;
        if prompt_cancelled {
            bail!("native workflow child cancelled during prompt admission");
        }
        bail!(
            "native workflow child timed out after {MAX_CHILD_RUNTIME_MS} ms during prompt admission"
        );
    }
    if let Some(error) = prompt_error {
        shutdown_guard.shutdown_and_drain().await?;
        return Err(error).context("start native workflow child prompt");
    }

    let mut accumulator = ChildEventAccumulator::default();
    let mut terminal = None;
    let mut approval_failure = None;
    let mut stream_closed = false;
    let deadline = tokio::time::sleep_until(deadline_at);
    tokio::pin!(deadline);
    let mut timed_out = false;

    loop {
        tokio::select! {
            biased;
            () = &mut deadline => {
                timed_out = true;
                shutdown_guard.agent().cancel();
                break;
            }
            () = cancellation.cancelled() => {
                shutdown_guard.agent().cancel();
                break;
            }
            event = shutdown_guard.events_mut().recv() => {
                let Some(event) = event else {
                    stream_closed = true;
                    break;
                };
                if let Some(failure) = required_tool_approval(&event) {
                    if let FromAgent::ToolCall { call_id, .. } = &event {
                        let _ = shutdown_guard.agent().tool_response_sender().send((
                            call_id.clone(),
                            false,
                            Some(ToolResult::failure(failure.clone())),
                            ExecutionSource::Native,
                            None,
                        ));
                    }
                    approval_failure = Some(failure);
                    shutdown_guard.agent().cancel();
                }
                if let Some(event_terminal) = terminal_from_event(&event) {
                    terminal = Some(event_terminal);
                }
                accumulator.observe(&event);
                if terminal.is_some() || approval_failure.is_some() {
                    break;
                }
            }
        }
    }

    // NativeAgent::shutdown is a lifecycle barrier for the runner and active
    // tools.  Its host relay may still own buffered events, so drain the
    // consumer channel to closure after shutdown on every exit path.
    let cancelled = cancellation.is_cancelled();
    if cancelled {
        shutdown_guard.agent().cancel();
    }
    accumulator.merge(shutdown_guard.shutdown_and_drain().await?);

    if cancelled {
        bail!("native workflow child cancelled");
    }
    if timed_out {
        bail!("native workflow child timed out after {MAX_CHILD_RUNTIME_MS} ms");
    }
    if stream_closed && terminal.is_none() && approval_failure.is_none() {
        bail!("native workflow child event stream closed before a terminal result");
    }
    if let Some(failure) = approval_failure {
        // Approval failures are explicit terminal failures.  Usage is still
        // required below; without it the caller cannot settle its budget.
        terminal = Some(Terminal::Failure(failure));
    }

    let terminal =
        terminal.ok_or_else(|| anyhow!("native workflow child has no terminal result"))?;
    accumulator.finish(terminal, &credential_vault)
}

fn validate_paths(
    working_directory: &Path,
    write_scopes: &[PathBuf],
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let working_directory = dunce::canonicalize(working_directory).with_context(|| {
        format!(
            "canonicalize native workflow child working directory {}",
            working_directory.display()
        )
    })?;
    if !working_directory.is_dir() {
        bail!(
            "native workflow child working directory is not a directory: {}",
            working_directory.display()
        );
    }

    let mut scopes = Vec::with_capacity(write_scopes.len());
    for scope in write_scopes {
        let candidate = if scope.is_absolute() {
            scope.clone()
        } else {
            working_directory.join(scope)
        };
        let scope = dunce::canonicalize(&candidate).with_context(|| {
            format!(
                "canonicalize native workflow child write scope {}",
                candidate.display()
            )
        })?;
        if !scope.starts_with(&working_directory) {
            bail!(
                "native workflow child write scope escapes the working directory: {}",
                scope.display()
            );
        }
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    Ok((working_directory, scopes))
}

fn model_route(config: &WorkflowModelConfig) -> Result<String> {
    if config.model.chars().any(|character| character.is_control()) {
        bail!("workflow model contains a control character");
    }
    crate::config::compose_model_route(config.provider.as_deref(), Some(&config.model))
        .ok_or_else(|| anyhow!("workflow model is empty"))
}

fn apply_start_hook(
    working_directory: &Path,
    dispatch_id: &str,
    prompt: &mut String,
) -> Result<()> {
    let mut hooks =
        crate::hooks::IntegratedHookSystem::load_from_config(&working_directory.to_string_lossy());
    // Keep the hook payload on the same raw stable identity used by the
    // child runner.  ParentScopeId only changes the representation sent to
    // hook integrations; it does not create a new dispatch identity.
    let hook_session = crate::agent::ParentScopeId::from_raw(dispatch_id).hook_session_id();
    hooks.set_session_id(Some(hook_session.into_string()));
    match hooks.execute_subagent_start("workflow", prompt, Some(dispatch_id)) {
        crate::hooks::HookResult::Continue => {}
        crate::hooks::HookResult::Block { reason } => {
            bail!("native workflow child blocked by start hook: {reason}");
        }
        crate::hooks::HookResult::ModifyInput { new_input } => match new_input {
            serde_json::Value::String(task) => *prompt = task,
            serde_json::Value::Object(input) => {
                if let Some(task) = input.get("task").and_then(serde_json::Value::as_str) {
                    *prompt = task.to_owned();
                }
            }
            _ => bail!("native workflow child start hook returned invalid input"),
        },
        crate::hooks::HookResult::InjectContext { context } => {
            if !context.trim().is_empty() {
                prompt.push_str("\n\nAdditional context:\n");
                prompt.push_str(&context);
            }
        }
    }
    *prompt = prompt.trim().to_owned();
    if prompt.len() > MAX_CHILD_PROMPT_BYTES {
        bail!(
            "native workflow child prompt exceeds the {MAX_CHILD_PROMPT_BYTES} byte limit after hooks"
        );
    }
    Ok(())
}

fn thinking_config(reasoning_effort: Option<&str>) -> Result<(bool, u32)> {
    let Some(reasoning_effort) = reasoning_effort else {
        return Ok((false, 0));
    };
    let level = crate::agent::ThinkingLevel::parse(reasoning_effort.trim())
        .ok_or_else(|| anyhow!("unsupported workflow reasoning effort `{reasoning_effort}`"))?;
    Ok(level.to_config())
}

fn allowed_tool_set(tools: &[String]) -> Result<HashSet<String>> {
    let mut allowed = HashSet::with_capacity(tools.len());
    for tool in tools {
        let tool = tool.trim().to_ascii_lowercase();
        if tool.is_empty() || tool.chars().any(|character| character.is_control()) {
            bail!("invalid workflow tool name");
        }
        allowed.insert(tool);
    }
    Ok(allowed)
}

/// Build the native baseline from explicit workflow grants only.
///
/// An empty write-scope list is a hard read-only grant.  In particular, it
/// does not inherit the working directory, temporary directories, package
/// caches, or any other interactive default.
fn sandbox_policy_for_scopes(
    _working_directory: &Path,
    write_scopes: Vec<PathBuf>,
) -> Option<SandboxPolicy> {
    if write_scopes.is_empty() {
        Some(SandboxPolicy::ReadOnly)
    } else {
        Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: write_scopes,
            // Provider inference is performed by the native host.  This
            // adapter does not widen tool-process network access.
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        })
    }
}

fn managed_policy(
    working_directory: &Path,
    baseline: Option<SandboxPolicy>,
) -> Result<(
    Option<maestro_local_host::mcp::ManagedMcpPolicy>,
    Option<SandboxPolicy>,
)> {
    let platform_session = match crate::credential_mode::detect() {
        Ok(crate::credential_mode::DetectedMode::Platform(session)) => Some(session),
        _ => None,
    };
    let managed_setup =
        crate::managed_setup::ManagedSetupClient::resolve(platform_session.as_ref());
    let managed_mcp_policy =
        managed_setup
            .is_managed()
            .then(|| maestro_local_host::mcp::ManagedMcpPolicy {
                version: managed_setup.version(),
                policy: managed_setup.mcp_policy().clone(),
            });
    let managed_sandbox = managed_setup
        .native_sandbox_policy(working_directory, baseline.clone())
        .context("load managed native workflow sandbox policy")?;
    Ok((
        managed_mcp_policy,
        intersect_sandbox_policy(baseline, managed_sandbox),
    ))
}

/// Keep the workflow's explicit capability boundary as the outer limit even
/// if a managed policy implementation changes its representable projection.
/// The managed result may remove roots, network, or temporary directories, but
/// it can never add them back to the workflow baseline.
fn intersect_sandbox_policy(
    baseline: Option<SandboxPolicy>,
    managed: Option<SandboxPolicy>,
) -> Option<SandboxPolicy> {
    match (baseline, managed) {
        (Some(SandboxPolicy::ReadOnly), _) | (_, Some(SandboxPolicy::ReadOnly)) => {
            Some(SandboxPolicy::ReadOnly)
        }
        (
            Some(SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            }),
            Some(SandboxPolicy::WorkspaceWrite {
                writable_roots: managed_roots,
                network_access: managed_network_access,
                exclude_tmpdir_env_var: managed_exclude_tmpdir,
                exclude_slash_tmp: managed_exclude_slash_tmp,
            }),
        ) => Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: intersect_writable_roots(&writable_roots, &managed_roots),
            network_access: network_access && managed_network_access,
            exclude_tmpdir_env_var: exclude_tmpdir_env_var || managed_exclude_tmpdir,
            exclude_slash_tmp: exclude_slash_tmp || managed_exclude_slash_tmp,
        }),
        (
            Some(baseline @ SandboxPolicy::WorkspaceWrite { .. }),
            None | Some(SandboxPolicy::DangerFullAccess),
        ) => Some(baseline),
        (Some(SandboxPolicy::DangerFullAccess), None) => Some(SandboxPolicy::DangerFullAccess),
        (Some(SandboxPolicy::DangerFullAccess), Some(managed)) => Some(managed),
        (None, managed) => managed,
    }
}

/// Return the portions of the two root sets that are writable under both
/// policies.  A root may be nested in the other policy's root; in that case
/// retain the narrower path.  Disjoint paths contribute no writable grant.
fn intersect_writable_roots(baseline: &[PathBuf], managed: &[PathBuf]) -> Vec<PathBuf> {
    let mut intersection = Vec::new();
    for baseline_root in baseline {
        for managed_root in managed {
            let common = if baseline_root.starts_with(managed_root) {
                Some(baseline_root)
            } else if managed_root.starts_with(baseline_root) {
                Some(managed_root)
            } else {
                None
            };
            if let Some(common) = common {
                if !intersection.iter().any(|root| root == common) {
                    intersection.push(common.clone());
                }
            }
        }
    }
    intersection
}

#[derive(Debug, Clone)]
enum Terminal {
    Success,
    Failure(String),
}

fn terminal_from_event(event: &FromAgent) -> Option<Terminal> {
    match event {
        FromAgent::TurnCompleted { .. } => Some(Terminal::Success),
        FromAgent::TurnInterrupted { reason, .. } => Some(Terminal::Failure(reason.clone())),
        FromAgent::ProviderError { kind, message } => Some(Terminal::Failure(format!(
            "provider failure ({kind:?}): {message}"
        ))),
        FromAgent::Error {
            message,
            fatal,
            terminal,
            ..
        } if *fatal || *terminal => Some(Terminal::Failure(message.clone())),
        _ => None,
    }
}

fn required_tool_approval(event: &FromAgent) -> Option<String> {
    match event {
        FromAgent::ToolCall {
            tool,
            requires_approval: true,
            ..
        } => Some(format!(
            "workflow child tool `{tool}` requires approval, which delegated runs cannot request"
        )),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct ChildEventAccumulator {
    current_output: String,
    last_output: String,
    input_tokens: u64,
    output_tokens: u64,
    response_end_seen: bool,
    terminal_usage_known: bool,
    unknown_usage: bool,
}

impl ChildEventAccumulator {
    fn observe(&mut self, event: &FromAgent) {
        match event {
            FromAgent::ResponseChunk {
                content,
                is_thinking: false,
                ..
            } => self.current_output.push_str(content),
            FromAgent::ResponseEnd {
                response_id, usage, ..
            } => {
                // The command loop publishes a transport boundary after the
                // provider response (`done` or `continue`) with no usage.
                // It is not a second provider response and must not turn a
                // response whose usage was reported into an indeterminate
                // result.  A boundary without any real response still fails
                // the terminal-usage check below because response_end_seen
                // remains false.
                let lifecycle_boundary = matches!(response_id.as_str(), "done" | "continue");
                if !lifecycle_boundary {
                    self.response_end_seen = true;
                    match usage {
                        Some(usage) => {
                            self.input_tokens =
                                self.input_tokens.saturating_add(usage.input_tokens);
                            self.output_tokens =
                                self.output_tokens.saturating_add(usage.output_tokens);
                            self.terminal_usage_known = true;
                        }
                        None => {
                            self.unknown_usage = true;
                            self.terminal_usage_known = false;
                        }
                    }
                }
                if !lifecycle_boundary {
                    self.last_output = std::mem::take(&mut self.current_output);
                }
            }
            _ => {}
        }
    }

    fn merge(&mut self, other: Self) {
        // Shutdown may release a final response event that was still queued
        // when the terminal marker was observed.  Prefer that newer response
        // rather than concatenating it with an older response from the main
        // loop.
        if !other.current_output.is_empty() {
            self.current_output = other.current_output;
            if !other.last_output.is_empty() {
                self.last_output = other.last_output;
            }
        } else if !other.last_output.is_empty() {
            self.current_output.clear();
            self.last_output = other.last_output;
        }
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.response_end_seen |= other.response_end_seen;
        self.terminal_usage_known = other.terminal_usage_known || self.terminal_usage_known;
        self.unknown_usage |= other.unknown_usage;
    }

    fn finish(
        self,
        terminal: Terminal,
        credential_vault: &CredentialVault,
    ) -> Result<NativeChildResult> {
        if !self.response_end_seen || !self.terminal_usage_known || self.unknown_usage {
            bail!("native workflow child terminal usage is missing or indeterminate");
        }
        let mut output = if self.current_output.is_empty() {
            self.last_output
        } else {
            self.current_output
        };
        if let Terminal::Failure(reason) = &terminal {
            if output.is_empty() {
                output = reason.clone();
            }
        }
        Ok(NativeChildResult {
            output: credential_vault.vault_in_text(&output),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            success: matches!(terminal, Terminal::Success),
        })
    }
}

/// Owns a live child while the adapter is running.  If the caller's future is
/// dropped by a timeout, `Drop` still cancels the native runner and schedules
/// its asynchronous shutdown/drain on the current Tokio runtime.
struct ChildShutdownGuard {
    agent: Option<maestro_runtime::agent::NativeAgent>,
    events: Option<mpsc::UnboundedReceiver<FromAgent>>,
}

impl ChildShutdownGuard {
    fn new(
        agent: maestro_runtime::agent::NativeAgent,
        events: mpsc::UnboundedReceiver<FromAgent>,
    ) -> Self {
        Self {
            agent: Some(agent),
            events: Some(events),
        }
    }

    fn agent(&self) -> &maestro_runtime::agent::NativeAgent {
        self.agent
            .as_ref()
            .expect("native child shutdown guard has already been consumed")
    }

    fn events_mut(&mut self) -> &mut mpsc::UnboundedReceiver<FromAgent> {
        self.events
            .as_mut()
            .expect("native child shutdown guard has already been consumed")
    }

    /// Spawn cleanup as a separate task before awaiting it.  This preserves
    /// cleanup even when the adapter itself is dropped while shutdown waits on
    /// an active tool or relay.
    async fn shutdown_and_drain(&mut self) -> Result<ChildEventAccumulator> {
        let Some(agent) = self.agent.take() else {
            return Ok(ChildEventAccumulator::default());
        };
        let Some(events) = self.events.take() else {
            agent.shutdown().await;
            return Ok(ChildEventAccumulator::default());
        };
        let cleanup = tokio::spawn(async move {
            let mut events = events;
            agent.shutdown().await;
            let mut accumulator = ChildEventAccumulator::default();
            while let Some(event) = events.recv().await {
                accumulator.observe(&event);
            }
            accumulator
        });
        cleanup
            .await
            .context("native workflow child shutdown task failed")
    }
}

impl Drop for ChildShutdownGuard {
    fn drop(&mut self) {
        let Some(agent) = self.agent.take() else {
            return;
        };
        // Cancellation is synchronous and prevents additional work while the
        // detached cleanup task waits for the runner and relay to close.
        agent.cancel();
        let Some(events) = self.events.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // There is no executor on which an async lifecycle barrier can
            // run.  The native runner has still received cancellation.
            return;
        };
        drop(handle.spawn(async move {
            agent.shutdown().await;
            let mut events = events;
            while events.recv().await.is_some() {}
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_runtime_contracts::TokenUsage;
    use tempfile::tempdir;

    fn response_end(response_id: &str, input_tokens: u64, output_tokens: u64) -> FromAgent {
        FromAgent::ResponseEnd {
            response_id: response_id.to_owned(),
            usage: Some(TokenUsage {
                input_tokens,
                output_tokens,
                ..TokenUsage::default()
            }),
        }
    }

    #[test]
    fn empty_write_scopes_are_read_only() {
        let policy = sandbox_policy_for_scopes(Path::new("/workspace"), Vec::new());
        assert_eq!(policy, Some(SandboxPolicy::ReadOnly));
    }

    #[test]
    fn explicit_write_scopes_do_not_use_interactive_defaults() {
        let scope = PathBuf::from("/workspace/src");
        let policy = sandbox_policy_for_scopes(Path::new("/workspace"), vec![scope.clone()]);
        assert_eq!(
            policy,
            Some(SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![scope],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            })
        );
    }

    #[test]
    fn managed_sandbox_cannot_widen_explicit_roots_or_network() {
        let baseline = Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/workspace/src")],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        });
        let managed = Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/workspace/src"), PathBuf::from("/outside")],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        });

        assert_eq!(
            intersect_sandbox_policy(baseline, managed),
            Some(SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![PathBuf::from("/workspace/src")],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            })
        );
    }

    #[test]
    fn managed_sandbox_keeps_the_narrower_root_for_overlapping_grants() {
        let baseline = vec![PathBuf::from("/repo/src"), PathBuf::from("/repo")];
        let managed = vec![PathBuf::from("/repo"), PathBuf::from("/repo/src/generated")];
        assert_eq!(
            intersect_writable_roots(&baseline, &managed),
            vec![
                PathBuf::from("/repo/src"),
                PathBuf::from("/repo/src/generated"),
                PathBuf::from("/repo"),
            ]
        );
    }

    #[test]
    fn disjoint_managed_roots_are_excluded() {
        assert!(
            intersect_writable_roots(&[PathBuf::from("/repo/src")], &[PathBuf::from("/other")])
                .is_empty()
        );
    }

    #[test]
    fn read_only_baseline_wins_over_managed_write_policy() {
        let managed = Some(SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/workspace")],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        });
        assert_eq!(
            intersect_sandbox_policy(Some(SandboxPolicy::ReadOnly), managed),
            Some(SandboxPolicy::ReadOnly)
        );
    }

    #[test]
    fn empty_allowed_tools_remains_empty() {
        assert!(allowed_tool_set(&[]).expect("empty tool grant").is_empty());
    }

    #[test]
    fn usage_is_aggregated_and_missing_usage_is_rejected() {
        let vault = CredentialVault::new();
        let mut accumulator = ChildEventAccumulator::default();
        accumulator.observe(&FromAgent::ResponseChunk {
            response_id: "response-1".to_owned(),
            content: "result".to_owned(),
            is_thinking: false,
        });
        accumulator.observe(&response_end("response-1", 3, 5));
        accumulator.observe(&FromAgent::ResponseChunk {
            response_id: "response-2".to_owned(),
            content: "final".to_owned(),
            is_thinking: false,
        });
        accumulator.observe(&response_end("response-2", 7, 11));
        accumulator.observe(&FromAgent::ResponseEnd {
            response_id: "done".to_owned(),
            usage: None,
        });
        let result = accumulator
            .finish(Terminal::Success, &vault)
            .expect("known usage");
        assert_eq!(
            result,
            NativeChildResult {
                output: "final".to_owned(),
                input_tokens: 10,
                output_tokens: 16,
                success: true,
            }
        );

        let mut missing = ChildEventAccumulator::default();
        missing.observe(&FromAgent::ResponseEnd {
            response_id: "response-1".to_owned(),
            usage: None,
        });
        assert!(missing.finish(Terminal::Success, &vault).is_err());
    }

    #[test]
    fn explicit_failed_terminal_preserves_failure_when_usage_is_known() {
        let vault = CredentialVault::new();
        let mut accumulator = ChildEventAccumulator::default();
        accumulator.observe(&FromAgent::ResponseChunk {
            response_id: "response-1".to_owned(),
            content: "partial".to_owned(),
            is_thinking: false,
        });
        accumulator.observe(&response_end("response-1", 2, 4));
        let result = accumulator
            .finish(Terminal::Failure("provider failed".to_owned()), &vault)
            .expect("known failure usage");
        assert!(!result.success);
        assert_eq!(result.output, "partial");
        assert_eq!(result.input_tokens, 2);
        assert_eq!(result.output_tokens, 4);
    }

    #[test]
    fn relative_scope_is_resolved_inside_working_directory() {
        let workspace = tempdir().expect("temporary workspace");
        let scope = workspace.path().join("src");
        std::fs::create_dir(&scope).expect("scope directory");
        let (cwd, scopes) = validate_paths(workspace.path(), &[PathBuf::from("src")])
            .expect("valid relative scope");
        assert_eq!(
            cwd,
            dunce::canonicalize(workspace.path()).expect("canonical cwd")
        );
        assert_eq!(
            scopes,
            vec![dunce::canonicalize(scope).expect("canonical scope")]
        );
    }
}
