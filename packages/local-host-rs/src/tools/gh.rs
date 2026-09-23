//! GitHub CLI helpers using `gh api`.
//!
//! This module provides wrappers around the GitHub CLI (`gh`) for common
//! repository operations like managing pull requests, issues, and repositories.
//!
//! # Requirements
//!
//! The `gh` CLI must be installed and authenticated. See <https://cli.github.com/>
//!
//! # Example
//!
//! ```rust,ignore
//! use maestro_local_host::tools::gh::{gh_pr, GhPrArgs};
//! use serde_json::json;
//!
//! // List open pull requests
//! let result = gh_pr(json!({"action": "list", "state": "open"}), ".").await;
//! ```

use serde::Deserialize;
use serde_json::Value;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::agent::ToolResult;

#[cfg(test)]
static TEST_GH_BINARY: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_COMMAND_SPAWNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static TEST_GH_OVERRIDE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn new_gh_command() -> tokio::process::Command {
    #[cfg(test)]
    if let Some(path) = TEST_GH_BINARY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return tokio::process::Command::new(path);
    }
    tokio::process::Command::new("gh")
}

#[derive(Debug)]
enum GhCommandError {
    Failed(String),
    Cancelled,
    Indeterminate(String),
}

impl From<String> for GhCommandError {
    fn from(value: String) -> Self {
        Self::Failed(value)
    }
}

fn gh_error_result(error: GhCommandError) -> ToolResult {
    match error {
        GhCommandError::Failed(message) => ToolResult::failure(message),
        GhCommandError::Cancelled => ToolResult::failure("GitHub command cancelled")
            .with_details(serde_json::json!({"cancelled": true})),
        GhCommandError::Indeterminate(message) => {
            ToolResult::failure(message).with_details(serde_json::json!({
                "cancelled": true,
                "remoteOutcome": "unknown",
                "retryable": false,
                "requiresReconciliation": true
            }))
        }
    }
}

fn classify_wait_error(error: std::io::Error, await_terminal_after_start: bool) -> GhCommandError {
    if await_terminal_after_start {
        GhCommandError::Indeterminate(format!(
            "GitHub write ended without a readable terminal response: {error}; remote outcome is unknown and must be reconciled before retry"
        ))
    } else {
        GhCommandError::Failed(error.to_string())
    }
}

#[cfg(any(windows, test))]
fn cleanup_error_after_terminal(
    error: std::io::Error,
    await_terminal_after_start: bool,
) -> Option<GhCommandError> {
    if await_terminal_after_start {
        // A real terminal Output is authoritative for the remote write. Do not
        // replace it with a local job-object cleanup error and invite a retry.
        None
    } else {
        Some(GhCommandError::Failed(error.to_string()))
    }
}

async fn run_command_output(
    command: Command,
    cancel: Option<&CancellationToken>,
) -> Result<Output, GhCommandError> {
    run_command_output_with_policy(command, cancel, false).await
}

async fn run_command_output_with_policy(
    mut command: Command,
    cancel: Option<&CancellationToken>,
    await_terminal_after_start: bool,
) -> Result<Output, GhCommandError> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err(GhCommandError::Cancelled);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        super::process_utils::set_new_process_group(&mut command);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }
    let child = command
        .spawn()
        .map_err(|error| GhCommandError::Failed(error.to_string()))?;
    #[cfg(test)]
    TEST_COMMAND_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    #[cfg(unix)]
    let mut process_group = ProcessGroupGuard::new(child.id());
    #[cfg(windows)]
    let mut job = JobObjectGuard::assign(&child)
        .map_err(|error| GhCommandError::Failed(error.to_string()))?;
    #[cfg(windows)]
    resume_suspended_process(&child).map_err(|error| GhCommandError::Failed(error.to_string()))?;
    let mut output = Box::pin(child.wait_with_output());

    let terminal = match (cancel, await_terminal_after_start) {
        (Some(cancel), true) => {
            tokio::select! {
                biased;
                result = &mut output => result,
                () = cancel.cancelled() => {
                    match tokio::time::timeout(Duration::from_secs(2), &mut output).await {
                        Ok(result) => result,
                        Err(_) => {
                            #[cfg(unix)]
                            drop(process_group);
                            #[cfg(windows)]
                            drop(job);
                            let _ = tokio::time::timeout(Duration::from_secs(2), &mut output).await;
                            return Err(GhCommandError::Indeterminate(
                                "GitHub write did not produce a terminal response before shutdown; remote outcome is unknown and must be reconciled before retry".to_string(),
                            ));
                        }
                    }
                }
            }
        }
        (None, true) => output.await,
        (Some(cancel), false) => {
            tokio::select! {
                biased;
                result = &mut output => result,
                () = cancel.cancelled() => {
                    #[cfg(unix)]
                    drop(process_group);
                    #[cfg(windows)]
                    drop(job);
                    let _ = tokio::time::timeout(Duration::from_secs(2), &mut output).await;
                    return Err(GhCommandError::Cancelled);
                }
            }
        }
        (None, false) => output.await,
    };
    if !await_terminal_after_start
        && cancel.is_some_and(CancellationToken::is_cancelled)
        && !matches!(terminal.as_ref(), Ok(output) if output.status.success())
    {
        return Err(GhCommandError::Cancelled);
    }
    if terminal.is_ok() {
        #[cfg(unix)]
        process_group.disarm();
        #[cfg(windows)]
        if let Err(error) = job.disarm() {
            if let Some(error) = cleanup_error_after_terminal(error, await_terminal_after_start) {
                return Err(error);
            }
        }
    }
    terminal.map_err(|error| classify_wait_error(error, await_terminal_after_start))
}

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

#[cfg(unix)]
use super::process_utils::ProcessGroupGuard;

#[cfg(windows)]
struct OwnedWindowsHandle(HANDLE);

#[cfg(windows)]
unsafe impl Send for OwnedWindowsHandle {}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the valid handle created by the
        // corresponding Win32 API call.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct JobObjectGuard(OwnedWindowsHandle);

#[cfg(windows)]
impl JobObjectGuard {
    fn assign(child: &tokio::process::Child) -> std::io::Result<Self> {
        // SAFETY: null security attributes and name request an unnamed job
        // object with default security.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = OwnedWindowsHandle(job);

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits has the exact layout and size required by this
        // information class, and job remains live for the call.
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let process_handle = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("spawned process has no handle"))?
            as HANDLE;
        // SAFETY: Tokio owns a live process handle until child is dropped.
        if unsafe { AssignProcessToJobObject(job.0, process_handle) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self(job))
    }

    fn disarm(&mut self) -> std::io::Result<()> {
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: limits has the exact layout and size required by this
        // information class, and this guard still owns a live job handle.
        if unsafe {
            SetInformationJobObject(
                self.0.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn resume_suspended_process(child: &tokio::process::Child) -> std::io::Result<()> {
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("spawned process has no pid"))?;
    // SAFETY: the snapshot has no caller-owned backing storage.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let snapshot = OwnedWindowsHandle(snapshot);
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut resumed = 0usize;

    // SAFETY: entry is initialized with the required structure size and
    // remains valid while the snapshot is enumerated.
    let mut has_entry = unsafe { Thread32First(snapshot.0, &mut entry) };
    while has_entry != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: the thread id came from the live system snapshot.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let thread = OwnedWindowsHandle(thread);
            // SAFETY: thread has THREAD_SUSPEND_RESUME access.
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                return Err(std::io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: same initialized snapshot and entry as above.
        has_entry = unsafe { Thread32Next(snapshot.0, &mut entry) };
    }

    if resumed == 0 {
        return Err(std::io::Error::other(
            "spawned process had no resumable threads",
        ));
    }
    Ok(())
}

/// Arguments for GitHub Pull Request operations.
///
/// Used by [`gh_pr`] to perform PR actions like create, list, view, checkout, etc.
#[derive(Debug, Deserialize)]
pub struct GhPrArgs {
    action: String,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    label: Option<Vec<String>>,
    #[serde(default)]
    milestone: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    json: Option<bool>,
    #[serde(default, alias = "nameOnly")]
    name_only: Option<bool>,
    #[serde(default)]
    repository: Option<String>,
}

/// Arguments for GitHub Issue operations.
///
/// Used by [`gh_issue`] to perform issue actions like create, list, view, comment, etc.
#[derive(Debug, Deserialize)]
pub struct GhIssueArgs {
    action: String,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    json: Option<bool>,
    #[serde(default)]
    repository: Option<String>,
}

/// Arguments for GitHub Repository operations.
///
/// Used by [`gh_repo`] to perform repo actions like view, fork, and clone.
#[derive(Debug, Deserialize)]
pub struct GhRepoArgs {
    action: String,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    json: Option<bool>,
}

async fn ensure_gh_available(cancel: Option<&CancellationToken>) -> Result<(), GhCommandError> {
    let mut command = new_gh_command();
    command.arg("--version");
    let output = run_command_output(command, cancel)
        .await
        .map_err(|error| match error {
            GhCommandError::Failed(message) => {
                GhCommandError::Failed(format!("Failed to run gh: {message}"))
            }
            GhCommandError::Cancelled => GhCommandError::Cancelled,
            GhCommandError::Indeterminate(message) => GhCommandError::Indeterminate(message),
        })?;
    if !output.status.success() {
        return Err(GhCommandError::Failed(
            "GitHub CLI (gh) is not available".to_string(),
        ));
    }
    Ok(())
}

fn append_field(args: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::String(s) => {
            args.push("-f".to_string());
            args.push(format!("{key}={s}"));
        }
        Value::Number(n) => {
            args.push("-F".to_string());
            args.push(format!("{key}={n}"));
        }
        Value::Bool(b) => {
            args.push("-F".to_string());
            args.push(format!("{key}={b}"));
        }
        Value::Array(values) => {
            for item in values {
                append_field(args, &format!("{key}[]"), item);
            }
        }
        Value::Null => {}
        Value::Object(_) => {}
    }
}

async fn run_gh_api(
    endpoint: &str,
    method: &str,
    fields: Vec<(String, Value)>,
    headers: Vec<String>,
    gh_repo: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<String, GhCommandError> {
    let mut cmd = new_gh_command();
    cmd.arg("api");
    cmd.arg(endpoint);
    cmd.arg("--method");
    cmd.arg(method);
    for header in headers {
        cmd.arg("-H").arg(header);
    }

    let mut args: Vec<String> = Vec::new();
    for (key, value) in fields {
        append_field(&mut args, &key, &value);
    }
    if !args.is_empty() {
        cmd.args(args);
    }
    if let Some(repo) = gh_repo {
        cmd.env("GH_REPO", repo);
    }

    let await_terminal_after_start = !method.eq_ignore_ascii_case("GET");
    let output = run_command_output_with_policy(cmd, cancel, await_terminal_after_start)
        .await
        .map_err(|error| match error {
            GhCommandError::Failed(message) => {
                GhCommandError::Failed(format!("Failed to run gh api: {message}"))
            }
            GhCommandError::Cancelled => GhCommandError::Cancelled,
            GhCommandError::Indeterminate(message) => GhCommandError::Indeterminate(message),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(GhCommandError::Failed(if stderr.is_empty() {
            "gh api failed".to_string()
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn git_current_branch(
    cwd: &str,
    cancel: Option<&CancellationToken>,
) -> Result<String, GhCommandError> {
    let mut command = Command::new("git");
    command
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .current_dir(cwd);
    let output = run_command_output(command, cancel)
        .await
        .map_err(|error| match error {
            GhCommandError::Failed(message) => {
                GhCommandError::Failed(format!("Failed to run git: {message}"))
            }
            GhCommandError::Cancelled => GhCommandError::Cancelled,
            GhCommandError::Indeterminate(message) => GhCommandError::Indeterminate(message),
        })?;
    if !output.status.success() {
        return Err(GhCommandError::Failed(
            "Unable to determine current branch".to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn resolve_default_branch(
    gh_repo: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<String, GhCommandError> {
    let output = run_gh_api(
        "repos/{owner}/{repo}",
        "GET",
        Vec::new(),
        Vec::new(),
        gh_repo,
        cancel,
    )
    .await?;
    let json: Value =
        serde_json::from_str(&output).map_err(|error| GhCommandError::Failed(error.to_string()))?;
    json.get("default_branch")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| GhCommandError::Failed("Failed to read default_branch".to_string()))
}

/// Tag a successful `gh_*` result with the repository it came from.
///
/// GitHub issue/PR bodies, comments, and repo metadata (README/description)
/// are free text authored by arbitrary GitHub users, not by this codebase or
/// its operator. Attaching an `origin` lets
/// `agent::protocol::ToolExecution::model_content()` show provenance in the
/// untrusted-content envelope it wraps this output in. Only successful
/// results carry remote content worth tagging; failures are already
/// agent-authored error strings.
fn with_repo_origin(result: ToolResult, repo: Option<&str>) -> ToolResult {
    if !result.success {
        return result;
    }
    let origin = repo.unwrap_or("(gh CLI default repository)");
    result.with_details(serde_json::json!({ "origin": format!("github:{origin}") }))
}

async fn resolve_repo_full_name(
    gh_repo: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<String, GhCommandError> {
    if let Some(repo) = gh_repo {
        return Ok(repo.to_string());
    }
    let output = run_gh_api(
        "repos/{owner}/{repo}",
        "GET",
        Vec::new(),
        Vec::new(),
        gh_repo,
        cancel,
    )
    .await?;
    let json: Value =
        serde_json::from_str(&output).map_err(|error| GhCommandError::Failed(error.to_string()))?;
    json.get("full_name")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| GhCommandError::Failed("Failed to read repo name".to_string()))
}

/// Execute a GitHub Pull Request operation.
///
/// # Supported Actions
///
/// - `create` - Create a new PR (requires `title`, optional `body`, `branch`, `base`, `draft`)
/// - `list` - List PRs (optional `state`, `author`, `label`, `milestone`, `limit`)
/// - `view` - View a specific PR (requires `number`) or list all
/// - `checkout` - Checkout a PR branch locally (requires `number`)
/// - `comment` - Add a comment to a PR (requires `number`, `body`)
/// - `checks` - View CI check status (requires `number`)
/// - `diff` - Get PR diff (requires `number`, optional `nameOnly`)
///
/// # Arguments
///
/// * `args` - JSON value containing [`GhPrArgs`] fields
/// * `cwd` - Current working directory for git operations
pub(crate) async fn gh_pr(
    args: Value,
    cwd: &str,
    cancel: Option<&CancellationToken>,
) -> ToolResult {
    let parsed: GhPrArgs = match serde_json::from_value(args) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid gh_pr arguments: {err}")),
    };

    if let Err(err) = ensure_gh_available(cancel).await {
        return gh_error_result(err);
    }

    let _ = parsed.json.as_ref();
    let repo = parsed.repository.as_deref();
    let result = match parsed.action.as_str() {
        "create" => {
            let title = match parsed.title {
                Some(val) => val,
                None => return ToolResult::failure("title required for create".to_string()),
            };
            let head = parsed.branch.clone().unwrap_or_default();
            let head = if head.is_empty() {
                match git_current_branch(cwd, cancel).await {
                    Ok(branch) => branch,
                    Err(err) => return gh_error_result(err),
                }
            } else {
                head
            };
            let base = match parsed.base {
                Some(val) => val,
                None => match resolve_default_branch(repo, cancel).await {
                    Ok(branch) => branch,
                    Err(err) => return gh_error_result(err),
                },
            };
            let mut fields = vec![
                ("title".to_string(), Value::String(title)),
                ("head".to_string(), Value::String(head)),
                ("base".to_string(), Value::String(base)),
            ];
            if let Some(body) = parsed.body {
                fields.push(("body".to_string(), Value::String(body)));
            }
            if parsed.draft.unwrap_or(false) {
                fields.push(("draft".to_string(), Value::Bool(true)));
            }

            match run_gh_api(
                "repos/{owner}/{repo}/pulls",
                "POST",
                fields,
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "checkout" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for checkout".to_string()),
            };
            let output = match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/pulls/{number}"),
                "GET",
                Vec::new(),
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => output,
                Err(err) => return gh_error_result(err),
            };
            let json: Value = match serde_json::from_str(&output) {
                Ok(val) => val,
                Err(err) => return ToolResult::failure(format!("Invalid PR response: {err}")),
            };
            let head_ref = json
                .get("head")
                .and_then(|v| v.get("ref"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing PR head ref".to_string());
            let head_ref = match head_ref {
                Ok(val) => val.to_string(),
                Err(err) => return ToolResult::failure(err),
            };
            let repo_url = json
                .get("head")
                .and_then(|v| v.get("repo"))
                .and_then(|v| v.get("clone_url"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing PR head repo url".to_string());
            let repo_url = match repo_url {
                Ok(val) => val.to_string(),
                Err(err) => return ToolResult::failure(err),
            };

            let branch_name = format!("pr-{number}");
            let mut fetch = Command::new("git");
            fetch
                .arg("fetch")
                .arg(&repo_url)
                .arg(&head_ref)
                .current_dir(cwd);
            match run_command_output(fetch, cancel).await {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    return ToolResult::failure(format!(
                        "git fetch failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                Err(error) => return gh_error_result(error),
            }

            let mut checkout = Command::new("git");
            checkout
                .arg("checkout")
                .arg("-B")
                .arg(&branch_name)
                .arg("FETCH_HEAD")
                .current_dir(cwd);
            match run_command_output(checkout, cancel).await {
                Ok(output) if output.status.success() => {
                    ToolResult::success(format!("Checked out PR #{number} as {branch_name}"))
                }
                Ok(output) => ToolResult::failure(format!(
                    "git checkout failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => gh_error_result(error),
            }
        }
        "view" => {
            let number = parsed.number;
            let endpoint = if let Some(num) = number {
                format!("repos/{{owner}}/{{repo}}/pulls/{num}")
            } else {
                "repos/{owner}/{repo}/pulls".to_string()
            };
            match run_gh_api(&endpoint, "GET", Vec::new(), Vec::new(), repo, cancel).await {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "list" => {
            let limit = parsed.limit.unwrap_or(30).min(100);
            let mut fields = vec![("per_page".to_string(), Value::Number(limit.into()))];
            if let Some(state) = &parsed.state {
                fields.push(("state".to_string(), Value::String(state.clone())));
            }

            let use_search =
                parsed.label.is_some() || parsed.milestone.is_some() || parsed.author.is_some();
            if use_search {
                let repo_name = match resolve_repo_full_name(repo, cancel).await {
                    Ok(name) => name,
                    Err(err) => return gh_error_result(err),
                };
                let mut query = format!("repo:{repo_name} is:pr");
                if let Some(state) = parsed.state {
                    if state != "all" {
                        query.push_str(&format!(" state:{state}"));
                    }
                }
                if let Some(author) = parsed.author {
                    query.push_str(&format!(" author:{author}"));
                }
                if let Some(labels) = parsed.label {
                    for label in labels {
                        query.push_str(&format!(" label:\"{label}\""));
                    }
                }
                if let Some(milestone) = parsed.milestone {
                    query.push_str(&format!(" milestone:\"{milestone}\""));
                }
                let fields = vec![
                    ("q".to_string(), Value::String(query)),
                    ("per_page".to_string(), Value::Number(limit.into())),
                ];
                match run_gh_api("search/issues", "GET", fields, Vec::new(), repo, cancel).await {
                    Ok(output) => ToolResult::success(output),
                    Err(err) => gh_error_result(err),
                }
            } else {
                match run_gh_api(
                    "repos/{owner}/{repo}/pulls",
                    "GET",
                    fields,
                    Vec::new(),
                    repo,
                    cancel,
                )
                .await
                {
                    Ok(output) => ToolResult::success(output),
                    Err(err) => gh_error_result(err),
                }
            }
        }
        "comment" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for comment".to_string()),
            };
            let body = match parsed.body {
                Some(val) => val,
                None => return ToolResult::failure("body required for comment".to_string()),
            };
            let fields = vec![("body".to_string(), Value::String(body))];
            match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/issues/{number}/comments"),
                "POST",
                fields,
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "checks" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for checks".to_string()),
            };
            let pr_output = match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/pulls/{number}"),
                "GET",
                Vec::new(),
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => output,
                Err(err) => return gh_error_result(err),
            };
            let json: Value = match serde_json::from_str(&pr_output) {
                Ok(val) => val,
                Err(err) => return ToolResult::failure(format!("Invalid PR response: {err}")),
            };
            let sha = json
                .get("head")
                .and_then(|v| v.get("sha"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing PR head sha".to_string());
            let sha = match sha {
                Ok(val) => val.to_string(),
                Err(err) => return ToolResult::failure(err),
            };
            match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/commits/{sha}/check-runs"),
                "GET",
                Vec::new(),
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "diff" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for diff".to_string()),
            };
            if parsed.name_only.unwrap_or(false) {
                match run_gh_api(
                    &format!("repos/{{owner}}/{{repo}}/pulls/{number}/files"),
                    "GET",
                    vec![("per_page".to_string(), Value::Number(100.into()))],
                    Vec::new(),
                    repo,
                    cancel,
                )
                .await
                {
                    Ok(output) => {
                        let json: Value = serde_json::from_str(&output).unwrap_or(Value::Null);
                        if let Some(files) = json.as_array() {
                            let names: Vec<String> = files
                                .iter()
                                .filter_map(|f| f.get("filename").and_then(|v| v.as_str()))
                                .map(std::string::ToString::to_string)
                                .collect();
                            ToolResult::success(names.join("\n"))
                        } else {
                            ToolResult::success(output)
                        }
                    }
                    Err(err) => gh_error_result(err),
                }
            } else {
                match run_gh_api(
                    &format!("repos/{{owner}}/{{repo}}/pulls/{number}"),
                    "GET",
                    Vec::new(),
                    vec!["Accept: application/vnd.github.v3.diff".to_string()],
                    repo,
                    cancel,
                )
                .await
                {
                    Ok(output) => ToolResult::success(output),
                    Err(err) => gh_error_result(err),
                }
            }
        }
        _ => ToolResult::failure("Unsupported gh_pr action".to_string()),
    };
    with_repo_origin(result, repo)
}

/// Execute a GitHub Issue operation.
///
/// # Supported Actions
///
/// - `create` - Create a new issue (requires `title`, optional `body`, `labels`)
/// - `list` - List issues (optional `state`, `author`, `labels`, `limit`)
/// - `view` - View a specific issue (requires `number`)
/// - `comment` - Add a comment to an issue (requires `number`, `body`)
/// - `close` - Close an issue (requires `number`)
///
/// # Arguments
///
/// * `args` - JSON value containing [`GhIssueArgs`] fields
pub(crate) async fn gh_issue(args: Value, cancel: Option<&CancellationToken>) -> ToolResult {
    let parsed: GhIssueArgs = match serde_json::from_value(args) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid gh_issue arguments: {err}")),
    };

    if let Err(err) = ensure_gh_available(cancel).await {
        return gh_error_result(err);
    }

    let _ = parsed.json.as_ref();
    let repo = parsed.repository.as_deref();
    let result = match parsed.action.as_str() {
        "create" => {
            let title = match parsed.title {
                Some(val) => val,
                None => return ToolResult::failure("title required for create".to_string()),
            };
            let mut fields = vec![("title".to_string(), Value::String(title))];
            if let Some(body) = parsed.body {
                fields.push(("body".to_string(), Value::String(body)));
            }
            if let Some(labels) = parsed.labels {
                fields.push((
                    "labels".to_string(),
                    Value::Array(labels.into_iter().map(Value::String).collect()),
                ));
            }
            match run_gh_api(
                "repos/{owner}/{repo}/issues",
                "POST",
                fields,
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "view" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for view".to_string()),
            };
            match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/issues/{number}"),
                "GET",
                Vec::new(),
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "list" => {
            let limit = parsed.limit.unwrap_or(30).min(100);
            let mut fields = vec![("per_page".to_string(), Value::Number(limit.into()))];
            if let Some(state) = parsed.state {
                fields.push(("state".to_string(), Value::String(state)));
            }
            if let Some(author) = parsed.author {
                fields.push(("creator".to_string(), Value::String(author)));
            }
            if let Some(labels) = parsed.labels {
                if !labels.is_empty() {
                    fields.push(("labels".to_string(), Value::String(labels.join(","))));
                }
            }
            match run_gh_api(
                "repos/{owner}/{repo}/issues",
                "GET",
                fields,
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "comment" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for comment".to_string()),
            };
            let body = match parsed.body {
                Some(val) => val,
                None => return ToolResult::failure("body required for comment".to_string()),
            };
            let fields = vec![("body".to_string(), Value::String(body))];
            match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/issues/{number}/comments"),
                "POST",
                fields,
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "close" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for close".to_string()),
            };
            let fields = vec![("state".to_string(), Value::String("closed".to_string()))];
            match run_gh_api(
                &format!("repos/{{owner}}/{{repo}}/issues/{number}"),
                "PATCH",
                fields,
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        _ => ToolResult::failure("Unsupported gh_issue action".to_string()),
    };
    with_repo_origin(result, repo)
}

/// Execute a GitHub Repository operation.
///
/// # Supported Actions
///
/// - `view` - View repository information
/// - `fork` - Fork the repository to your account
/// - `clone` - Clone the repository locally (optional `directory`)
///
/// # Arguments
///
/// * `args` - JSON value containing [`GhRepoArgs`] fields
/// * `cwd` - Current working directory for clone operations
pub(crate) async fn gh_repo(
    args: Value,
    cwd: &str,
    cancel: Option<&CancellationToken>,
) -> ToolResult {
    let parsed: GhRepoArgs = match serde_json::from_value(args) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid gh_repo arguments: {err}")),
    };

    if let Err(err) = ensure_gh_available(cancel).await {
        return gh_error_result(err);
    }

    let _ = parsed.json.as_ref();
    let repo = parsed.repository.as_deref();
    let result = match parsed.action.as_str() {
        "view" => {
            match run_gh_api(
                "repos/{owner}/{repo}",
                "GET",
                Vec::new(),
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => ToolResult::success(output),
                Err(err) => gh_error_result(err),
            }
        }
        "fork" => match run_gh_api(
            "repos/{owner}/{repo}/forks",
            "POST",
            Vec::new(),
            Vec::new(),
            repo,
            cancel,
        )
        .await
        {
            Ok(output) => ToolResult::success(output),
            Err(err) => gh_error_result(err),
        },
        "clone" => {
            let repo_name = match resolve_repo_full_name(repo, cancel).await {
                Ok(name) => name,
                Err(err) => return gh_error_result(err),
            };
            let output = match run_gh_api(
                "repos/{owner}/{repo}",
                "GET",
                Vec::new(),
                Vec::new(),
                repo,
                cancel,
            )
            .await
            {
                Ok(output) => output,
                Err(err) => return gh_error_result(err),
            };
            let json: Value = match serde_json::from_str(&output) {
                Ok(val) => val,
                Err(err) => return ToolResult::failure(format!("Invalid repo response: {err}")),
            };
            let clone_url = json
                .get("clone_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing clone_url".to_string());
            let clone_url = match clone_url {
                Ok(val) => val.to_string(),
                Err(err) => return ToolResult::failure(err),
            };
            let dir = parsed.directory.unwrap_or_else(|| {
                repo_name
                    .split('/')
                    .next_back()
                    .unwrap_or("repo")
                    .to_string()
            });
            let mut clone = Command::new("git");
            clone
                .arg("clone")
                .arg(&clone_url)
                .arg(&dir)
                .current_dir(cwd);
            match run_command_output(clone, cancel).await {
                Ok(output) if output.status.success() => {
                    ToolResult::success(format!("Cloned {repo_name} to {dir}"))
                }
                Ok(output) => ToolResult::failure(format!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => gh_error_result(error),
            }
        }
        _ => ToolResult::failure("Unsupported gh_repo action".to_string()),
    };
    with_repo_origin(result, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Process scheduling on a loaded CI runner can outlast the command's own
    // shutdown bound. Wait for the fake command to start before testing it.
    #[cfg(unix)]
    const GH_TEST_PROCESS_START_TIMEOUT: Duration = Duration::from_secs(10);

    #[cfg(unix)]
    struct TestGhOverride;

    #[cfg(unix)]
    impl TestGhOverride {
        fn install(path: std::path::PathBuf) -> Self {
            *TEST_GH_BINARY
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
            Self
        }
    }

    #[cfg(unix)]
    impl Drop for TestGhOverride {
        fn drop(&mut self) {
            *TEST_GH_BINARY
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_gh_command_kills_and_reaps_its_process_group() {
        let workspace = tempfile::tempdir().expect("workspace");
        let pid_path = workspace.path().join("pid");
        let sentinel_path = workspace.path().join("sentinel");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "printf '%s' \"$$\" > '{}'; sleep 0.4; printf leaked > '{}'",
                pid_path.display(),
                sentinel_path.display()
            ))
            .current_dir(workspace.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let execution =
            tokio::spawn(async move { run_command_output(command, Some(&cancel_for_task)).await });

        tokio::time::timeout(GH_TEST_PROCESS_START_TIMEOUT, async {
            while !pid_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("command should publish its pid");
        cancel.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
            .await
            .expect("cancelled gh command must finish within the shutdown bound")
            .expect("gh command task should not panic");
        assert!(matches!(result, Err(GhCommandError::Cancelled)));
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !sentinel_path.exists(),
            "cancelled gh command survived and mutated after shutdown"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mutating_gh_issue_cancellation_awaits_the_remote_terminal_response() {
        use std::os::unix::fs::PermissionsExt as _;

        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        let pid_path = workspace.path().join("api.pid");
        let completion_path = workspace.path().join("completed");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
                 printf '%s' \"$$\" > '{}'\n\
                 sleep 0.4\n\
                 printf completed > '{}'\n\
                 printf '{{}}'\n",
                pid_path.display(),
                completion_path.display()
            ),
        )
        .expect("write fake gh");
        let mut permissions = std::fs::metadata(&fake_gh)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");
        let _override = TestGhOverride::install(fake_gh);

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let execution = tokio::spawn(async move {
            gh_issue(
                serde_json::json!({
                    "action": "create",
                    "title": "must report its terminal outcome",
                    "repository": "evalops/example"
                }),
                Some(&cancel_for_task),
            )
            .await
        });
        tokio::time::timeout(GH_TEST_PROCESS_START_TIMEOUT, async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake mutating gh command should start");
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("mutating gh command must reach its terminal response")
            .expect("gh issue task should not panic");
        assert!(result.success, "terminal success must survive cancellation");
        assert!(
            completion_path.exists(),
            "a started GitHub write must not be killed before its outcome is known"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_mutating_gh_issue_returns_bounded_indeterminate_outcome() {
        use std::os::unix::fs::PermissionsExt as _;

        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        let pid_path = workspace.path().join("api.pid");
        let completion_path = workspace.path().join("completed");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
                 printf '%s' \"$$\" > '{}'\n\
                 sleep 60\n\
                 printf completed > '{}'\n",
                pid_path.display(),
                completion_path.display()
            ),
        )
        .expect("write fake gh");
        let mut permissions = std::fs::metadata(&fake_gh)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");
        let _override = TestGhOverride::install(fake_gh);

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let execution = tokio::spawn(async move {
            gh_issue(
                serde_json::json!({
                    "action": "create",
                    "title": "must become indeterminate",
                    "repository": "evalops/example"
                }),
                Some(&cancel_for_task),
            )
            .await
        });
        tokio::time::timeout(GH_TEST_PROCESS_START_TIMEOUT, async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake mutating gh command should start");
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), execution)
            .await
            .expect("hung mutating gh command must finish within the shutdown bound")
            .expect("gh issue task should not panic");
        assert!(!result.success);
        let details = result.details.expect("indeterminate details");
        assert_eq!(details["remoteOutcome"], "unknown");
        assert_eq!(details["retryable"], false);
        assert_eq!(details["requiresReconciliation"], true);
        assert!(
            !completion_path.exists(),
            "timed-out gh process survived after indeterminate outcome"
        );
    }

    #[test]
    fn spawned_mutation_wait_error_is_indeterminate() {
        let error = classify_wait_error(std::io::Error::other("lost child status"), true);
        assert!(matches!(error, GhCommandError::Indeterminate(message)
            if message.contains("lost child status")
                && message.contains("must be reconciled before retry")));
    }

    #[test]
    fn read_only_wait_error_remains_a_failure() {
        let error = classify_wait_error(std::io::Error::other("lost child status"), false);
        assert!(matches!(error, GhCommandError::Failed(message)
            if message == "lost child status"));
    }

    #[test]
    fn mutation_terminal_output_survives_cleanup_error() {
        let error =
            cleanup_error_after_terminal(std::io::Error::other("failed to disarm job"), true);
        assert!(
            error.is_none(),
            "known mutation output must remain authoritative"
        );
    }

    #[test]
    fn read_only_cleanup_error_remains_a_failure() {
        let error =
            cleanup_error_after_terminal(std::io::Error::other("failed to disarm job"), false);
        assert!(matches!(error, Some(GhCommandError::Failed(message))
            if message == "failed to disarm job"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_cancelled_mutating_gh_issue_never_starts_a_process() {
        use std::os::unix::fs::PermissionsExt as _;

        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        let started_path = workspace.path().join("started");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 0.4\n",
                started_path.display()
            ),
        )
        .expect("write fake gh");
        let mut permissions = std::fs::metadata(&fake_gh)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");
        let _override = TestGhOverride::install(fake_gh);

        let cancel = CancellationToken::new();
        cancel.cancel();
        TEST_COMMAND_SPAWNS.store(0, std::sync::atomic::Ordering::SeqCst);
        let result = gh_issue(
            serde_json::json!({
                "action": "create",
                "title": "must never start",
                "repository": "evalops/example"
            }),
            Some(&cancel),
        )
        .await;

        assert!(!result.success);
        assert_eq!(
            result
                .details
                .as_ref()
                .and_then(|details| details.get("cancelled")),
            Some(&Value::Bool(true))
        );
        assert!(
            !started_path.exists(),
            "pre-cancelled mutating gh command was still spawned"
        );
        assert_eq!(
            TEST_COMMAND_SPAWNS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "pre-cancelled mutating gh command crossed the spawn boundary"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_subprocess_future_kills_spawned_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 60 & child=$!; echo \"$child\" > \"$1\"; wait")
            .arg("sh")
            .arg(&pid_file);

        let task = tokio::spawn(run_command_output(command, None));
        // `echo "$child" > pid` makes the file visible to `exists()` before
        // the shell writes the pid into it, so poll for a complete,
        // parseable pid rather than mere existence.
        let pid: libc::pid_t = 'published: {
            for _ in 0..100 {
                if let Ok(contents) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = contents.trim().parse() {
                        break 'published pid;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("subprocess must publish child pid");
        };

        task.abort();
        let _ = task.await;
        for _ in 0..100 {
            // SAFETY: signal 0 only probes process existence.
            if unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("grandchild process {pid} survived cancellation");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_subprocess_future_kills_spawned_job_tree() {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-Command")
            .arg(
                "$child = Start-Process powershell.exe -ArgumentList '-NoProfile', \
                 '-Command', 'Start-Sleep -Seconds 60' -PassThru; \
                 Set-Content -LiteralPath $env:MAESTRO_TEST_PID_FILE -Value $child.Id; \
                 $child.WaitForExit()",
            )
            .env("MAESTRO_TEST_PID_FILE", &pid_file);

        let task = tokio::spawn(run_command_output(command, None));
        for _ in 0..200 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("subprocess must publish child pid")
            .trim()
            .parse()
            .unwrap();

        task.abort();
        let _ = task.await;
        for _ in 0..200 {
            // SAFETY: this opens a query-only handle to the pid published by
            // the test child. A null result means the process no longer
            // exists; any live handle is closed immediately.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return;
            }
            drop(OwnedWindowsHandle(handle));
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("grandchild process {pid} survived cancellation");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn successful_subprocess_keeps_spawned_descendant_alive() {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
        };

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let stdout_file = dir.path().join("child.stdout");
        let stderr_file = dir.path().join("child.stderr");
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-Command")
            .arg(
                "$child = Start-Process powershell.exe -ArgumentList '-NoProfile', \
                 '-Command', 'Start-Sleep -Seconds 60' -RedirectStandardOutput \
                 $env:MAESTRO_TEST_STDOUT_FILE -RedirectStandardError \
                 $env:MAESTRO_TEST_STDERR_FILE -PassThru; \
                 Set-Content -LiteralPath $env:MAESTRO_TEST_PID_FILE -Value $child.Id",
            )
            .env("MAESTRO_TEST_PID_FILE", &pid_file)
            .env("MAESTRO_TEST_STDOUT_FILE", &stdout_file)
            .env("MAESTRO_TEST_STDERR_FILE", &stderr_file);

        let output = run_command_output(command, None).await.unwrap();
        assert!(output.status.success());
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("subprocess must publish child pid")
            .trim()
            .parse()
            .unwrap();

        // SAFETY: the pid was published by the successful test subprocess.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        assert!(
            !handle.is_null(),
            "successful subprocess killed its descendant"
        );
        // SAFETY: handle grants PROCESS_TERMINATE and is exclusively closed
        // by OwnedWindowsHandle below.
        assert_ne!(unsafe { TerminateProcess(handle, 0) }, 0);
        drop(OwnedWindowsHandle(handle));
    }

    // ========================================================================
    // GhPrArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_gh_pr_args_minimal() {
        let json = serde_json::json!({"action": "list"});
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "list");
        assert!(args.number.is_none());
        assert!(args.title.is_none());
        assert!(args.repository.is_none());
    }

    #[test]
    fn test_gh_pr_args_create() {
        let json = serde_json::json!({
            "action": "create",
            "title": "Add new feature",
            "body": "This PR adds...",
            "branch": "feature-branch",
            "base": "main",
            "draft": true
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "create");
        assert_eq!(args.title.unwrap(), "Add new feature");
        assert_eq!(args.body.unwrap(), "This PR adds...");
        assert_eq!(args.branch.unwrap(), "feature-branch");
        assert_eq!(args.base.unwrap(), "main");
        assert!(args.draft.unwrap());
    }

    #[test]
    fn test_gh_pr_args_with_labels() {
        let json = serde_json::json!({
            "action": "list",
            "label": ["bug", "priority"],
            "state": "open",
            "limit": 50
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "list");
        assert_eq!(args.label.unwrap(), vec!["bug", "priority"]);
        assert_eq!(args.state.unwrap(), "open");
        assert_eq!(args.limit.unwrap(), 50);
    }

    #[test]
    fn test_gh_pr_args_name_only_alias() {
        let json = serde_json::json!({
            "action": "diff",
            "number": 123,
            "nameOnly": true
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "diff");
        assert_eq!(args.number.unwrap(), 123);
        assert!(args.name_only.unwrap());
    }

    // ========================================================================
    // GhIssueArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_gh_issue_args_minimal() {
        let json = serde_json::json!({"action": "list"});
        let args: GhIssueArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "list");
        assert!(args.number.is_none());
    }

    #[test]
    fn test_gh_issue_args_create() {
        let json = serde_json::json!({
            "action": "create",
            "title": "Bug report",
            "body": "Steps to reproduce...",
            "labels": ["bug", "critical"]
        });
        let args: GhIssueArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "create");
        assert_eq!(args.title.unwrap(), "Bug report");
        assert_eq!(args.body.unwrap(), "Steps to reproduce...");
        assert_eq!(args.labels.unwrap(), vec!["bug", "critical"]);
    }

    #[test]
    fn test_gh_issue_args_with_filters() {
        let json = serde_json::json!({
            "action": "list",
            "state": "closed",
            "author": "octocat",
            "limit": 25,
            "repository": "owner/repo"
        });
        let args: GhIssueArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "list");
        assert_eq!(args.state.unwrap(), "closed");
        assert_eq!(args.author.unwrap(), "octocat");
        assert_eq!(args.limit.unwrap(), 25);
        assert_eq!(args.repository.unwrap(), "owner/repo");
    }

    // ========================================================================
    // GhRepoArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_gh_repo_args_minimal() {
        let json = serde_json::json!({"action": "view"});
        let args: GhRepoArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "view");
        assert!(args.repository.is_none());
        assert!(args.directory.is_none());
    }

    #[test]
    fn test_gh_repo_args_clone() {
        let json = serde_json::json!({
            "action": "clone",
            "repository": "owner/repo",
            "directory": "my-local-dir"
        });
        let args: GhRepoArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.action, "clone");
        assert_eq!(args.repository.unwrap(), "owner/repo");
        assert_eq!(args.directory.unwrap(), "my-local-dir");
    }

    // ========================================================================
    // append_field Tests
    // ========================================================================

    #[test]
    fn test_append_field_string() {
        let mut args = Vec::new();
        append_field(&mut args, "title", &Value::String("Hello".to_string()));
        assert_eq!(args, vec!["-f", "title=Hello"]);
    }

    #[test]
    fn test_append_field_number() {
        let mut args = Vec::new();
        append_field(&mut args, "count", &serde_json::json!(42));
        assert_eq!(args, vec!["-F", "count=42"]);
    }

    #[test]
    fn test_append_field_bool() {
        let mut args = Vec::new();
        append_field(&mut args, "draft", &Value::Bool(true));
        assert_eq!(args, vec!["-F", "draft=true"]);
    }

    #[test]
    fn test_append_field_array() {
        let mut args = Vec::new();
        append_field(
            &mut args,
            "labels",
            &serde_json::json!(["bug", "enhancement"]),
        );
        assert_eq!(
            args,
            vec!["-f", "labels[]=bug", "-f", "labels[]=enhancement"]
        );
    }

    #[test]
    fn test_append_field_null() {
        let mut args = Vec::new();
        append_field(&mut args, "optional", &Value::Null);
        assert!(args.is_empty());
    }

    #[test]
    fn test_append_field_object_ignored() {
        let mut args = Vec::new();
        append_field(
            &mut args,
            "complex",
            &serde_json::json!({"nested": "value"}),
        );
        assert!(args.is_empty());
    }

    // ========================================================================
    // Error Cases Tests
    // ========================================================================

    #[test]
    fn test_gh_pr_args_invalid_json() {
        let json = serde_json::json!({"wrong_field": "value"});
        let result: Result<GhPrArgs, _> = serde_json::from_value(json);
        // Missing required "action" field
        assert!(result.is_err());
    }

    #[test]
    fn test_gh_issue_args_invalid_json() {
        let json = serde_json::json!({"number": 123});
        let result: Result<GhIssueArgs, _> = serde_json::from_value(json);
        // Missing required "action" field
        assert!(result.is_err());
    }
}
