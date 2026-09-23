//! Non-interactive print mode (Grok-style `--print` / single-shot / `exec`).
//!
//! Runs the native agent without a TUI, auto-approves tools, prints the
//! assistant response, and exits. Supports `--output-last-message` and a
//! lightweight JSON Schema check via `--output-schema`.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent::{
    CredentialVault, ExecutionSource, ExternalToolSchemaPolicy, FromAgent, MaxTokensSource,
    NativeAgent, NativeAgentConfig,
};
use crate::safety::FirewallVerdict;
use crate::sandbox::SandboxPolicy;
use crate::state::ApprovalMode;
use crate::tools::ToolExecutor;

/// Options for print / exec-style runs.
#[derive(Debug, Clone)]
pub struct PrintModeOptions {
    /// Named specialist profile, resolved once before the run.
    pub specialist: Option<String>,
    pub prompt: String,
    /// Emit simple JSONL events instead of plain text.
    pub json: bool,
    /// Model override (or from `MAESTRO_MODEL` / default).
    pub model: Option<String>,
    /// Write final assistant text to this path (exec parity).
    pub output_last_message: Option<PathBuf>,
    /// JSON Schema path or inline JSON object (required keys + type checks).
    pub output_schema: Option<String>,
    /// Native sandbox policy for tool subprocesses.
    pub sandbox_policy: Option<SandboxPolicy>,
    /// Reject tool calls that would require interactive approval.
    pub fail_on_approval: bool,
    /// Restrict this run to these tools before applying narrower policy ceilings.
    pub only_tools: Vec<String>,
    /// Add tools to the run default without widening an administrator ceiling.
    pub add_tools: Vec<String>,
    /// Remove tools from the effective run catalog.
    pub remove_tools: Vec<String>,
    /// Searchable run metadata emitted in structured output and telemetry.
    pub tags: Vec<serde_json::Value>,
    /// Correlation key for related CI, release, or batch runs.
    pub log_group_id: Option<String>,
}

fn approval_denied(
    executor: &ToolExecutor,
    tool: &str,
    args: &serde_json::Value,
    fail_on_approval: bool,
) -> bool {
    fail_on_approval
        && (executor.requires_approval(tool, args)
            || matches!(
                executor.firewall_verdict(tool, args),
                FirewallVerdict::RequireApproval { .. }
            ))
}

/// Print mode owns its tool limits, workspace policy, and execution. Native
/// must therefore defer every tool call to this event loop so each call is
/// executed exactly once through the print-mode executor.
fn print_mode_approval_mode() -> ApprovalMode {
    ApprovalMode::Safe
}

fn typed_terminal_exit_code(event: &FromAgent) -> Option<i32> {
    match event {
        FromAgent::TurnCompleted { .. } => Some(0),
        FromAgent::TurnInterrupted { .. } | FromAgent::ProviderError { .. } => Some(1),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct PrintModeLimits {
    max_tokens: u32,
    max_tokens_source: MaxTokensSource,
    max_tool_calls: usize,
    max_turns: usize,
    workspace_only_file_tools: bool,
    allowed_tools: Option<HashSet<String>>,
}

impl PrintModeLimits {
    fn from_env(model: &str) -> Result<Self> {
        let (max_tokens, max_tokens_is_explicit) = positive_env_with_presence(
            "MAESTRO_PRINT_MAX_TOKENS",
            crate::model_catalog::default_max_output_tokens(model),
        )?;
        Ok(Self {
            // Explicit env override wins; otherwise catalog-known models get
            // their full output ceiling and unknown models the fallback.
            max_tokens,
            max_tokens_source: if max_tokens_is_explicit {
                MaxTokensSource::Explicit
            } else {
                MaxTokensSource::Catalog
            },
            max_tool_calls: positive_env("MAESTRO_PRINT_MAX_TOOL_CALLS", usize::MAX)?,
            // Unbounded was the old default and it had no terminator: a
            // model that keeps calling tools never ends the turn. The
            // shared per-turn step budget is the floor; the env var still
            // raises or lowers it.
            max_turns: positive_env(
                "MAESTRO_PRINT_MAX_TURNS",
                crate::agent::DEFAULT_MAX_TURN_STEPS,
            )?,
            workspace_only_file_tools: bool_env("MAESTRO_PRINT_WORKSPACE_ONLY_FILE_TOOLS")?,
            allowed_tools: allowed_tools_from_env()?,
        })
    }
}

fn positive_env<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr + PartialOrd + From<u8> + Copy,
    T::Err: std::fmt::Display,
{
    positive_env_with_presence(name, default).map(|(value, _)| value)
}

fn positive_env_with_presence<T>(name: &str, default: T) -> Result<(T, bool)>
where
    T: std::str::FromStr + PartialOrd + From<u8> + Copy,
    T::Err: std::fmt::Display,
{
    let Ok(raw) = std::env::var(name) else {
        return Ok((default, false));
    };
    let value = raw
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("{name} must be a positive integer: {error}"))?;
    if value < T::from(1) {
        bail!("{name} must be a positive integer");
    }
    Ok((value, true))
}

fn bool_env(name: &str) -> Result<bool> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(err) => Err(err.into()),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" | "" => Ok(false),
            _ => bail!("{name} must be a boolean"),
        },
    }
}

fn allowed_tools_from_env() -> Result<Option<HashSet<String>>> {
    let Ok(raw) = std::env::var("MAESTRO_PRINT_ALLOWED_TOOLS") else {
        return Ok(None);
    };
    let tools = raw
        .split(',')
        .map(|tool| tool.trim().to_ascii_lowercase())
        .filter(|tool| !tool.is_empty())
        .collect::<HashSet<_>>();
    if tools.is_empty() {
        bail!("MAESTRO_PRINT_ALLOWED_TOOLS must list at least one tool");
    }
    Ok(Some(tools))
}

fn normalized_tool_names(values: &[String]) -> HashSet<String> {
    values
        .iter()
        .flat_map(|value| value.split([',', ' ']))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn available_tool_names() -> HashSet<String> {
    crate::tools::ToolRegistry::new()
        .tools()
        .map(|definition| definition.tool.name.to_ascii_lowercase())
        .collect()
}

fn effective_run_tools(
    environment_ceiling: Option<HashSet<String>>,
    only: &[String],
    add: &[String],
    remove: &[String],
) -> Result<Option<HashSet<String>>> {
    let available = available_tool_names();
    let only = normalized_tool_names(only);
    let add = normalized_tool_names(add);
    let remove = normalized_tool_names(remove);
    let requested = only
        .iter()
        .chain(add.iter())
        .chain(remove.iter())
        .filter(|tool| !available.contains(*tool))
        .cloned()
        .collect::<Vec<_>>();
    if !requested.is_empty() {
        bail!(
            "Unknown print-run tool(s): {}. Run `deixic-code exec --list-tools`.",
            requested.join(", ")
        );
    }

    let has_cli_selection = !only.is_empty() || !add.is_empty() || !remove.is_empty();
    if !has_cli_selection {
        return Ok(environment_ceiling);
    }

    let ceiling = environment_ceiling.unwrap_or_else(|| available.clone());
    let mut effective = if only.is_empty() {
        ceiling.clone()
    } else {
        only.intersection(&ceiling).cloned().collect()
    };
    // Additions are still bounded by the authoritative environment ceiling.
    effective.extend(add.intersection(&ceiling).cloned());
    effective.retain(|tool| !remove.contains(tool));
    Ok(Some(effective))
}

pub(crate) fn print_available_tools(json: bool) -> Result<()> {
    let registry = crate::tools::ToolRegistry::new();
    let mut tools = registry
        .tools()
        .map(|definition| {
            serde_json::json!({
                "name": definition.tool.name,
                "requiresApproval": definition.requires_approval,
            })
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"tools": tools}))?
        );
    } else {
        for tool in tools {
            println!(
                "{:<28} {}",
                tool["name"].as_str().unwrap_or_default(),
                if tool["requiresApproval"].as_bool().unwrap_or(false) {
                    "approval may be required"
                } else {
                    "available"
                }
            );
        }
    }
    Ok(())
}

pub(crate) fn canonical_workspace_path(workspace: &Path, input: &str) -> Result<PathBuf> {
    let canonical_workspace = dunce::canonicalize(workspace)
        .with_context(|| format!("canonicalize workspace {}", workspace.display()))?;
    let candidate = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        workspace.join(input)
    };
    let canonical = dunce::canonicalize(&candidate)
        .with_context(|| format!("canonicalize tool path {}", candidate.display()))?;
    if !canonical.starts_with(&canonical_workspace) {
        bail!(
            "Tool path `{}` resolves outside workspace `{}`",
            input,
            canonical_workspace.display()
        );
    }
    Ok(canonical)
}

fn canonical_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path.to_path_buf();
    while !ancestor.exists() {
        if !ancestor.pop() {
            bail!("Tool path has no existing ancestor: {}", path.display());
        }
    }
    dunce::canonicalize(&ancestor)
        .with_context(|| format!("canonicalize tool path ancestor {}", ancestor.display()))
}

pub(crate) fn prepare_workspace_tool_args(
    tool: &str,
    args: &serde_json::Value,
    workspace: &Path,
) -> Result<serde_json::Value> {
    let mut prepared = args.clone();
    match tool.to_ascii_lowercase().as_str() {
        "read" => {
            let input = args
                .get("path")
                .or_else(|| args.get("file_path"))
                .and_then(serde_json::Value::as_str)
                .context("read tool requires a path")?;
            let canonical = canonical_workspace_path(workspace, input)?;
            prepared["path"] = serde_json::Value::String(canonical.display().to_string());
            if let Some(object) = prepared.as_object_mut() {
                object.remove("file_path");
            }
        }
        "glob" => {
            let pattern = args
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .context("glob tool requires a pattern")?;
            let pattern_path = Path::new(pattern);
            if pattern_path.is_absolute()
                || pattern_path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                bail!("Glob pattern must stay relative to its workspace base path");
            }
            let base_input = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(".");
            let base = canonical_workspace_path(workspace, base_input)?;
            let components = pattern_path.components().collect::<Vec<_>>();
            let first_magic = components.iter().position(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .chars()
                    .any(|character| matches!(character, '*' | '?' | '[' | '{'))
            });
            if first_magic.is_some_and(|index| index + 1 < components.len()) {
                bail!("Workspace-only glob patterns cannot wildcard directories");
            }
            let fixed_prefix = components
                .iter()
                .take(first_magic.unwrap_or(components.len()))
                .copied()
                .collect::<PathBuf>();
            let prefix = canonical_existing_ancestor(&base.join(fixed_prefix))?;
            if !prefix.starts_with(workspace) {
                bail!("Glob pattern resolves through a symlink outside the workspace");
            }
            prepared["path"] = serde_json::Value::String(base.display().to_string());
        }
        _ => {}
    }
    Ok(prepared)
}

/// Sanitize a raw provider stream delta before it reaches the real terminal
/// in the non-`--json` `--print` output path, which has no ratatui `Buffer`
/// to filter it (unlike the TUI chat pane).
fn sanitize_stream_chunk(content: &str) -> String {
    crate::output_sanitize::sanitize_control_chars(content)
}

/// Decide whether a `--print` stream delta's stdout write should sanitize
/// `content`.
///
/// Redirected output (piped to a file or another process) cannot execute a
/// terminal escape sequence and must stay byte-exact -- it is also what
/// `--output-last-message` saves from and, unlike this path, never
/// sanitizes, so unconditionally sanitizing here would make plain
/// redirected `--print` stdout diverge from that saved copy for the same
/// run. Only sanitize when stdout is an actual terminal. Takes
/// `stdout_is_terminal` as a parameter (mirroring
/// `hyperlink::format_link_with_fallback`'s `is_tty` parameter) so this
/// stays unit-testable without a real pty.
fn stream_chunk_for_stdout(content: &str, stdout_is_terminal: bool) -> String {
    if stdout_is_terminal {
        sanitize_stream_chunk(content)
    } else {
        content.to_string()
    }
}

fn validate_print_local_model(
    route: &str,
    discovered: &crate::model_catalog::ModelInfo,
) -> Result<()> {
    // Ollama lists installed models through /v1/models, but only lists loaded
    // models (and their live context allocation) through /api/ps. An idle
    // model must be allowed to reach the chat request that loads it.
    if discovered.capabilities.context_tokens == 0 && discovered.provider != "ollama" {
        bail!(
            "Local model {route} did not report its live context limit; configure the runtime to expose context metadata before print/exec"
        );
    }
    Ok(())
}

/// Establish per-run managed request lineage before the first print prompt.
/// This identity does not claim ownership of a persisted transcript or spills.
pub(crate) async fn start_print_prompt(
    agent: &NativeAgent,
    run_id: &str,
    prompt: String,
) -> Result<()> {
    agent
        .set_session_context(Some(run_id.to_owned()), "print", false)
        .context("Failed to establish print session context")?;
    agent.send_ready();
    agent
        .prompt(prompt, vec![])
        .await
        .context("Failed to send prompt")
}

fn response_usage_event(
    response_id: &str,
    usage: &Option<crate::agent::TokenUsage>,
) -> Option<serde_json::Value> {
    (response_id != "done").then(|| {
        serde_json::json!({
            "type": "item", "subtype": "response_usage",
            "response_id": response_id, "usage": usage,
        })
    })
}

// This changes model-visible text only; canonical cwd still governs every file tool.
fn print_system_prompt(cwd: &str, stable: bool) -> String {
    if stable {
        "You are Deixic Code, an AI coding assistant. File paths are relative to the tool workspace. Be concise and use tools when helpful.".to_string()
    } else {
        format!(
            "You are Deixic Code, an AI coding assistant. Working directory: {cwd}. Be concise and use tools when helpful."
        )
    }
}

/// Intersect specialist tools with the caller's existing ceiling.
fn specialist_tool_ceiling(existing: Option<HashSet<String>>, tools: &[String]) -> HashSet<String> {
    let tools = tools
        .iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .collect();
    match existing {
        Some(allowed) => allowed.intersection(&tools).cloned().collect(),
        None => tools,
    }
}

/// Run one prompt non-interactively and print the final answer.
pub async fn run_print_mode(options: PrintModeOptions) -> Result<i32> {
    let workspace = dunce::canonicalize(
        &std::env::current_dir().context("resolve print-mode working directory")?,
    )
    .context("canonicalize print-mode working directory")?;
    let specialist = options
        .specialist
        .as_deref()
        .map(|name| crate::agents_cli::resolve_specialist(name, &workspace))
        .transpose()?;
    let model = options
        .model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| specialist.as_ref().and_then(|p| p.model.clone()))
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(crate::codex_auth::resolve_default_model);
    // A signed exact route is administrator-supplied metadata, not a discovered
    // user override; do not probe ordinary local-runtime environment URLs.
    let disconnected = maestro_local_host::safety::disconnected_route(&model)
        .map_err(anyhow::Error::msg)?
        .is_some();
    if crate::local_models::is_local_model_route(&model) && !disconnected {
        let discovered = crate::local_models::discover_local_model(&model)
            .await?
            .with_context(|| {
                format!(
                    "Local model {model} was not reported by its runtime; verify the runtime and /v1/models endpoint"
                )
            })?;
        validate_print_local_model(&model, &discovered)?;
        crate::local_models::replace_discovered_models(0, &[discovered], Some(&model));
    }
    let mut limits = PrintModeLimits::from_env(&model)?;
    limits.allowed_tools = effective_run_tools(
        limits.allowed_tools.take(),
        &options.only_tools,
        &options.add_tools,
        &options.remove_tools,
    )?;
    if let Some(tools) = specialist.as_ref().and_then(|p| p.tools.as_ref()) {
        limits.allowed_tools = Some(specialist_tool_ceiling(limits.allowed_tools.take(), tools));
    }

    let cwd = workspace.to_string_lossy().to_string();

    let mut system_prompt = print_system_prompt(
        &cwd,
        std::env::var("MAESTRO_PRINT_STABLE_SYSTEM_PROMPT").as_deref() == Ok("true"),
    );

    if let Some(specialist) = &specialist {
        system_prompt.push_str("\n\nSpecialist focus:\n");
        system_prompt.push_str(&specialist.prompt);
    }
    let (thinking_enabled, thinking_budget) = specialist
        .as_ref()
        .and_then(|p| p.thinking)
        .map(|level| level.to_config())
        .unwrap_or((false, 0));
    let config = NativeAgentConfig {
        model_capabilities: None,
        model_dynamics: crate::config::model_dynamics_config(),
        model: model.clone(),
        max_tokens: limits.max_tokens,
        max_tokens_source: limits.max_tokens_source,
        system_prompt: Some(system_prompt),
        thinking_enabled,
        thinking_budget,
        cwd: cwd.clone(),
        // Print mode owns the limits, workspace policy, and executor below.
        // Defer every call to this event loop so native cannot auto-execute a
        // selective-safe call before this mode sees the ToolCall event.
        approval_mode: print_mode_approval_mode(),
        context_window: None,
        // The native agent runner's own tool executor -- which runs every
        // call the per-tool heuristic above doesn't flag for approval --
        // is a separate executor from the sandboxed one constructed below
        // for this function's own bypass-approval check. Without this, only
        // that local check was sandbox-aware; the actual execution wasn't
        // (review finding on #3144).
        sandbox_policy: options.sandbox_policy.clone(),
        managed_mcp_policy: None,
        external_tool_schema_policy: ExternalToolSchemaPolicy::Eager,
        // The print run's own `MAESTRO_PRINT_MAX_TURNS` bound is the same
        // bound the turn loop enforces; feeding it here makes the loop stop
        // and report at the budget instead of running past it until this
        // event loop notices and cancels.
        max_turn_steps: limits.max_turns,
        allow_unbounded_turn: false,
        retry_config: crate::agent::retry::RetryConfig::default(),
    };

    let mut parent_choice = crate::model_dynamics::ModelChoice {
        model: config.model.clone(),
        thinking: crate::model_dynamics::thinking_level(
            config.thinking_enabled,
            config.thinking_budget,
        ),
    };
    if let Some(specialist) = &specialist {
        use sha2::{Digest, Sha256};
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(specialist)?));
        if options.json {
            println!(
                "{}",
                serde_json::json!({"type":"thread", "subtype":"specialist", "name":specialist.name, "scope":specialist.scope, "digest":digest})
            );
        } else {
            eprintln!("Specialist: {} ({digest})", specialist.name);
        }
    }
    let credential_vault = CredentialVault::new();
    let (agent, mut event_rx) = match &limits.allowed_tools {
        Some(allowed_tools) => NativeAgent::new_with_allowed_tools_and_credential_vault(
            config,
            allowed_tools,
            credential_vault.clone(),
        ),
        None => NativeAgent::new_with_credential_vault(config, credential_vault.clone()),
    }
    .context("Failed to create native agent for print mode")?;
    let tool_tx = agent.tool_response_sender();
    // Print/exec mode has no approval UI (see the `bypass_sandbox` rejection
    // below), so tools fail closed instead of escalating to a user who is not
    // there.
    let tool_executor = match options.sandbox_policy.clone() {
        Some(policy) => ToolExecutor::with_credential_vault(&cwd, credential_vault.clone())
            .unattended()
            .with_sandbox_policy(policy),
        None => ToolExecutor::with_credential_vault(&cwd, credential_vault.clone()).unattended(),
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let mut effective_tools = limits
        .allowed_tools
        .as_ref()
        .map(|tools| tools.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_else(|| available_tool_names().into_iter().collect());
    effective_tools.sort();
    if options.json {
        println!(
            "{}",
            serde_json::json!({
                "type": "run_started",
                "run_id": run_id,
                "tags": options.tags,
                "log_group_id": options.log_group_id,
                "effective_tools": effective_tools,
            })
        );
    }
    tracing::info!(
        run_id = %run_id,
        log_group_id = options.log_group_id.as_deref().unwrap_or(""),
        tags = %serde_json::Value::Array(options.tags.clone()),
        "print run started"
    );
    start_print_prompt(&agent, &run_id, options.prompt).await?;

    let mut exit_code = 0i32;
    let mut assistant_buf = String::new();
    let mut last_assistant_message = String::new();
    let mut tool_calls = 0usize;
    let mut turns = 0usize;

    loop {
        let Some(msg) = event_rx.recv().await else {
            break;
        };
        let typed_terminal_exit_code = typed_terminal_exit_code(&msg);

        match &msg {
            FromAgent::ModelChanged { model, .. } => parent_choice.model.clone_from(model),
            FromAgent::BoostChanged {
                thinking: Some(thinking),
                ..
            } => parent_choice.thinking = *thinking,
            _ => {}
        }
        tool_executor.set_subagent_parent_model(parent_choice.clone());
        match msg {
            FromAgent::ResponseChunk {
                content,
                is_thinking,
                ..
            } => {
                if is_thinking {
                    continue;
                }
                if options.json {
                    let line = serde_json::json!({
                        "type": "item",
                        "subtype": "message_delta",
                        "text": content,
                    });
                    println!("{line}");
                } else {
                    // `content` is a raw provider stream delta with no
                    // ratatui `Buffer` in this loop to filter it, unlike the
                    // TUI chat pane. Sanitize at this print boundary rather
                    // than upstream: the same delta also flows to the
                    // `--json` branch above, where `serde_json` already
                    // escapes control characters, so filtering earlier would
                    // be redundant there. Sanitizing is further gated on
                    // stdout actually being a terminal: see
                    // `stream_chunk_for_stdout`'s doc comment.
                    print!(
                        "{}",
                        stream_chunk_for_stdout(&content, std::io::stdout().is_terminal())
                    );
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                assistant_buf.push_str(&content);
            }
            FromAgent::ToolCall {
                call_id,
                tool,
                args,
                ..
            } => {
                tool_calls += 1;
                let normalized_tool = tool.to_ascii_lowercase();
                let limit_error = if limits
                    .allowed_tools
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(&normalized_tool))
                {
                    Some(format!("Tool `{tool}` is not allowed in this print run"))
                } else if tool_calls > limits.max_tool_calls {
                    Some(format!(
                        "Print run exceeded MAESTRO_PRINT_MAX_TOOL_CALLS ({})",
                        limits.max_tool_calls
                    ))
                } else if turns >= limits.max_turns {
                    Some(format!(
                        "Tool `{tool}` would exceed MAESTRO_PRINT_MAX_TURNS ({})",
                        limits.max_turns
                    ))
                } else {
                    None
                };

                if options.json {
                    let line = serde_json::json!({
                        "type": "item",
                        "subtype": "tool_call",
                        "call_id": call_id,
                        "tool": tool,
                        "args": args,
                    });
                    println!("{line}");
                } else {
                    eprintln!("[tool] {tool}");
                }

                // Model-authored references are never authority to reveal a
                // credential to a tool. Connection transports inject their own
                // secrets after admission.
                let mut execution_args = args.clone();
                let workspace_error = if limits.workspace_only_file_tools {
                    match prepare_workspace_tool_args(&tool, &execution_args, &workspace) {
                        Ok(prepared) => {
                            execution_args = prepared;
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    }
                } else {
                    None
                };
                let denied = approval_denied(
                    &tool_executor,
                    &tool,
                    &execution_args,
                    options.fail_on_approval,
                );
                let rejection = limit_error.or(workspace_error).or_else(|| {
                    // A `bypass_sandbox` request can only ever run after a
                    // human explicitly approves it, and print/exec has no
                    // approval UI to collect that approval — so it is
                    // rejected here rather than silently honored (which would
                    // let the model waive the sandbox at will).
                    if tool_executor.requires_sandbox_bypass_approval(&tool, &execution_args) {
                        Some(format!(
                            "Tool `{tool}` requested `bypass_sandbox: true`, but running \
                             outside the native sandbox requires human approval and this \
                             non-interactive run cannot collect it. Retry without \
                             `bypass_sandbox`."
                        ))
                    } else {
                        denied.then(|| {
                            format!("Tool `{tool}` requires approval, but approval mode is fail")
                        })
                    }
                });
                let result = if let Some(message) = &rejection {
                    crate::agent::ToolResult::failure(message)
                } else if denied {
                    crate::agent::ToolResult::failure(format!(
                        "Tool `{tool}` requires approval, but approval mode is fail"
                    ))
                } else {
                    tool_executor
                        .execute(&tool, &execution_args, None, &call_id)
                        .await
                };

                if options.json {
                    let line = serde_json::json!({
                        "type": "item",
                        "subtype": "tool_result",
                        "call_id": call_id,
                        "tool": tool,
                        "success": result.success,
                        "output": result.output,
                    });
                    println!("{line}");
                }

                let approved = rejection.is_none() && !denied;
                let _ = tool_tx.send((
                    call_id,
                    approved,
                    Some(result),
                    ExecutionSource::Native,
                    None,
                ));
                if rejection.is_some() {
                    exit_code = 1;
                    agent.cancel();
                    break;
                }
            }
            FromAgent::ResponseEnd { response_id, usage } => {
                if options.json {
                    if let Some(event) = response_usage_event(&response_id, &usage) {
                        println!("{event}");
                    }
                }

                if response_id != "done" {
                    turns += 1;
                    if turns > limits.max_turns {
                        if options.json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "type": "error",
                                    "message": format!(
                                        "Print run exceeded MAESTRO_PRINT_MAX_TURNS ({})",
                                        limits.max_turns
                                    ),
                                    "fatal": true,
                                })
                            );
                        }
                        exit_code = 1;
                        agent.cancel();
                        break;
                    }
                }
                if options.json && !assistant_buf.is_empty() {
                    let line = serde_json::json!({
                        "type": "item",
                        "subtype": "message_complete",
                        "text": assistant_buf,
                        "usage": usage,
                    });
                    println!("{line}");
                } else if !options.json
                    && !assistant_buf.is_empty()
                    && !assistant_buf.ends_with('\n')
                {
                    println!();
                }

                record_completed_response(
                    &response_id,
                    &mut assistant_buf,
                    &mut last_assistant_message,
                );
            }
            FromAgent::TurnCompleted { .. } => {
                exit_code = typed_terminal_exit_code.expect("typed completion exit code");
                if options.json {
                    let done = serde_json::json!({
                        "type": "done",
                        "status": "ok",
                        "run_id": run_id,
                        "tags": options.tags,
                        "log_group_id": options.log_group_id,
                    });
                    println!("{done}");
                }
                break;
            }
            FromAgent::TurnInterrupted { reason, .. } => {
                exit_code = typed_terminal_exit_code.expect("typed interruption exit code");
                if options.json {
                    println!(
                        "{}",
                        serde_json::json!({"type":"error","message":reason,"fatal":false})
                    );
                } else {
                    eprintln!("Error: {reason}");
                }
                break;
            }
            FromAgent::Error {
                message,
                fatal,
                terminal,
                ..
            } => {
                if options.json {
                    let line = serde_json::json!({
                        "type": "error",
                        "message": message,
                        "fatal": fatal,
                    });
                    println!("{line}");
                } else {
                    eprintln!("Error: {message}");
                }
                if fatal || terminal {
                    exit_code = 1;
                    break;
                }
            }
            FromAgent::ProviderError { kind, message } => {
                exit_code = typed_terminal_exit_code.expect("typed provider exit code");
                if options.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "provider_error",
                            "kind": kind,
                            "message": message,
                            "fatal": false,
                        })
                    );
                } else {
                    eprintln!("Provider error ({kind:?}): {message}");
                }
                break;
            }
            _ => {}
        }
    }

    if exit_code == 0 {
        if let Some(schema_src) = &options.output_schema {
            if let Err(err) = validate_against_schema(&last_assistant_message, schema_src) {
                if options.json {
                    let line = serde_json::json!({
                        "type": "error",
                        "message": err.to_string(),
                        "fatal": true,
                    });
                    println!("{line}");
                } else {
                    eprintln!("Schema validation failed: {err:#}");
                }
                exit_code = 1;
            }
        }
    }

    if exit_code == 0 {
        if let Some(path) = &options.output_last_message {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create dir for {}", path.display()))?;
                }
            }
            std::fs::write(path, &last_assistant_message)
                .with_context(|| format!("write output-last-message to {}", path.display()))?;
            if !options.json {
                eprintln!("Wrote last message to {}", path.display());
            }
        }
    }

    Ok(exit_code)
}

/// Record a completed model response. A model-call boundary never completes
/// the enclosing agent turn; only `FromAgent::TurnCompleted` does that.
fn record_completed_response(response_id: &str, current: &mut String, last_completed: &mut String) {
    if response_id == "done" {
        return;
    }
    if !current.is_empty() {
        last_completed.clone_from(current);
        current.clear();
    }
}

/// Process multiple prompts sequentially (exec multi-prompt).
pub async fn run_print_prompts(
    prompts: Vec<String>,
    json: bool,
    model: Option<String>,
    output_last_message: Option<PathBuf>,
    output_schema: Option<String>,
) -> Result<i32> {
    let mut code = 0;
    let last = prompts.len().saturating_sub(1);
    for (i, prompt) in prompts.into_iter().enumerate() {
        if i > 0 && !json {
            println!("\n---\n");
        }
        // Only attach file/schema capture on the final prompt (exec parity).
        let result = run_print_mode(PrintModeOptions {
            specialist: None,
            prompt,
            json,
            model: model.clone(),
            output_last_message: if i == last {
                output_last_message.clone()
            } else {
                None
            },
            output_schema: if i == last {
                output_schema.clone()
            } else {
                None
            },
            sandbox_policy: None,
            fail_on_approval: false,
            only_tools: Vec::new(),
            add_tools: Vec::new(),
            remove_tools: Vec::new(),
            tags: Vec::new(),
            log_group_id: None,
        })
        .await?;
        if result != 0 {
            code = result;
            break;
        }
    }
    Ok(code)
}

/// Lightweight JSON Schema subset check (type + required + property types).
/// Full draft validation is not required for killing the TS exec path.
fn validate_against_schema(text: &str, schema_source: &str) -> Result<()> {
    let (schema, label) = load_schema(schema_source)?;
    let parsed: serde_json::Value = serde_json::from_str(text.trim())
        .with_context(|| format!("Assistant output is not valid JSON for schema {label}"))?;
    check_value(&parsed, &schema, "$").with_context(|| format!("schema {label}"))?;
    Ok(())
}

fn load_schema(source: &str) -> Result<(serde_json::Value, String)> {
    let trimmed = source.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let schema: serde_json::Value =
            serde_json::from_str(trimmed).context("parse inline JSON schema")?;
        return Ok((schema, "inline".to_string()));
    }
    let path = PathBuf::from(trimmed);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let raw = std::fs::read_to_string(&absolute)
        .with_context(|| format!("Schema file not found: {}", absolute.display()))?;
    let schema: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse schema {}", absolute.display()))?;
    Ok((schema, absolute.display().to_string()))
}

fn check_value(value: &serde_json::Value, schema: &serde_json::Value, path: &str) -> Result<()> {
    if let Some(types) = schema.get("type") {
        if !type_matches(value, types) {
            bail!("{path} has wrong type (expected {types}, got {value})");
        }
    }

    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        let obj = value
            .as_object()
            .with_context(|| format!("{path} is not an object but schema requires keys"))?;
        for key in required {
            let Some(name) = key.as_str() else {
                continue;
            };
            if !obj.contains_key(name) {
                bail!("{path} missing required property `{name}`");
            }
        }
    }

    if let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(|p| p.as_object()),
        value.as_object(),
    ) {
        for (key, prop_schema) in props {
            if let Some(child) = obj.get(key) {
                check_value(child, prop_schema, &format!("{path}.{key}"))?;
            }
        }
    }

    if let (Some(item_schema), Some(arr)) = (schema.get("items"), value.as_array()) {
        for (i, item) in arr.iter().enumerate() {
            check_value(item, item_schema, &format!("{path}[{i}]"))?;
        }
    }

    if let Some(enum_vals) = schema.get("enum").and_then(|e| e.as_array()) {
        if !enum_vals.iter().any(|v| v == value) {
            bail!("{path} value not in enum");
        }
    }

    Ok(())
}

fn type_matches(value: &serde_json::Value, types: &serde_json::Value) -> bool {
    let check_one = |t: &str| -> bool {
        match t {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        }
    };
    if let Some(t) = types.as_str() {
        return check_one(t);
    }
    if let Some(arr) = types.as_array() {
        return arr.iter().filter_map(|v| v.as_str()).any(check_one);
    }
    true
}

/// Resolve a relative path against cwd.
#[allow(dead_code)]
pub fn resolve_output_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn run_tool_selection_cannot_widen_environment_ceiling() {
        let ceiling = Some(["read".to_string()].into_iter().collect());
        let effective = super::effective_run_tools(ceiling, &[], &["read,bash".to_string()], &[])
            .expect("valid tool selection")
            .expect("explicit selection");
        assert_eq!(effective, ["read".to_string()].into_iter().collect());
    }

    #[test]
    fn run_tool_selection_rejects_unknown_names() {
        let error = super::effective_run_tools(None, &["missing-tool-9f64".to_string()], &[], &[])
            .expect_err("unknown tools must fail closed");
        assert!(error.to_string().contains("missing-tool-9f64"));
    }

    #[test]
    fn specialist_tools_only_narrow_the_run_ceiling() {
        let allowed = Some(["read".to_string()].into_iter().collect());
        let narrowed = super::specialist_tool_ceiling(allowed, &["read".into(), "bash".into()]);
        assert_eq!(narrowed, ["read".to_string()].into_iter().collect());
        assert!(super::specialist_tool_ceiling(Some(narrowed), &[]).is_empty());
    }

    #[tokio::test]
    async fn unknown_specialist_fails_before_provider_access() {
        let result = super::run_print_mode(super::PrintModeOptions {
            specialist: Some("missing-specialist-8335".into()),
            prompt: "review".into(),
            json: false,
            model: None,
            output_last_message: None,
            output_schema: None,
            sandbox_policy: None,
            fail_on_approval: true,
            only_tools: Vec::new(),
            add_tools: Vec::new(),
            remove_tools: Vec::new(),
            tags: Vec::new(),
            log_group_id: None,
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("authorized scope"));
    }

    #[test]
    fn response_usage_is_independent_of_assistant_text() {
        let usage = Some(crate::agent::TokenUsage {
            input_tokens: 100,
            cost: Some(0.012),
            ..Default::default()
        });
        let event = super::response_usage_event("tool-only", &usage).unwrap();
        assert_eq!(event["usage"]["input_tokens"], 100);
        assert_eq!(event["usage"]["cost"], 0.012);
        assert!(super::response_usage_event("done", &usage).is_none());
        assert!(super::response_usage_event("missing-usage", &None).unwrap()["usage"].is_null());
    }

    #[test]
    fn stable_print_prompt_does_not_include_random_workspace() {
        assert_eq!(
            super::print_system_prompt("/tmp/one", true),
            super::print_system_prompt("/tmp/two", true)
        );
        assert!(super::print_system_prompt("/tmp/one", false).contains("/tmp/one"));
    }

    use super::*;

    fn discovered_local_model(
        provider: &str,
        context_tokens: u32,
    ) -> crate::model_catalog::ModelInfo {
        crate::model_catalog::ModelInfo {
            id: "test-model".to_owned(),
            name: "test-model".to_owned(),
            provider: provider.to_owned(),
            description: "test model".to_owned(),
            capabilities: crate::model_catalog::ModelCapabilities {
                protocol: crate::model_catalog::ModelProtocol::OpenAiChat,
                tools: false,
                vision: false,
                reasoning: false,
                streaming: true,
                context_tokens,
                output_tokens: None,
            },
            verification: crate::model_catalog::ModelVerification {
                state: crate::model_catalog::VerificationState::Verified,
                source: "test".to_owned(),
                detail: None,
            },
        }
    }

    static PRINT_LIMIT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn print_mode_allows_an_installed_but_idle_ollama_model() {
        let discovered = discovered_local_model("ollama", 0);

        assert!(validate_print_local_model("ollama/test-model", &discovered).is_ok());
    }

    #[test]
    fn print_mode_still_requires_live_context_for_other_local_runtimes() {
        let discovered = discovered_local_model("llamacpp", 0);

        let error = validate_print_local_model("llamacpp/test-model", &discovered).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not report its live context limit")
        );
    }

    #[test]
    fn print_limit_equal_to_catalog_default_is_still_explicit() {
        let _guard = PRINT_LIMIT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = "MAESTRO_TEST_PRINT_MAX_TOKENS_SOURCE";
        // SAFETY: this module serializes mutations of its private test-only key.
        unsafe { std::env::set_var(name, "16384") };

        let result = positive_env_with_presence(name, 16_384_u32);

        // SAFETY: see the serialized mutation above.
        unsafe { std::env::remove_var(name) };
        let (value, is_explicit) = result.unwrap();
        assert_eq!(value, 16_384);
        assert!(is_explicit);
    }

    #[test]
    fn sanitize_stream_chunk_strips_osc_injection_from_provider_delta() {
        // A minimal OSC-0 (set title) sequence embedded in a provider
        // stream delta -- this is what `--print`'s non-JSON branch writes
        // straight to `print!` with no ratatui `Buffer` to filter it.
        let input = "before\x1b]0;evil\x07after";
        let out = sanitize_stream_chunk(input);
        assert_eq!(out, "before]0;evilafter");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
    }

    #[test]
    fn sanitize_stream_chunk_preserves_ordinary_streamed_text() {
        let input = "Here is a plan:\n1. First\n2. Second\tindented";
        assert_eq!(sanitize_stream_chunk(input), input);
    }

    #[test]
    fn stream_chunk_for_stdout_sanitizes_only_when_a_terminal() {
        let input = "before\x1b]0;evil\x07after";
        assert_eq!(stream_chunk_for_stdout(input, true), "before]0;evilafter");
        // Redirected output must stay byte-exact, matching what
        // `--output-last-message` saves for the same run.
        assert_eq!(stream_chunk_for_stdout(input, false), input);
    }

    #[test]
    fn print_options_default_json_false() {
        let opts = PrintModeOptions {
            specialist: None,
            prompt: "hi".into(),
            json: false,
            model: None,
            output_last_message: None,
            output_schema: None,
            sandbox_policy: None,
            fail_on_approval: false,
            only_tools: Vec::new(),
            add_tools: Vec::new(),
            remove_tools: Vec::new(),
            tags: Vec::new(),
            log_group_id: None,
        };
        assert!(!opts.json);
        assert_eq!(opts.prompt, "hi");
    }

    #[test]
    fn print_mode_defers_every_tool_call_to_its_host_executor() {
        assert_eq!(print_mode_approval_mode(), ApprovalMode::Safe);
    }

    #[test]
    fn schema_required_keys() {
        let schema =
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;
        validate_against_schema(r#"{"name":"ok"}"#, schema).unwrap();
        assert!(validate_against_schema(r#"{"other":1}"#, schema).is_err());
        assert!(validate_against_schema("not-json", schema).is_err());
    }

    #[test]
    fn schema_array_items() {
        let schema = r#"{"type":"array","items":{"type":"number"}}"#;
        validate_against_schema("[1,2,3]", schema).unwrap();
        assert!(validate_against_schema(r#"["a"]"#, schema).is_err());
    }

    #[test]
    fn print_mode_waits_for_terminal_event_and_keeps_last_response() {
        let mut current = "I will inspect the file.".to_string();
        let mut last = String::new();
        record_completed_response("model-turn-1", &mut current, &mut last);
        assert_eq!(last, "I will inspect the file.");

        current.push_str("The final answer.");
        record_completed_response("model-turn-2", &mut current, &mut last);
        assert_eq!(last, "The final answer.");
        record_completed_response("done", &mut current, &mut last);
        assert_eq!(last, "The final answer.");
    }

    #[test]
    fn typed_turn_terminals_choose_process_exit_status() {
        assert_eq!(
            typed_terminal_exit_code(&FromAgent::TurnCompleted {
                response_id: "done".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            }),
            Some(0)
        );
        assert_eq!(
            typed_terminal_exit_code(&FromAgent::TurnInterrupted {
                response_id: "done".to_string(),
                reason: "cancelled".to_string(),
            }),
            Some(1)
        );
        assert_eq!(
            typed_terminal_exit_code(&FromAgent::ProviderError {
                kind: maestro_ai::ProviderStreamErrorKind::TransientProtocol,
                message: "unexpected eof".to_string(),
            }),
            Some(1)
        );
    }

    #[test]
    fn fail_approval_mode_denies_restricted_tools() {
        let executor = ToolExecutor::new(".");
        assert!(approval_denied(
            &executor,
            "write",
            &serde_json::json!({"file_path":"note.txt","content":"hi"}),
            true,
        ));
        assert!(!approval_denied(
            &executor,
            "read",
            &serde_json::json!({"file_path":"note.txt"}),
            true,
        ));
    }

    #[test]
    fn workspace_paths_reject_traversal() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();

        let read = prepare_workspace_tool_args(
            "read",
            &serde_json::json!({"path": outside_file}),
            workspace.path(),
        );
        assert!(read.is_err());
        let glob = prepare_workspace_tool_args(
            "glob",
            &serde_json::json!({"path": ".", "pattern": "../*.txt"}),
            workspace.path(),
        );
        assert!(glob.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_paths_reject_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), workspace.path().join("outside-link")).unwrap();

        let read = prepare_workspace_tool_args(
            "read",
            &serde_json::json!({"path": "outside-link/secret.txt"}),
            workspace.path(),
        );
        assert!(read.is_err());
        let glob = prepare_workspace_tool_args(
            "glob",
            &serde_json::json!({"path": ".", "pattern": "outside-link/*.txt"}),
            workspace.path(),
        );
        assert!(glob.is_err());
        let wildcarded_link = prepare_workspace_tool_args(
            "glob",
            &serde_json::json!({"path": ".", "pattern": "*/*.txt"}),
            workspace.path(),
        );
        assert!(wildcarded_link.is_err());
    }

    #[test]
    fn workspace_read_rewrites_to_canonical_path() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("nested")).unwrap();
        let file = workspace.path().join("nested").join("marker.txt");
        std::fs::write(&file, "marker").unwrap();

        let args = prepare_workspace_tool_args(
            "read",
            &serde_json::json!({"path": "nested/../nested/marker.txt"}),
            workspace.path(),
        )
        .unwrap();
        assert_eq!(
            args["path"].as_str(),
            dunce::canonicalize(&file).unwrap().to_str()
        );
    }
}
