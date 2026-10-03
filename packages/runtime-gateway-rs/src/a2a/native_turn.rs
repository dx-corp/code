use chrono::{DateTime, Utc};
use maestro_local_host::SandboxPolicy;
use maestro_local_host::agent::{CredentialVault, FromAgent, NativeAgent, NativeAgentConfig};
use maestro_local_host::state::ApprovalMode;
use maestro_local_host::tools::ToolExecutor;
use maestro_runtime::{ExecutionSource, TokenUsage, ToolResult};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use super::ValidatedSubagentTaskCapsule;
use crate::{
    A2A_DEFAULT_TURN_TIMEOUT_MS, A2ACancelReceiver, AppState, env_bool, env_u64,
    finish_tool_metadata, record_tool_call_metadata, trimmed_env,
};

fn a2a_explicit_terminal(event: &FromAgent) -> Option<Result<(), String>> {
    match event {
        FromAgent::TurnCompleted { .. } => Some(Ok(())),
        FromAgent::TurnInterrupted { reason, .. } => Some(Err(reason.clone())),
        FromAgent::ProviderError { kind, message } => {
            Some(Err(format!("provider failure ({kind:?}): {message}")))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct A2ASubagentExecutionPolicy {
    pub(crate) model: String,
    pub(crate) turn_timeout: Duration,
    pub(crate) guidance: String,
    pub(crate) allowed_tools: BTreeSet<String>,
    deadline_at: DateTime<Utc>,
    workspace_root: PathBuf,
    cwd: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    acceptance_checks: Vec<AcceptanceCheck>,
    sandbox_policy: SandboxPolicy,
}

impl A2ASubagentExecutionPolicy {
    pub(crate) fn guard_tool_call(&self, tool: &str, args: &Value) -> Result<(), String> {
        if !self.allowed_tools.contains(tool) {
            return Err(format!("tool {tool:?} is outside the task capsule"));
        }
        if !args.is_object() {
            return Err("tool arguments must be an object".to_string());
        }
        match tool {
            "read" => self.guard_path_argument(args, &["path", "file_path"], &self.read_roots),
            "glob" => {
                self.guard_optional_read_path(args)?;
                self.guard_glob_pattern(args)
            }
            "grep" | "list" | "find" | "diff" => self.guard_optional_read_path(args),
            "search" => self.guard_search_paths(args),
            "write" | "edit" => {
                self.guard_path_argument(args, &["path", "file_path"], &self.write_roots)
            }
            _ => Err(format!("tool {tool:?} has no capsule execution guard")),
        }
    }

    fn guard_path_argument(
        &self,
        args: &Value,
        names: &[&str],
        roots: &[PathBuf],
    ) -> Result<(), String> {
        let raw = names
            .iter()
            .find_map(|name| args.get(*name).and_then(Value::as_str))
            .ok_or_else(|| format!("tool path argument {} is required", names[0]))?;
        self.guard_path(raw, roots)
    }

    fn guard_optional_read_path(&self, args: &Value) -> Result<(), String> {
        match args.get("path") {
            Some(Value::String(path)) => self.guard_path(path, &self.read_roots),
            Some(_) => Err("tool path must be a string".to_string()),
            None => self.guard_path(&self.cwd.to_string_lossy(), &self.read_roots),
        }
    }

    fn guard_search_paths(&self, args: &Value) -> Result<(), String> {
        if args.get("cwd").is_some() {
            return Err("search cwd is server-owned for task capsules".to_string());
        }
        let Some(paths) = args.get("paths") else {
            return self.guard_path(&self.cwd.to_string_lossy(), &self.read_roots);
        };
        match paths {
            Value::String(path) => self.guard_path(path, &self.read_roots),
            Value::Array(paths) if !paths.is_empty() => {
                for path in paths {
                    let path = path
                        .as_str()
                        .ok_or_else(|| "search paths must be strings".to_string())?;
                    self.guard_path(path, &self.read_roots)?;
                }
                Ok(())
            }
            _ => Err("search paths must be a string or nonempty string array".to_string()),
        }
    }

    fn guard_glob_pattern(&self, args: &Value) -> Result<(), String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| "glob pattern is required".to_string())?;
        if pattern.trim().is_empty()
            || Path::new(pattern).is_absolute()
            || pattern.contains('\0')
            || pattern
                .split(['/', '\\'])
                .any(|component| component == "..")
        {
            return Err("glob pattern must stay relative to its guarded path".to_string());
        }
        Ok(())
    }

    fn guard_path(&self, raw: &str, roots: &[PathBuf]) -> Result<(), String> {
        self.resolve_guarded_path(raw, roots).map(|_| ())
    }

    fn resolve_guarded_path(&self, raw: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
        if raw.trim().is_empty() || raw.contains('\0') {
            return Err("tool path must be nonempty".to_string());
        }
        let path = Path::new(raw);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };
        let candidate = canonicalize_existing_ancestor(&candidate)?;
        roots
            .iter()
            .any(|root| candidate.starts_with(root))
            .then_some(candidate)
            .ok_or_else(|| format!("path {raw:?} is outside the task capsule"))
    }

    pub(crate) async fn execute_tool_call(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        mut cancel_rx: A2ACancelReceiver,
    ) -> ToolResult {
        if *cancel_rx.borrow() {
            return ToolResult::failure("task capsule canceled before tool execution");
        }
        let guarded_args = match self.guarded_tool_args(tool, args) {
            Ok(args) => args,
            Err(reason) => return ToolResult::failure(reason),
        };
        let (cwd, sandbox_policy) = if matches!(tool, "write" | "edit") {
            let raw = guarded_args
                .get("path")
                .or_else(|| guarded_args.get("file_path"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let target = PathBuf::from(raw);
            let Some(root) = self
                .write_roots
                .iter()
                .filter(|root| target.starts_with(root))
                .max_by_key(|root| root.components().count())
            else {
                return ToolResult::failure("write target is outside the task capsule");
            };
            (
                root.clone(),
                SandboxPolicy::WorkspaceWrite {
                    writable_roots: Vec::new(),
                    network_access: false,
                    exclude_tmpdir_env_var: true,
                    exclude_slash_tmp: true,
                },
            )
        } else {
            (self.workspace_root.clone(), SandboxPolicy::ReadOnly)
        };
        let executor = ToolExecutor::with_credential_vault(
            cwd.to_string_lossy().to_string(),
            CredentialVault::new(),
        )
        .with_sandbox_policy(sandbox_policy)
        .without_ambient_mutation_validators();
        let cancellation = CancellationToken::new();
        let execution = executor.execute_with_receipt_cancellable(
            tool,
            &guarded_args,
            None,
            call_id,
            cancellation.clone(),
        );
        tokio::pin!(execution);
        let remaining = match self.remaining_time() {
            Ok(remaining) => remaining,
            Err(reason) => return ToolResult::failure(reason),
        };
        tokio::select! {
            result = &mut execution => result.to_legacy(),
            _ = tokio::time::sleep(remaining) => {
                cancellation.cancel();
                let _ = execution.await;
                ToolResult::failure("task capsule deadline elapsed during tool execution")
            }
            changed = cancel_rx.changed() => {
                let reason = if changed.is_ok() && *cancel_rx.borrow() {
                    "task capsule canceled during tool execution"
                } else {
                    "task capsule cancellation channel closed"
                };
                cancellation.cancel();
                let _ = execution.await;
                ToolResult::failure(reason)
            }
        }
    }

    fn guarded_tool_args(&self, tool: &str, args: &Value) -> Result<Value, String> {
        self.guard_tool_call(tool, args)?;
        let mut guarded = args.clone();
        match tool {
            "read" | "write" | "edit" => {
                let key = if guarded.get("path").is_some() {
                    "path"
                } else {
                    "file_path"
                };
                let raw = guarded[key]
                    .as_str()
                    .ok_or_else(|| "tool path must be a string".to_string())?;
                let roots = if matches!(tool, "write" | "edit") {
                    &self.write_roots
                } else {
                    &self.read_roots
                };
                guarded[key] =
                    Value::String(self.resolve_guarded_path(raw, roots)?.display().to_string());
            }
            "glob" | "grep" | "list" | "find" | "diff" => {
                let raw = guarded
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| self.cwd.display().to_string());
                guarded["path"] = Value::String(
                    self.resolve_guarded_path(&raw, &self.read_roots)?
                        .display()
                        .to_string(),
                );
            }
            "search" => {
                if guarded.get("paths").is_none() {
                    guarded["paths"] = Value::String(self.cwd.display().to_string());
                }
                match guarded.get_mut("paths") {
                    Some(Value::String(path)) => {
                        *path = self
                            .resolve_guarded_path(path, &self.read_roots)?
                            .display()
                            .to_string();
                    }
                    Some(Value::Array(paths)) => {
                        for path in paths {
                            let raw = path
                                .as_str()
                                .ok_or_else(|| "search paths must be strings".to_string())?;
                            *path = Value::String(
                                self.resolve_guarded_path(raw, &self.read_roots)?
                                    .display()
                                    .to_string(),
                            );
                        }
                    }
                    _ => return Err("search paths are required".to_string()),
                }
            }
            _ => return Err(format!("tool {tool:?} has no capsule execution guard")),
        }
        if tool == "search" {
            guarded["cwd"] = Value::String(self.cwd.display().to_string());
        }
        Ok(guarded)
    }

    fn remaining_time(&self) -> Result<Duration, String> {
        (self.deadline_at - Utc::now())
            .to_std()
            .ok()
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "task capsule deadline has expired".to_string())
    }
}

#[derive(Debug, Clone)]
struct AcceptanceCheck {
    package: String,
    filter: String,
}

fn parse_acceptance_check(check: &str) -> Result<AcceptanceCheck, String> {
    let parts = check.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 5
        || parts[0] != "cargo"
        || parts[1] != "test"
        || parts[2] != "-p"
        || parts[3] != "maestro-runtime-gateway"
        || !parts[4]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':'))
    {
        return Err(format!(
            "acceptance check {check:?} is not in the server command allowlist"
        ));
    }
    Ok(AcceptanceCheck {
        package: parts[3].to_string(),
        filter: parts[4].to_string(),
    })
}

impl A2ASubagentExecutionPolicy {
    pub(crate) async fn run_acceptance_checks(
        &self,
        cancel_rx: &mut A2ACancelReceiver,
    ) -> Result<Vec<Value>, String> {
        let mut reports = Vec::new();
        for check in &self.acceptance_checks {
            let scratch = std::env::temp_dir().join(format!(
                "maestro-a2a-check-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or_default()
            ));
            tokio::fs::create_dir_all(&scratch)
                .await
                .map_err(|error| format!("cannot create acceptance-check scratch: {error}"))?;
            let manifest = self.workspace_root.join("Cargo.toml");
            let target = scratch.join("target");
            let command = format!(
                "CARGO_TARGET_DIR={} cargo test --manifest-path {} -p {} {}",
                shell_quote_path(&target),
                shell_quote_path(&manifest),
                check.package,
                check.filter
            );
            let executor = ToolExecutor::with_credential_vault(
                scratch.to_string_lossy().to_string(),
                CredentialVault::new(),
            )
            .with_sandbox_policy(SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            })
            .without_ambient_mutation_validators();
            let args = serde_json::json!({"command": command});
            let call_id = format!("acceptance:{}:{}", check.package, check.filter);
            let cancellation = CancellationToken::new();
            let execution = executor.execute_with_receipt_cancellable(
                "bash",
                &args,
                None,
                &call_id,
                cancellation.clone(),
            );
            tokio::pin!(execution);
            let remaining = self.remaining_time()?;
            let execution = tokio::select! {
                execution = &mut execution => execution,
                _ = tokio::time::sleep(remaining) => {
                    cancellation.cancel();
                    let _ = execution.await;
                    let _ = tokio::fs::remove_dir_all(&scratch).await;
                    return Err("task capsule deadline elapsed during acceptance checks".to_string());
                }
                changed = cancel_rx.changed() => {
                    cancellation.cancel();
                    let _ = execution.await;
                    let _ = tokio::fs::remove_dir_all(&scratch).await;
                    if changed.is_ok() && *cancel_rx.borrow() {
                        return Err("task capsule canceled during acceptance checks".to_string());
                    }
                    return Err("task capsule cancellation channel closed".to_string());
                }
            };
            let result = execution.to_legacy();
            let _ = tokio::fs::remove_dir_all(&scratch).await;
            reports.push(serde_json::json!({
                "kind": "acceptance.check",
                "package": check.package,
                "filter": check.filter,
                "success": result.success,
                "output": result.output
            }));
            if !result.success {
                return Err(format!(
                    "server-owned acceptance check failed for {} {}: {}",
                    check.package, check.filter, result.output
                ));
            }
        }
        Ok(reports)
    }
}

fn shell_quote_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

pub(crate) fn build_a2a_subagent_execution_policy(
    capsule: &ValidatedSubagentTaskCapsule,
    workspace_root: &Path,
    global_timeout: Duration,
    now: DateTime<Utc>,
) -> Result<A2ASubagentExecutionPolicy, String> {
    let workspace_root = dunce::canonicalize(workspace_root)
        .map_err(|error| format!("cannot resolve A2A workspace root: {error}"))?;
    if !capsule.in_scope_resources.is_empty() || !capsule.mutation_resources.is_empty() {
        return Err("resource-scoped task capsules have no fail-closed A2A executor".to_string());
    }
    let read_roots = resolve_capsule_roots(&workspace_root, &capsule.in_scope_paths, false)?;
    let write_roots = resolve_capsule_roots(&workspace_root, &capsule.mutation_paths, true)?;
    let cwd = write_roots
        .first()
        .or_else(|| read_roots.first())
        .cloned()
        .ok_or_else(|| "task capsule must declare at least one filesystem scope".to_string())?;

    let until_deadline = (capsule.deadline_at - now)
        .to_std()
        .map_err(|_| "task capsule deadline has expired".to_string())?;
    if until_deadline.is_zero() {
        return Err("task capsule deadline has expired".to_string());
    }
    let turn_timeout = global_timeout.min(until_deadline);
    let model = match capsule.model_route.as_str() {
        "haiku" => "anthropic/claude-haiku-4-5".to_string(),
        route => return Err(format!("model route {route:?} is not server-allowlisted")),
    };

    let mut allowed_tools = BTreeSet::new();
    for capability in &capsule.allowed_capabilities {
        match capability.as_str() {
            "repo:read" => {
                allowed_tools.extend(
                    ["diff", "find", "glob", "grep", "list", "read", "search"].map(str::to_string),
                );
            }
            "repo:write-scoped" => {
                allowed_tools.extend(["edit", "write"].map(str::to_string));
            }
            "tool:execute-tests" => {
                // Acceptance checks run after the child through a separate,
                // server-owned command path. The child never receives bash.
            }
            capability => {
                return Err(format!(
                    "capability {capability:?} has no fail-closed A2A execution mapping"
                ));
            }
        }
    }
    let acceptance_checks = capsule
        .acceptance_checks
        .iter()
        .map(|check| parse_acceptance_check(check))
        .collect::<Result<Vec<_>, _>>()?;

    let sandbox_policy = SandboxPolicy::ReadOnly;
    let guidance = deterministic_capsule_guidance(capsule, &cwd);

    Ok(A2ASubagentExecutionPolicy {
        model,
        turn_timeout,
        guidance,
        allowed_tools,
        deadline_at: capsule.deadline_at,
        workspace_root,
        cwd,
        read_roots,
        write_roots,
        acceptance_checks,
        sandbox_policy,
    })
}

pub(crate) fn build_a2a_subagent_execution_policy_for_state(
    state: &AppState,
    capsule: &ValidatedSubagentTaskCapsule,
) -> Result<A2ASubagentExecutionPolicy, String> {
    build_a2a_subagent_execution_policy(
        capsule,
        &state.config.cwd,
        Duration::from_millis(env_u64(
            "MAESTRO_A2A_TURN_TIMEOUT_MS",
            A2A_DEFAULT_TURN_TIMEOUT_MS,
        )),
        Utc::now(),
    )
}

fn resolve_capsule_roots(
    workspace_root: &Path,
    relative_roots: &[String],
    require_existing_directory: bool,
) -> Result<Vec<PathBuf>, String> {
    relative_roots
        .iter()
        .map(|relative| {
            let root = canonicalize_existing_ancestor(&workspace_root.join(relative))?;
            if !root.starts_with(workspace_root) {
                return Err(format!(
                    "capsule root {relative:?} resolves outside the workspace"
                ));
            }
            if require_existing_directory && !root.is_dir() {
                return Err(format!(
                    "capsule mutation root {relative:?} must be an existing directory"
                ));
            }
            Ok(root)
        })
        .collect()
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return Err(format!("cannot resolve path {}", path.display()));
        };
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("cannot resolve path {}", path.display()))?;
    }
    let mut resolved = dunce::canonicalize(ancestor)
        .map_err(|error| format!("cannot resolve path {}: {error}", path.display()))?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn deterministic_capsule_guidance(capsule: &ValidatedSubagentTaskCapsule, cwd: &Path) -> String {
    fn lines(values: &[String]) -> String {
        values
            .iter()
            .map(|value| format!("- {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
    let context_artifacts = capsule
        .context_artifacts
        .iter()
        .map(|(artifact_id, sha256)| format!("- {artifact_id} sha256:{sha256}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are executing server-governed task capsule {}.\n\
Task id: {}\nParent task id: {}\nLane: {}\nTask class: {}\n\
Objective:\n{}\n\
Execution cwd: {}\n\
In-scope paths:\n{}\nIn-scope resources:\n{}\n\
Mutation paths:\n{}\nMutation resources:\n{}\n\
Out of scope:\n{}\nContext artifacts:\n{}\n\
Expected artifact kinds:\n{}\nAcceptance checks:\n{}\nStop conditions:\n{}\n\
Retry limit: {}\n\
Never access a path or resource outside these boundaries. Stop rather than broaden scope.",
        super::SUBAGENT_TASK_CAPSULE_VERSION,
        capsule.task_id,
        capsule.parent_task_id,
        capsule.lane_id,
        capsule.task_class,
        capsule.objective,
        cwd.display(),
        lines(&capsule.in_scope_paths),
        lines(&capsule.in_scope_resources),
        lines(&capsule.mutation_paths),
        lines(&capsule.mutation_resources),
        lines(&capsule.out_of_scope),
        context_artifacts,
        lines(&capsule.expected_artifact_kinds),
        lines(&capsule.acceptance_checks),
        lines(&capsule.stop_conditions),
        capsule.retry_limit,
    )
}

#[derive(Debug, Default)]
pub(crate) struct A2ATurnOutput {
    pub(crate) assistant_text: String,
    pub(crate) thinking_text: String,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) tools: Vec<Value>,
    pub(crate) acceptance_reports: Vec<Value>,
}

pub(crate) enum A2ATurnResult {
    Completed(A2ATurnOutput),
    Canceled,
}

type A2ASessionTurnLocks = Mutex<HashMap<String, Weak<Mutex<()>>>>;

fn a2a_session_turn_locks() -> &'static A2ASessionTurnLocks {
    static LOCKS: OnceLock<A2ASessionTurnLocks> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn a2a_session_turn_mutex(session_id: &str) -> Arc<Mutex<()>> {
    let mut locks = a2a_session_turn_locks().lock().await;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(session_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(session_id.to_owned(), Arc::downgrade(&lock));
    lock
}

async fn acquire_a2a_session_turn(
    session_id: &str,
    cancel_rx: &mut A2ACancelReceiver,
) -> Option<OwnedMutexGuard<()>> {
    let lock = a2a_session_turn_mutex(session_id).await;
    loop {
        if *cancel_rx.borrow() {
            return None;
        }
        tokio::select! {
            guard = lock.clone().lock_owned() => return Some(guard),
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    return None;
                }
            }
        }
    }
}

pub(crate) async fn run_a2a_native_turn(
    state: &AppState,
    prompt: String,
    session_id: &str,
    mut cancel_rx: A2ACancelReceiver,
    capsule: Option<&ValidatedSubagentTaskCapsule>,
    execution_policy: Option<&A2ASubagentExecutionPolicy>,
) -> Result<A2ATurnResult, String> {
    if *cancel_rx.borrow() {
        return Ok(A2ATurnResult::Canceled);
    }
    let Some(_session_turn) = acquire_a2a_session_turn(session_id, &mut cancel_rx).await else {
        return Ok(A2ATurnResult::Canceled);
    };

    #[cfg(test)]
    if let Some(response) = trimmed_env("MAESTRO_A2A_FAKE_RESPONSE") {
        if a2a_wait_for_fake_response_delay(&mut cancel_rx).await {
            return Ok(A2ATurnResult::Canceled);
        }
        return Ok(A2ATurnResult::Completed(A2ATurnOutput {
            assistant_text: response,
            ..Default::default()
        }));
    }

    let global_timeout = Duration::from_millis(env_u64(
        "MAESTRO_A2A_TURN_TIMEOUT_MS",
        A2A_DEFAULT_TURN_TIMEOUT_MS,
    ));
    let execution_policy = match (capsule, execution_policy) {
        (Some(_), Some(policy)) => Some(policy),
        (Some(_), None) => {
            return Err(
                "governed task capsule is missing its pre-claim execution policy".to_string(),
            );
        }
        (None, None) => None,
        (None, Some(_)) => {
            return Err("subagent execution policy is missing its validated capsule".to_string());
        }
    };

    if let Some(response) = trimmed_env("MAESTRO_A2A_FAKE_RESPONSE") {
        if execution_policy.is_some() {
            return Err(
                "MAESTRO_A2A_FAKE_RESPONSE is disabled for governed task capsules".to_string(),
            );
        }
        if a2a_wait_for_fake_response_delay(&mut cancel_rx).await {
            return Ok(A2ATurnResult::Canceled);
        }
        return Ok(A2ATurnResult::Completed(A2ATurnOutput {
            assistant_text: response,
            ..Default::default()
        }));
    }

    let model = if let Some(policy) = execution_policy.as_ref() {
        policy.model.clone()
    } else if let Some(model) = trimmed_env("MAESTRO_A2A_MODEL") {
        model
    } else {
        let selected = state.selected_model.lock().await;
        format!("{}/{}", selected.provider, selected.id)
    };
    let base_system_prompt =
        trimmed_env("MAESTRO_A2A_SYSTEM_PROMPT").unwrap_or_else(|| {
            "You are the local Deixic Code Desktop A2A agent. Complete delegated work from peer agents clearly and concisely.".to_string()
        });
    let system_prompt = execution_policy
        .as_ref()
        .map_or(base_system_prompt.clone(), |policy| {
            format!("{base_system_prompt}\n\n{}", policy.guidance)
        });
    let config = NativeAgentConfig {
        model,
        cwd: execution_policy.as_ref().map_or_else(
            || state.config.cwd.to_string_lossy().to_string(),
            |policy| policy.cwd.to_string_lossy().to_string(),
        ),
        system_prompt: Some(system_prompt),
        thinking_enabled: env_bool("MAESTRO_A2A_THINKING").unwrap_or(false),
        thinking_budget: env::var("MAESTRO_A2A_THINKING_BUDGET")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10_000),
        approval_mode: execution_policy
            .as_ref()
            .map_or_else(ApprovalMode::default, |_| ApprovalMode::Safe),
        sandbox_policy: execution_policy
            .as_ref()
            .map(|policy| policy.sandbox_policy.clone()),
        ..NativeAgentConfig::default()
    };
    let capsule_allowed_tools = execution_policy
        .as_ref()
        .map(|policy| policy.allowed_tools.iter().cloned().collect::<HashSet<_>>());
    let (agent, mut events) = if execution_policy.is_some() {
        NativeAgent::new_with_allowed_tools_and_credential_vault(
            config,
            capsule_allowed_tools
                .as_ref()
                .expect("capsule tool set should exist"),
            CredentialVault::new(),
        )
    } else {
        NativeAgent::new(config)
    }
    .map_err(|error| error.to_string())?;
    agent
        .set_session_context(Some(session_id.to_owned()), "a2a", false)
        .map_err(|error| error.to_string())?;
    let prompt = execution_policy.as_ref().map_or(prompt.clone(), |policy| {
        format!("{}\n\nDelegated request:\n{prompt}", policy.guidance)
    });
    agent
        .prompt(prompt, Vec::new())
        .await
        .map_err(|error| error.to_string())?;

    let timeout = execution_policy
        .as_ref()
        .map_or(global_timeout, |policy| policy.turn_timeout);
    let approval_mode = trimmed_env("MAESTRO_A2A_TOOL_APPROVAL")
        .unwrap_or_else(|| "fail".to_string())
        .to_ascii_lowercase();
    let auto_approve_tools = matches!(approval_mode.as_str(), "auto" | "approve" | "approved");
    let mut output = A2ATurnOutput::default();
    let mut last_error: Option<String> = None;
    let mut turn_completed = false;
    let turn_timeout = tokio::time::sleep(timeout);
    tokio::pin!(turn_timeout);

    loop {
        let event = tokio::select! {
            _ = &mut turn_timeout => {
                agent.cancel();
                return Err("A2A native TUI turn timed out".to_string());
            }
            changed = cancel_rx.changed() => {
                if changed.is_ok() && *cancel_rx.borrow() {
                    agent.cancel();
                    return Ok(A2ATurnResult::Canceled);
                }
                continue;
            }
            event = events.recv() => event.ok_or_else(||
                "A2A native TUI turn stream ended before an explicit terminal".to_string()
            )?,
        };
        if let Some(terminal) = a2a_explicit_terminal(&event) {
            match terminal {
                Ok(()) => turn_completed = true,
                Err(message) => last_error = Some(message),
            }
            break;
        }
        match event {
            FromAgent::ResponseStart { .. } => {}
            FromAgent::ResponseChunk {
                content,
                is_thinking,
                ..
            } => {
                if is_thinking {
                    output.thinking_text.push_str(&content);
                } else {
                    output.assistant_text.push_str(&content);
                }
            }
            FromAgent::ResponseEnd { usage, .. } => {
                output.usage = usage;
            }
            FromAgent::ToolCall {
                call_id,
                tool,
                args,
                requires_approval,
                ..
            } => {
                record_tool_call_metadata(&mut output.tools, &call_id, &tool, args.clone());
                if let Some(policy) = execution_policy.as_ref() {
                    let result = policy
                        .execute_tool_call(&tool, &args, &call_id, cancel_rx.clone())
                        .await;
                    let success = result.success;
                    let _ = agent.tool_response_sender().send((
                        call_id.clone(),
                        true,
                        Some(result),
                        ExecutionSource::Native,
                        None,
                    ));
                    if !success {
                        finish_tool_metadata(&mut output.tools, &call_id, false);
                    }
                } else if requires_approval {
                    let _ = agent.tool_response_sender().send((
                        call_id.clone(),
                        auto_approve_tools,
                        None,
                        ExecutionSource::RemoteClient,
                        None,
                    ));
                    if !auto_approve_tools {
                        finish_tool_metadata(&mut output.tools, &call_id, false);
                    }
                }
            }
            FromAgent::ToolEnd {
                call_id, success, ..
            } => {
                finish_tool_metadata(&mut output.tools, &call_id, success);
            }
            FromAgent::HookBlocked {
                call_id,
                tool,
                reason,
            } => {
                if !output
                    .tools
                    .iter()
                    .any(|entry| entry.get("id").and_then(Value::as_str) == Some(&call_id))
                {
                    record_tool_call_metadata(&mut output.tools, &call_id, &tool, Value::Null);
                }
                finish_tool_metadata(&mut output.tools, &call_id, false);
                last_error = Some(reason);
            }
            FromAgent::Error {
                message,
                fatal,
                terminal,
                ..
            } => {
                last_error = Some(message);
                if fatal || terminal {
                    break;
                }
            }
            // A2A persists child ToolCall/ToolEnd metadata above. The grouped
            // UI summary is not task output or an additional owner receipt.
            FromAgent::CodeModeProgress { .. } => {}
            FromAgent::CodexSessionState { .. }
            | FromAgent::CodexTurnState { .. }
            | FromAgent::CodexUsageState { .. }
            | FromAgent::CodexCompatibility { .. } => {}
            _ => {}
        }
    }

    if turn_completed {
        if let Some(policy) = execution_policy.as_ref() {
            output.acceptance_reports = policy.run_acceptance_checks(&mut cancel_rx).await?;
            output
                .tools
                .extend(output.acceptance_reports.iter().cloned());
        }
        Ok(A2ATurnResult::Completed(output))
    } else {
        Err(last_error
            .unwrap_or_else(|| "A2A native TUI turn ended before an explicit terminal".to_string()))
    }
}

async fn a2a_wait_for_fake_response_delay(cancel_rx: &mut A2ACancelReceiver) -> bool {
    let delay_ms = env_u64("MAESTRO_A2A_FAKE_RESPONSE_DELAY_MS", 0);
    if delay_ms == 0 {
        return *cancel_rx.borrow();
    }

    let delay = tokio::time::sleep(Duration::from_millis(delay_ms));
    tokio::pin!(delay);
    tokio::select! {
        _ = &mut delay => *cancel_rx.borrow(),
        changed = cancel_rx.changed() => changed.is_ok() && *cancel_rx.borrow(),
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn capsule_read_tools_normalize_omitted_paths_to_the_authorized_cwd() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scope = workspace.path().join("scope");
        std::fs::create_dir(&scope).expect("scope");
        let workspace_root = dunce::canonicalize(workspace.path()).expect("workspace root");
        let scope = dunce::canonicalize(scope).expect("scope root");
        let allowed_tools = ["diff", "find", "glob", "grep", "list", "search"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let policy = A2ASubagentExecutionPolicy {
            model: "test".to_string(),
            turn_timeout: Duration::from_secs(1),
            guidance: String::new(),
            allowed_tools,
            deadline_at: Utc::now() + chrono::Duration::seconds(1),
            workspace_root,
            cwd: scope.clone(),
            read_roots: vec![scope.clone()],
            write_roots: Vec::new(),
            acceptance_checks: Vec::new(),
            sandbox_policy: SandboxPolicy::ReadOnly,
        };

        for (tool, args) in [
            ("diff", serde_json::json!({})),
            ("list", serde_json::json!({})),
            ("grep", serde_json::json!({"pattern": "TODO"})),
            ("find", serde_json::json!({"pattern": "*.rs"})),
            ("glob", serde_json::json!({"pattern": "*.rs"})),
        ] {
            let guarded = policy
                .guarded_tool_args(tool, &args)
                .unwrap_or_else(|error| panic!("{tool} omitted path should be valid: {error}"));
            assert_eq!(guarded["path"], scope.display().to_string(), "{tool}");
        }

        let search = policy
            .guarded_tool_args("search", &serde_json::json!({"pattern": "TODO"}))
            .expect("search omitted paths should be valid");
        assert_eq!(search["paths"], scope.display().to_string());
        assert_eq!(search["cwd"], scope.display().to_string());
    }

    #[test]
    fn response_end_is_not_an_a2a_turn_terminal() {
        assert!(
            a2a_explicit_terminal(&FromAgent::ResponseEnd {
                response_id: "done".to_string(),
                usage: None,
            })
            .is_none()
        );
        assert!(matches!(
            a2a_explicit_terminal(&FromAgent::TurnCompleted {
                response_id: "done".to_string(),
                coding_completion: None,
                coding_child_records: Vec::new(),
            }),
            Some(Ok(()))
        ));
        assert!(matches!(
            a2a_explicit_terminal(&FromAgent::ProviderError {
                kind: maestro_local_host::ai::ProviderStreamErrorKind::TransientProtocol,
                message: "unexpected eof".to_string(),
            }),
            Some(Err(message)) if message.contains("unexpected eof")
        ));
    }

    #[tokio::test]
    async fn one_session_serializes_turns_without_blocking_other_sessions() {
        let first = a2a_session_turn_mutex("session-serial").await;
        let same = a2a_session_turn_mutex("session-serial").await;
        let other = a2a_session_turn_mutex("session-parallel").await;
        assert!(Arc::ptr_eq(&first, &same));
        assert!(!Arc::ptr_eq(&first, &other));

        let guard = first.lock_owned().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), same.clone().lock_owned())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), other.lock_owned())
                .await
                .is_ok()
        );
        drop(guard);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), same.lock_owned())
                .await
                .is_ok()
        );
    }
}
