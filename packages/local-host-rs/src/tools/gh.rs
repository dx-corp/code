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
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent::ToolResult;

/// Output-bounding limits for `gh_pr` actions that fan out over
/// model-authored or third-party text (review threads, reviews, CI logs).
/// Mirrors the cap-and-mark-truncated pattern used by `tools/exa.rs`
/// (`MAX_OUTPUT_CHARS`) so an unattended agent can never be handed an
/// unbounded response from a PR with hundreds of comments or a noisy CI log.
mod bounds {
    /// Review threads returned by `review_threads` (GraphQL `first:`).
    pub(super) const MAX_REVIEW_THREADS: usize = 50;
    /// Comments returned per thread (GraphQL `first:`).
    pub(super) const MAX_THREAD_COMMENTS: usize = 20;
    /// Top-level review summaries returned (GraphQL `first:`).
    pub(super) const MAX_REVIEWS: usize = 20;
    /// Characters kept per review-thread comment body.
    pub(super) const MAX_COMMENT_BODY_CHARS: usize = 2000;
    /// Characters kept per top-level review body.
    pub(super) const MAX_REVIEW_BODY_CHARS: usize = 4000;
    /// Final safety net on the serialized `review_threads` output.
    pub(super) const MAX_OUTPUT_CHARS: usize = 20_000;
    /// Poll interval for `checks_watch`.
    pub(super) const CHECKS_WATCH_INTERVAL_SECS: u64 = 30;
    /// Default `checks_watch` timeout when the caller doesn't specify one.
    pub(super) const CHECKS_WATCH_DEFAULT_TIMEOUT_SECS: u64 = 900;
    /// Hard ceiling on `checks_watch` timeout regardless of caller input.
    pub(super) const CHECKS_WATCH_MAX_TIMEOUT_SECS: u64 = 3600;
    /// Failed jobs whose logs are fetched by `checks_watch`.
    pub(super) const MAX_FAILED_JOB_LOGS: usize = 3;
    /// Lines kept per failed-job log tail.
    pub(super) const MAX_LOG_TAIL_LINES: usize = 200;
    /// Bytes kept per failed-job log tail.
    pub(super) const MAX_LOG_TAIL_BYTES: usize = 16 * 1024;
}

/// Truncate `text` to at most `max_chars` UTF-8 scalar values, appending a
/// `(truncated)` marker when truncation actually happened.
fn truncate_text(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push_str("\n\n(truncated)");
    (truncated, true)
}

/// Keep the last `max_lines` lines of `text`, further bounded to
/// `max_bytes`. Used to cap CI failure logs, which can otherwise run to
/// megabytes for a single job.
fn tail_text(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut truncated_lines = lines.len() > max_lines;
    let start = lines.len().saturating_sub(max_lines);
    let mut tail = lines[start..].join("\n");
    if tail.len() > max_bytes {
        truncated_lines = true;
        // Byte-safe: walk back to a char boundary before slicing.
        let mut cut = tail.len() - max_bytes;
        while cut < tail.len() && !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = tail[cut..].to_string();
    }
    (tail, truncated_lines)
}

/// Validate a GitHub GraphQL node id used to address a pull request review
/// thread. Real ids look like `PRRT_kwDOA...`: an opaque prefix identifying
/// the node type, an underscore, then a base64url-ish payload. Reject
/// anything else up front so a malformed or hallucinated id fails with a
/// clear tool error instead of reaching `gh api graphql` as an untrusted
/// argument.
fn is_valid_review_thread_id(id: &str) -> bool {
    match id.strip_prefix("PRRT_") {
        Some(rest) => {
            !rest.is_empty()
                && rest.len() <= 128
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
        }
        None => false,
    }
}

/// Extract `(run_id, job_id)` from a check run's `details_url`, e.g.
/// `https://github.com/{owner}/{repo}/actions/runs/{run_id}/job/{job_id}`.
/// Returns `None` for check runs that aren't backed by a GitHub Actions job
/// (third-party checks apps use their own `details_url` shape).
fn parse_run_and_job_ids(details_url: &str) -> Option<(String, String)> {
    let runs_at = details_url.find("/actions/runs/")?;
    let rest = &details_url[runs_at + "/actions/runs/".len()..];
    let (run_id, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("job/")?;
    let job_id = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if run_id.chars().all(|c| c.is_ascii_digit())
        && !run_id.is_empty()
        && job_id.chars().all(|c| c.is_ascii_digit())
        && !job_id.is_empty()
    {
        Some((run_id.to_string(), job_id.to_string()))
    } else {
        None
    }
}

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
    /// GraphQL node id of a `PullRequestReviewThread`, e.g. `PRRT_kwDOA...`.
    /// Required by `reply_review_thread` and `resolve_review_thread`.
    #[serde(default, alias = "threadId")]
    thread_id: Option<String>,
    /// `review_threads`: include resolved threads too (default: unresolved
    /// only, since that's what an unattended agent needs to act on).
    #[serde(default, alias = "includeResolved")]
    include_resolved: Option<bool>,
    /// `checks_watch`: how long to poll before giving up. Defaults to 900s,
    /// clamped to a 3600s ceiling.
    #[serde(default, alias = "timeoutSecs")]
    timeout_secs: Option<u64>,
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

/// Run a `gh api graphql` query or mutation and return the decoded JSON
/// response body.
///
/// `is_mutation` controls the same cancellation policy `run_gh_api` derives
/// from the HTTP method: queries behave like a `GET` (safe to abandon on
/// cancellation) while mutations behave like a `POST` (a cancellation after
/// the process starts must await the real terminal response so a write that
/// already landed isn't reported as failed/retryable). GraphQL always
/// transports as POST at the HTTP layer, so that policy can't be inferred
/// from the method the way `run_gh_api` does.
async fn run_gh_graphql(
    query: &str,
    variables: Vec<(String, Value)>,
    gh_repo: Option<&str>,
    cancel: Option<&CancellationToken>,
    is_mutation: bool,
) -> Result<Value, GhCommandError> {
    let mut cmd = new_gh_command();
    cmd.arg("api").arg("graphql");
    cmd.arg("-f").arg(format!("query={query}"));

    let mut args: Vec<String> = Vec::new();
    for (key, value) in variables {
        append_field(&mut args, &key, &value);
    }
    if !args.is_empty() {
        cmd.args(args);
    }
    if let Some(repo) = gh_repo {
        cmd.env("GH_REPO", repo);
    }

    let output = run_command_output_with_policy(cmd, cancel, is_mutation)
        .await
        .map_err(|error| match error {
            GhCommandError::Failed(message) => {
                GhCommandError::Failed(format!("Failed to run gh api graphql: {message}"))
            }
            GhCommandError::Cancelled => GhCommandError::Cancelled,
            GhCommandError::Indeterminate(message) => GhCommandError::Indeterminate(message),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(GhCommandError::Failed(if stderr.is_empty() {
            "gh api graphql failed".to_string()
        } else {
            stderr
        }));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let json: Value = serde_json::from_str(&stdout)
        .map_err(|error| GhCommandError::Failed(format!("Invalid GraphQL response: {error}")))?;
    if let Some(errors) = json.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            let messages: Vec<String> = errors
                .iter()
                .filter_map(|error| error.get("message").and_then(Value::as_str))
                .map(std::string::ToString::to_string)
                .collect();
            return Err(GhCommandError::Failed(if messages.is_empty() {
                "GraphQL request returned errors".to_string()
            } else {
                messages.join("; ")
            }));
        }
    }
    Ok(json)
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
/// - `checks` - View CI check status once (requires `number`)
/// - `diff` - Get PR diff (requires `number`, optional `nameOnly`)
/// - `review_threads` - List PR review threads and top-level review
///   summaries (requires `number`, optional `includeResolved`). Use this
///   before acting on review feedback: it defaults to unresolved threads
///   only, with each comment's author, body, and whether the thread is
///   outdated (superseded by a later push).
/// - `reply_review_thread` - Reply inline to a specific review thread
///   (requires `threadId` from `review_threads`, and `body`). Use this to
///   answer a reviewer's comment in place instead of a general PR comment.
/// - `resolve_review_thread` - Mark a review thread resolved (requires
///   `threadId`). Only resolve a thread after actually addressing it in
///   code or in a reply; resolving without addressing it hides the feedback.
/// - `checks_watch` - Poll CI to completion instead of a one-shot snapshot
///   (requires `number`, optional `timeoutSecs`, default 900s, max 3600s).
///   Returns a per-check summary and, for any check that failed, a bounded
///   tail of its job log. Prefer this over repeated `checks` calls after
///   opening or updating a PR.
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
        "review_threads" => {
            let number = match parsed.number {
                Some(val) => val,
                None => {
                    return ToolResult::failure("number required for review_threads".to_string());
                }
            };
            let full_name = match resolve_repo_full_name(repo, cancel).await {
                Ok(name) => name,
                Err(err) => return gh_error_result(err),
            };
            let Some((owner, repo_name)) = full_name.split_once('/') else {
                return ToolResult::failure(format!("Unexpected repository name '{full_name}'"));
            };
            let include_resolved = parsed.include_resolved.unwrap_or(false);
            let query = review_threads_query();
            let variables = vec![
                ("owner".to_string(), Value::String(owner.to_string())),
                ("repo".to_string(), Value::String(repo_name.to_string())),
                ("number".to_string(), Value::Number(number.into())),
            ];
            let response = match run_gh_graphql(&query, variables, repo, cancel, false).await {
                Ok(json) => json,
                Err(err) => return gh_error_result(err),
            };
            let Some(pr) = response.pointer("/data/repository/pullRequest") else {
                return ToolResult::failure(
                    "GraphQL response missing repository.pullRequest".to_string(),
                );
            };
            build_review_threads_result(pr, include_resolved)
        }
        "reply_review_thread" => {
            let thread_id = match parsed.thread_id.as_deref() {
                Some(val) if is_valid_review_thread_id(val) => val.to_string(),
                Some(_) => {
                    return ToolResult::failure(
                        "threadId is not a valid PullRequestReviewThread id (expected PRRT_...)"
                            .to_string(),
                    );
                }
                None => {
                    return ToolResult::failure(
                        "threadId required for reply_review_thread".to_string(),
                    );
                }
            };
            let body = match parsed.body {
                Some(val) => val,
                None => {
                    return ToolResult::failure(
                        "body required for reply_review_thread".to_string(),
                    );
                }
            };
            let variables = vec![
                ("threadId".to_string(), Value::String(thread_id)),
                ("body".to_string(), Value::String(body)),
            ];
            match run_gh_graphql(
                reply_review_thread_mutation(),
                variables,
                repo,
                cancel,
                true,
            )
            .await
            {
                Ok(json) => ToolResult::success(
                    serde_json::to_string_pretty(
                        json.pointer("/data/addPullRequestReviewThreadReply/comment")
                            .unwrap_or(&Value::Null),
                    )
                    .unwrap_or_else(|_| json.to_string()),
                ),
                Err(err) => gh_error_result(err),
            }
        }
        "resolve_review_thread" => {
            let thread_id = match parsed.thread_id.as_deref() {
                Some(val) if is_valid_review_thread_id(val) => val.to_string(),
                Some(_) => {
                    return ToolResult::failure(
                        "threadId is not a valid PullRequestReviewThread id (expected PRRT_...)"
                            .to_string(),
                    );
                }
                None => {
                    return ToolResult::failure(
                        "threadId required for resolve_review_thread".to_string(),
                    );
                }
            };
            let variables = vec![("threadId".to_string(), Value::String(thread_id))];
            match run_gh_graphql(
                resolve_review_thread_mutation(),
                variables,
                repo,
                cancel,
                true,
            )
            .await
            {
                Ok(json) => ToolResult::success(
                    serde_json::to_string_pretty(
                        json.pointer("/data/resolveReviewThread/thread")
                            .unwrap_or(&Value::Null),
                    )
                    .unwrap_or_else(|_| json.to_string()),
                ),
                Err(err) => gh_error_result(err),
            }
        }
        "checks_watch" => {
            let number = match parsed.number {
                Some(val) => val,
                None => return ToolResult::failure("number required for checks_watch".to_string()),
            };
            let timeout_secs = clamp_checks_watch_timeout(parsed.timeout_secs);
            watch_checks(number, timeout_secs, repo, cancel).await
        }
        _ => ToolResult::failure("Unsupported gh_pr action".to_string()),
    };
    with_repo_origin(result, repo)
}

/// Build the bounded GraphQL query used by the `review_threads` action.
/// Factored out from the `gh_pr` dispatch so its shape (field selection and
/// `first:` bounds) can be asserted directly in tests.
fn review_threads_query() -> String {
    format!(
        "query($owner: String!, $repo: String!, $number: Int!) {{ \
           repository(owner: $owner, name: $repo) {{ \
             pullRequest(number: $number) {{ \
               reviews(first: {max_reviews}) {{ \
                 totalCount \
                 nodes {{ author {{ login }} state body submittedAt }} \
               }} \
               reviewThreads(first: {max_threads}) {{ \
                 totalCount \
                 nodes {{ \
                   id isResolved isOutdated path line \
                   comments(first: {max_comments}) {{ \
                     totalCount \
                     nodes {{ author {{ login }} body createdAt }} \
                   }} \
                 }} \
               }} \
             }} \
           }} \
         }}",
        max_reviews = bounds::MAX_REVIEWS,
        max_threads = bounds::MAX_REVIEW_THREADS,
        max_comments = bounds::MAX_THREAD_COMMENTS,
    )
}

/// GraphQL mutation used by the `reply_review_thread` action.
fn reply_review_thread_mutation() -> &'static str {
    "mutation($threadId: ID!, $body: String!) { \
        addPullRequestReviewThreadReply(input: { \
          pullRequestReviewThreadId: $threadId, body: $body \
        }) { \
          comment { id body createdAt author { login } } \
        } \
      }"
}

/// GraphQL mutation used by the `resolve_review_thread` action.
fn resolve_review_thread_mutation() -> &'static str {
    "mutation($threadId: ID!) { \
        resolveReviewThread(input: { threadId: $threadId }) { \
          thread { id isResolved } \
        } \
      }"
}

/// Clamp a caller-supplied `checks_watch` timeout into `[1,
/// CHECKS_WATCH_MAX_TIMEOUT_SECS]`, defaulting to
/// `CHECKS_WATCH_DEFAULT_TIMEOUT_SECS` when unset. A caller cannot request
/// an unbounded or zero-length poll.
fn clamp_checks_watch_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(bounds::CHECKS_WATCH_DEFAULT_TIMEOUT_SECS)
        .clamp(1, bounds::CHECKS_WATCH_MAX_TIMEOUT_SECS)
}

/// Shape the GraphQL `pullRequest` payload from `review_threads` into the
/// bounded, model-facing summary: unresolved (by default) review threads
/// with their comments, plus top-level review state summaries.
fn build_review_threads_result(pr: &Value, include_resolved: bool) -> ToolResult {
    let mut truncated_any = false;

    let reviews_total = pr
        .pointer("/reviews/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let review_nodes = pr
        .pointer("/reviews/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if reviews_total > review_nodes.len() as u64 {
        truncated_any = true;
    }
    let reviews: Vec<Value> = review_nodes
        .into_iter()
        .map(|review| {
            let (body, body_truncated) = truncate_text(
                review.get("body").and_then(Value::as_str).unwrap_or(""),
                bounds::MAX_REVIEW_BODY_CHARS,
            );
            truncated_any |= body_truncated;
            serde_json::json!({
                "author": review.pointer("/author/login").and_then(Value::as_str),
                "state": review.get("state").and_then(Value::as_str),
                "body": body,
                "submittedAt": review.get("submittedAt").and_then(Value::as_str),
            })
        })
        .collect();

    let threads_total = pr
        .pointer("/reviewThreads/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thread_nodes = pr
        .pointer("/reviewThreads/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if threads_total > thread_nodes.len() as u64 {
        truncated_any = true;
    }

    let threads: Vec<Value> = thread_nodes
        .into_iter()
        .filter(|thread| {
            include_resolved
                || !thread
                    .get("isResolved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
        .map(|thread| {
            let comments_total = thread
                .pointer("/comments/totalCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let comment_nodes = thread
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let comments_truncated = comments_total > comment_nodes.len() as u64;
            truncated_any |= comments_truncated;
            let comments: Vec<Value> = comment_nodes
                .into_iter()
                .map(|comment| {
                    let (body, body_truncated) = truncate_text(
                        comment.get("body").and_then(Value::as_str).unwrap_or(""),
                        bounds::MAX_COMMENT_BODY_CHARS,
                    );
                    truncated_any |= body_truncated;
                    serde_json::json!({
                        "author": comment.pointer("/author/login").and_then(Value::as_str),
                        "body": body,
                        "createdAt": comment.get("createdAt").and_then(Value::as_str),
                    })
                })
                .collect();
            serde_json::json!({
                "id": thread.get("id").and_then(Value::as_str),
                "path": thread.get("path").and_then(Value::as_str),
                "line": thread.get("line"),
                "isResolved": thread.get("isResolved").and_then(Value::as_bool).unwrap_or(false),
                "isOutdated": thread.get("isOutdated").and_then(Value::as_bool).unwrap_or(false),
                "comments": comments,
                "commentsTruncated": comments_truncated,
            })
        })
        .collect();

    let payload = serde_json::json!({
        "reviews": reviews,
        "threads": threads,
        "includeResolved": include_resolved,
    });
    let rendered = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    let (output, output_truncated) = truncate_text(&rendered, bounds::MAX_OUTPUT_CHARS);
    truncated_any |= output_truncated;

    let mut result = ToolResult::success(output);
    if truncated_any {
        result = result.with_details(serde_json::json!({ "truncated": true }));
    }
    result
}

/// Poll `repos/{owner}/{repo}/commits/{sha}/check-runs` for PR `number`
/// until every check reports `status: "completed"`, `timeout_secs` elapses,
/// or `cancel` fires. Returns a per-check summary and, for failed checks, a
/// bounded tail of the job's failure log.
async fn watch_checks(
    number: u64,
    timeout_secs: u64,
    repo: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> ToolResult {
    watch_checks_with_interval(
        number,
        timeout_secs,
        Duration::from_secs(bounds::CHECKS_WATCH_INTERVAL_SECS),
        repo,
        cancel,
    )
    .await
}

/// `watch_checks` with an injectable poll interval so tests can exercise
/// the completion/timeout state machine against a fake `gh` command runner
/// without waiting on the real 30s production interval.
async fn watch_checks_with_interval(
    number: u64,
    timeout_secs: u64,
    interval: Duration,
    repo: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> ToolResult {
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
    let pr_json: Value = match serde_json::from_str(&pr_output) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid PR response: {err}")),
    };
    let Some(sha) = pr_json
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
    else {
        return ToolResult::failure("Missing PR head sha".to_string());
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut check_runs: Vec<Value>;
    let mut timed_out = false;

    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return gh_error_result(GhCommandError::Cancelled);
        }
        let checks_output = match run_gh_api(
            &format!("repos/{{owner}}/{{repo}}/commits/{sha}/check-runs"),
            "GET",
            vec![("per_page".to_string(), Value::Number(100.into()))],
            Vec::new(),
            repo,
            cancel,
        )
        .await
        {
            Ok(output) => output,
            Err(err) => return gh_error_result(err),
        };
        let checks_json: Value = match serde_json::from_str(&checks_output) {
            Ok(val) => val,
            Err(err) => return ToolResult::failure(format!("Invalid check-runs response: {err}")),
        };
        check_runs = checks_json
            .get("check_runs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let all_completed = !check_runs.is_empty()
            && check_runs
                .iter()
                .all(|run| run.get("status").and_then(Value::as_str) == Some("completed"));
        if all_completed {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }

        match cancel {
            Some(token) => {
                tokio::select! {
                    biased;
                    () = token.cancelled() => return gh_error_result(GhCommandError::Cancelled),
                    () = tokio::time::sleep(interval) => {}
                }
            }
            None => tokio::time::sleep(interval).await,
        }
    }

    let mut failed_logs: Vec<Value> = Vec::new();
    let failed_runs = check_runs.iter().filter(|run| {
        run.get("status").and_then(Value::as_str) == Some("completed")
            && !matches!(
                run.get("conclusion").and_then(Value::as_str),
                Some("success" | "neutral" | "skipped")
            )
    });
    for run in failed_runs.take(bounds::MAX_FAILED_JOB_LOGS) {
        let name = run
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let Some((run_id, job_id)) = run
            .get("details_url")
            .and_then(Value::as_str)
            .and_then(parse_run_and_job_ids)
        else {
            continue;
        };
        let mut cmd = new_gh_command();
        cmd.arg("run")
            .arg("view")
            .arg(&run_id)
            .arg("--job")
            .arg(&job_id)
            .arg("--log-failed");
        if let Some(repo) = repo {
            cmd.env("GH_REPO", repo);
        }
        match run_command_output(cmd, cancel).await {
            Ok(output) if output.status.success() => {
                let raw = String::from_utf8_lossy(&output.stdout).to_string();
                let (tail, truncated) =
                    tail_text(&raw, bounds::MAX_LOG_TAIL_LINES, bounds::MAX_LOG_TAIL_BYTES);
                failed_logs.push(serde_json::json!({
                    "name": name,
                    "runId": run_id,
                    "jobId": job_id,
                    "log": tail,
                    "truncated": truncated,
                }));
            }
            Err(GhCommandError::Cancelled) => return gh_error_result(GhCommandError::Cancelled),
            Ok(_) | Err(_) => {
                // A missing/expired log (or a non-zero `gh run view` exit)
                // must not fail the whole watch; the per-check summary
                // already reports the failure.
                failed_logs.push(serde_json::json!({
                    "name": name,
                    "runId": run_id,
                    "jobId": job_id,
                    "log": Value::Null,
                    "truncated": false,
                }));
            }
        }
    }

    let summary: Vec<Value> = check_runs
        .iter()
        .map(|run| {
            serde_json::json!({
                "name": run.get("name").and_then(Value::as_str),
                "status": run.get("status").and_then(Value::as_str),
                "conclusion": run.get("conclusion").and_then(Value::as_str),
                "detailsUrl": run.get("details_url").and_then(Value::as_str),
            })
        })
        .collect();
    let all_completed = !check_runs.is_empty()
        && check_runs
            .iter()
            .all(|run| run.get("status").and_then(Value::as_str) == Some("completed"));
    let all_succeeded = all_completed
        && check_runs.iter().all(|run| {
            matches!(
                run.get("conclusion").and_then(Value::as_str),
                Some("success" | "neutral" | "skipped")
            )
        });

    let payload = serde_json::json!({
        "complete": all_completed,
        "timedOut": timed_out,
        "allSucceeded": all_succeeded,
        "checks": summary,
        "failedLogs": failed_logs,
    });
    // The loop above only ever exits via `all_completed` (success) or
    // `timed_out` (failure); `all_completed` is recomputed here from the
    // last fetched `check_runs` rather than threaded out of the loop, but
    // the two conditions are exhaustive and mutually exclusive by
    // construction.
    let output = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    if all_completed {
        ToolResult::success(output)
    } else {
        ToolResult::failure(format!(
            "checks_watch timed out after {timeout_secs}s before all checks completed"
        ))
        .with_details(payload)
    }
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

    // ========================================================================
    // review_threads / reply_review_thread / resolve_review_thread /
    // checks_watch -- argument validation
    // ========================================================================

    #[test]
    fn test_gh_pr_args_thread_id_snake_case() {
        let json = serde_json::json!({
            "action": "resolve_review_thread",
            "thread_id": "PRRT_kwDOA1234"
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.thread_id.unwrap(), "PRRT_kwDOA1234");
    }

    #[test]
    fn test_gh_pr_args_thread_id_camel_case_alias() {
        let json = serde_json::json!({
            "action": "resolve_review_thread",
            "threadId": "PRRT_kwDOA1234"
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.thread_id.unwrap(), "PRRT_kwDOA1234");
    }

    #[test]
    fn test_gh_pr_args_include_resolved_alias() {
        let json = serde_json::json!({
            "action": "review_threads",
            "number": 5,
            "includeResolved": true
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.include_resolved, Some(true));
    }

    #[test]
    fn test_gh_pr_args_timeout_secs_alias() {
        let json = serde_json::json!({
            "action": "checks_watch",
            "number": 5,
            "timeoutSecs": 120
        });
        let args: GhPrArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.timeout_secs, Some(120));
    }

    #[test]
    fn test_gh_pr_args_number_must_be_numeric() {
        let json = serde_json::json!({
            "action": "checks_watch",
            "number": "not-a-number"
        });
        let result: Result<GhPrArgs, _> = serde_json::from_value(json);
        assert!(result.is_err(), "a string PR number must fail closed");
    }

    // ========================================================================
    // is_valid_review_thread_id
    // ========================================================================

    #[test]
    fn test_review_thread_id_accepts_real_shape() {
        assert!(is_valid_review_thread_id("PRRT_kwDOE5NpvM5okADy"));
    }

    #[test]
    fn test_review_thread_id_rejects_missing_prefix() {
        assert!(!is_valid_review_thread_id("kwDOE5NpvM5okADy"));
        assert!(!is_valid_review_thread_id(
            "MDE3OlB1bGxSZXF1ZXN0UmV2aWV3VGhyZWFkMTIzNDU2Nzg="
        ));
    }

    #[test]
    fn test_review_thread_id_rejects_empty_suffix() {
        assert!(!is_valid_review_thread_id("PRRT_"));
    }

    #[test]
    fn test_review_thread_id_rejects_shell_metacharacters() {
        assert!(!is_valid_review_thread_id("PRRT_abc; rm -rf /"));
        assert!(!is_valid_review_thread_id("PRRT_abc$(whoami)"));
        assert!(!is_valid_review_thread_id("PRRT_abc/../etc"));
        assert!(!is_valid_review_thread_id("PRRT_ abc"));
    }

    #[test]
    fn test_review_thread_id_rejects_oversized_suffix() {
        let oversized = format!("PRRT_{}", "a".repeat(200));
        assert!(!is_valid_review_thread_id(&oversized));
    }

    #[test]
    fn test_review_thread_id_accepts_base64url_and_padding_chars() {
        assert!(is_valid_review_thread_id("PRRT_A-Za_0-9="));
    }

    // ========================================================================
    // GraphQL query/mutation construction
    // ========================================================================

    #[test]
    fn test_review_threads_query_is_bounded_and_shaped() {
        let query = review_threads_query();
        assert!(query.contains("$owner: String!"));
        assert!(query.contains("$repo: String!"));
        assert!(query.contains("$number: Int!"));
        assert!(query.contains("reviewThreads(first: 50)"));
        assert!(query.contains("reviews(first: 20)"));
        assert!(query.contains("comments(first: 20)"));
        assert!(query.contains("isResolved"));
        assert!(query.contains("isOutdated"));
        assert!(query.contains("path"));
        assert!(query.contains("line"));
        assert!(query.contains("createdAt"));
        assert!(query.contains("submittedAt"));
        assert!(query.contains("state"));
    }

    #[test]
    fn test_reply_review_thread_mutation_shape() {
        let mutation = reply_review_thread_mutation();
        assert!(mutation.contains("addPullRequestReviewThreadReply"));
        assert!(mutation.contains("pullRequestReviewThreadId: $threadId"));
        assert!(mutation.contains("body: $body"));
    }

    #[test]
    fn test_resolve_review_thread_mutation_shape() {
        let mutation = resolve_review_thread_mutation();
        assert!(mutation.contains("resolveReviewThread"));
        assert!(mutation.contains("threadId: $threadId"));
        assert!(mutation.contains("isResolved"));
    }

    // ========================================================================
    // clamp_checks_watch_timeout
    // ========================================================================

    #[test]
    fn test_clamp_checks_watch_timeout_defaults_to_900() {
        assert_eq!(clamp_checks_watch_timeout(None), 900);
    }

    #[test]
    fn test_clamp_checks_watch_timeout_caps_at_3600() {
        assert_eq!(clamp_checks_watch_timeout(Some(1_000_000)), 3600);
    }

    #[test]
    fn test_clamp_checks_watch_timeout_rejects_zero() {
        assert_eq!(clamp_checks_watch_timeout(Some(0)), 1);
    }

    #[test]
    fn test_clamp_checks_watch_timeout_passes_through_in_range() {
        assert_eq!(clamp_checks_watch_timeout(Some(120)), 120);
    }

    // ========================================================================
    // truncate_text / tail_text
    // ========================================================================

    #[test]
    fn test_truncate_text_no_op_under_limit() {
        let (text, truncated) = truncate_text("short", 100);
        assert_eq!(text, "short");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_text_marks_truncated_over_limit() {
        let (text, truncated) = truncate_text(&"a".repeat(50), 10);
        assert!(truncated);
        assert!(text.starts_with(&"a".repeat(10)));
        assert!(text.contains("(truncated)"));
    }

    #[test]
    fn test_tail_text_keeps_last_n_lines() {
        let input: String = (1..=10)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (tail, truncated) = tail_text(&input, 3, 10_000);
        assert!(truncated);
        assert_eq!(tail, "line-8\nline-9\nline-10");
    }

    #[test]
    fn test_tail_text_enforces_byte_cap() {
        let input = "x".repeat(1000);
        let (tail, truncated) = tail_text(&input, 10_000, 100);
        assert!(truncated);
        assert_eq!(tail.len(), 100);
    }

    #[test]
    fn test_tail_text_no_op_under_both_limits() {
        let input = "a\nb\nc";
        let (tail, truncated) = tail_text(input, 10, 100);
        assert_eq!(tail, "a\nb\nc");
        assert!(!truncated);
    }

    // ========================================================================
    // parse_run_and_job_ids
    // ========================================================================

    #[test]
    fn test_parse_run_and_job_ids_extracts_ids() {
        let url = "https://github.com/acme/widget/actions/runs/1234567/job/9876543";
        assert_eq!(
            parse_run_and_job_ids(url),
            Some(("1234567".to_string(), "9876543".to_string()))
        );
    }

    #[test]
    fn test_parse_run_and_job_ids_rejects_non_actions_url() {
        assert_eq!(
            parse_run_and_job_ids("https://example.com/some/other/path"),
            None
        );
    }

    #[test]
    fn test_parse_run_and_job_ids_rejects_non_numeric_ids() {
        assert_eq!(
            parse_run_and_job_ids("https://github.com/acme/widget/actions/runs/abc/job/def"),
            None
        );
    }

    #[test]
    fn test_parse_run_and_job_ids_ignores_trailing_query() {
        let url = "https://github.com/acme/widget/actions/runs/111/job/222?pr=1";
        assert_eq!(
            parse_run_and_job_ids(url),
            Some(("111".to_string(), "222".to_string()))
        );
    }

    // ========================================================================
    // build_review_threads_result -- output parsing from fixture JSON
    // ========================================================================

    fn review_threads_fixture() -> Value {
        serde_json::json!({
            "reviews": {
                "totalCount": 1,
                "nodes": [
                    {
                        "author": {"login": "reviewer1"},
                        "state": "CHANGES_REQUESTED",
                        "body": "Please fix the error handling.",
                        "submittedAt": "2026-09-01T00:00:00Z"
                    }
                ]
            },
            "reviewThreads": {
                "totalCount": 2,
                "nodes": [
                    {
                        "id": "PRRT_unresolved1",
                        "isResolved": false,
                        "isOutdated": false,
                        "path": "src/lib.rs",
                        "line": 42,
                        "comments": {
                            "totalCount": 1,
                            "nodes": [
                                {
                                    "author": {"login": "reviewer1"},
                                    "body": "This needs a null check.",
                                    "createdAt": "2026-09-01T00:00:00Z"
                                }
                            ]
                        }
                    },
                    {
                        "id": "PRRT_resolved1",
                        "isResolved": true,
                        "isOutdated": true,
                        "path": "src/main.rs",
                        "line": 7,
                        "comments": {
                            "totalCount": 1,
                            "nodes": [
                                {
                                    "author": {"login": "reviewer2"},
                                    "body": "Already fixed, thanks.",
                                    "createdAt": "2026-09-02T00:00:00Z"
                                }
                            ]
                        }
                    }
                ]
            }
        })
    }

    #[test]
    fn test_build_review_threads_result_filters_resolved_by_default() {
        let fixture = review_threads_fixture();
        let result = build_review_threads_result(&fixture, false);
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        let threads = payload["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["id"], "PRRT_unresolved1");
        assert_eq!(threads[0]["isResolved"], false);
    }

    #[test]
    fn test_build_review_threads_result_includes_resolved_when_requested() {
        let fixture = review_threads_fixture();
        let result = build_review_threads_result(&fixture, true);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        let threads = payload["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 2);
    }

    #[test]
    fn test_build_review_threads_result_marks_outdated() {
        let fixture = review_threads_fixture();
        let result = build_review_threads_result(&fixture, true);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        let threads = payload["threads"].as_array().unwrap();
        let resolved_thread = threads
            .iter()
            .find(|t| t["id"] == "PRRT_resolved1")
            .unwrap();
        assert_eq!(resolved_thread["isOutdated"], true);
        let unresolved_thread = threads
            .iter()
            .find(|t| t["id"] == "PRRT_unresolved1")
            .unwrap();
        assert_eq!(unresolved_thread["isOutdated"], false);
    }

    #[test]
    fn test_build_review_threads_result_reports_review_summaries() {
        let fixture = review_threads_fixture();
        let result = build_review_threads_result(&fixture, false);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        let reviews = payload["reviews"].as_array().unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["author"], "reviewer1");
        assert_eq!(reviews[0]["state"], "CHANGES_REQUESTED");
        assert_eq!(reviews[0]["body"], "Please fix the error handling.");
    }

    #[test]
    fn test_build_review_threads_result_truncates_long_comment_body() {
        let mut fixture = review_threads_fixture();
        let long_body = "x".repeat(bounds::MAX_COMMENT_BODY_CHARS + 500);
        fixture["reviewThreads"]["nodes"][0]["comments"]["nodes"][0]["body"] =
            Value::String(long_body);
        let result = build_review_threads_result(&fixture, false);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        let comment_body = payload["threads"][0]["comments"][0]["body"]
            .as_str()
            .unwrap();
        assert!(comment_body.len() < bounds::MAX_COMMENT_BODY_CHARS + 500);
        assert!(comment_body.contains("(truncated)"));
        assert_eq!(
            result
                .details
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn test_build_review_threads_result_marks_comments_truncated_when_more_exist() {
        let mut fixture = review_threads_fixture();
        fixture["reviewThreads"]["nodes"][0]["comments"]["totalCount"] = Value::from(5);
        let result = build_review_threads_result(&fixture, false);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["threads"][0]["commentsTruncated"], true);
    }

    #[test]
    fn test_build_review_threads_result_handles_empty_threads() {
        let fixture = serde_json::json!({
            "reviews": {"totalCount": 0, "nodes": []},
            "reviewThreads": {"totalCount": 0, "nodes": []}
        });
        let result = build_review_threads_result(&fixture, false);
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["threads"].as_array().unwrap().len(), 0);
        assert_eq!(payload["reviews"].as_array().unwrap().len(), 0);
    }

    // ========================================================================
    // gh_pr dispatch -- process-backed integration tests (unix only, mirrors
    // the existing TestGhOverride/TEST_GH_OVERRIDE_LOCK convention above)
    // ========================================================================

    #[cfg(unix)]
    #[tokio::test]
    async fn gh_pr_review_threads_requires_number() {
        use std::os::unix::fs::PermissionsExt as _;

        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        let reached_path = workspace.path().join("reached-api");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
                 printf reached > '{}'\n\
                 echo '{{}}'\n",
                reached_path.display()
            ),
        )
        .expect("write fake gh");
        let mut permissions = std::fs::metadata(&fake_gh)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");
        let _override = TestGhOverride::install(fake_gh);

        let result = gh_pr(
            serde_json::json!({"action": "review_threads"}),
            workspace.path().to_str().unwrap(),
            None,
        )
        .await;

        assert!(!result.success);
        assert!(
            !reached_path.exists(),
            "missing number must fail before any gh api call"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gh_pr_reply_review_thread_rejects_malformed_thread_id() {
        use std::os::unix::fs::PermissionsExt as _;

        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        let reached_path = workspace.path().join("reached-api");
        std::fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
                 printf reached > '{}'\n\
                 echo '{{}}'\n",
                reached_path.display()
            ),
        )
        .expect("write fake gh");
        let mut permissions = std::fs::metadata(&fake_gh)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_gh, permissions).expect("make fake gh executable");
        let _override = TestGhOverride::install(fake_gh);

        let result = gh_pr(
            serde_json::json!({
                "action": "reply_review_thread",
                "threadId": "not-a-real-id",
                "body": "thanks"
            }),
            workspace.path().to_str().unwrap(),
            None,
        )
        .await;

        assert!(!result.success);
        assert!(
            !reached_path.exists(),
            "an invalid threadId must fail before any gh api call"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gh_pr_resolve_review_thread_requires_thread_id() {
        let workspace = tempfile::tempdir().expect("workspace");
        let result = gh_pr(
            serde_json::json!({"action": "resolve_review_thread"}),
            workspace.path().to_str().unwrap(),
            None,
        )
        .await;
        // ensure_gh_available runs first and will fail if the real `gh` is
        // absent from PATH in this environment, but either way the tool
        // must not report success without a threadId.
        assert!(!result.success);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_mutating_reply_review_thread_returns_bounded_indeterminate_outcome() {
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
        let workspace_path = workspace.path().to_str().unwrap().to_string();
        let execution = tokio::spawn(async move {
            gh_pr(
                serde_json::json!({
                    "action": "reply_review_thread",
                    "threadId": "PRRT_kwDOA1234",
                    "body": "must become indeterminate"
                }),
                &workspace_path,
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
        .expect("fake mutating gh graphql call should start");
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), execution)
            .await
            .expect("hung mutating gh_pr call must finish within the shutdown bound")
            .expect("gh_pr task should not panic");
        assert!(!result.success);
        let details = result.details.expect("indeterminate details");
        assert_eq!(details["remoteOutcome"], "unknown");
        assert_eq!(details["requiresReconciliation"], true);
        assert!(
            !completion_path.exists(),
            "timed-out gh graphql mutation survived after indeterminate outcome"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_review_threads_query_is_plain_cancelled_not_indeterminate() {
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
                 case \"$*\" in\n\
                 *graphql*) printf '%s' \"$$\" > '{pid}'; sleep 60; printf completed > '{done}';;\n\
                 *) echo '{{\"full_name\":\"acme/widget\"}}';;\n\
                 esac\n",
                pid = pid_path.display(),
                done = completion_path.display()
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
        let workspace_path = workspace.path().to_str().unwrap().to_string();
        let execution = tokio::spawn(async move {
            gh_pr(
                serde_json::json!({"action": "review_threads", "number": 7}),
                &workspace_path,
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
        .expect("fake gh graphql query should start");
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), execution)
            .await
            .expect("hung review_threads query must finish within the shutdown bound")
            .expect("gh_pr task should not panic");
        assert!(!result.success);
        let details = result.details.expect("cancellation details");
        assert_eq!(details["cancelled"], true);
        assert!(
            details.get("requiresReconciliation").is_none(),
            "a read-only query must not be reported as needing reconciliation"
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !completion_path.exists(),
            "cancelled read-only query survived and mutated after shutdown"
        );
    }

    // ========================================================================
    // checks_watch -- completion/timeout against a fake command runner
    // ========================================================================

    #[cfg(unix)]
    fn write_executable_script(path: &std::path::Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, contents).expect("write fake gh script");
        let mut permissions = std::fs::metadata(path)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("make fake gh executable");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_watch_completes_once_status_transitions_and_fetches_failed_log() {
        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        let count_path = workspace.path().join("check-runs.count");
        write_executable_script(
            &fake_gh,
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
                 if [ \"$1\" = \"run\" ]; then\n\
                 for i in $(seq 1 300); do echo \"log line $i\"; done\n\
                 exit 0\n\
                 fi\n\
                 case \"$2\" in\n\
                 *check-runs*)\n\
                   count=$(cat '{count}' 2>/dev/null || echo 0)\n\
                   count=$((count+1))\n\
                   echo \"$count\" > '{count}'\n\
                   if [ \"$count\" -lt 3 ]; then\n\
                     echo '{{\"total_count\":1,\"check_runs\":[{{\"id\":1,\"name\":\"build\",\"status\":\"in_progress\",\"conclusion\":null,\"details_url\":\"https://github.com/acme/widget/actions/runs/111/job/222\"}}]}}'\n\
                   else\n\
                     echo '{{\"total_count\":1,\"check_runs\":[{{\"id\":1,\"name\":\"build\",\"status\":\"completed\",\"conclusion\":\"failure\",\"details_url\":\"https://github.com/acme/widget/actions/runs/111/job/222\"}}]}}'\n\
                   fi\n\
                   ;;\n\
                 *)\n\
                   echo '{{\"head\":{{\"sha\":\"deadbeefsha\"}}}}'\n\
                   ;;\n\
                 esac\n",
                count = count_path.display(),
            ),
        );
        let _override = TestGhOverride::install(fake_gh);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            watch_checks_with_interval(42, 60, Duration::from_millis(20), None, None),
        )
        .await
        .expect("checks_watch must not hang against a fake command runner");

        // The watch itself succeeded (it reached a final, non-timed-out
        // answer); the fact that a check failed is data in the payload, not
        // a tool error -- the same convention the existing one-shot
        // `checks` action uses (it reports success and lets the model read
        // the conclusions out of the body).
        assert!(
            result.success,
            "reaching a final answer is a tool success even when a check failed: {:?}",
            result.error
        );
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["complete"], true);
        assert_eq!(payload["timedOut"], false);
        assert_eq!(payload["allSucceeded"], false);
        let checks = payload["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0]["status"], "completed");
        assert_eq!(checks[0]["conclusion"], "failure");
        let logs = payload["failedLogs"].as_array().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["runId"], "111");
        assert_eq!(logs[0]["jobId"], "222");
        let log_text = logs[0]["log"].as_str().unwrap();
        assert!(log_text.lines().count() <= 200);
        assert!(log_text.contains("log line 300"));
        assert!(!log_text.contains("log line 1\n"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_watch_times_out_when_checks_never_complete() {
        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        write_executable_script(
            &fake_gh,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
             case \"$2\" in\n\
             *check-runs*)\n\
               echo '{\"total_count\":1,\"check_runs\":[{\"id\":1,\"name\":\"build\",\"status\":\"in_progress\",\"conclusion\":null,\"details_url\":null}]}'\n\
               ;;\n\
             *)\n\
               echo '{\"head\":{\"sha\":\"deadbeefsha\"}}'\n\
               ;;\n\
             esac\n",
        );
        let _override = TestGhOverride::install(fake_gh);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            watch_checks_with_interval(42, 0, Duration::from_millis(20), None, None),
        )
        .await
        .expect("checks_watch must time out rather than hang forever");

        assert!(!result.success);
        assert!(
            result
                .error
                .as_ref()
                .is_some_and(|message| message.contains("timed out")),
            "{:?}",
            result.error
        );
        let details = result.details.expect("checks_watch timeout details");
        assert_eq!(details["complete"], false);
        assert_eq!(details["timedOut"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_watch_succeeds_when_all_checks_pass() {
        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        write_executable_script(
            &fake_gh,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
             case \"$2\" in\n\
             *check-runs*)\n\
               echo '{\"total_count\":2,\"check_runs\":[{\"id\":1,\"name\":\"build\",\"status\":\"completed\",\"conclusion\":\"success\"},{\"id\":2,\"name\":\"lint\",\"status\":\"completed\",\"conclusion\":\"neutral\"}]}'\n\
               ;;\n\
             *)\n\
               echo '{\"head\":{\"sha\":\"deadbeefsha\"}}'\n\
               ;;\n\
             esac\n",
        );
        let _override = TestGhOverride::install(fake_gh);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            watch_checks_with_interval(42, 60, Duration::from_millis(20), None, None),
        )
        .await
        .expect("checks_watch must not hang against a fake command runner");

        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["complete"], true);
        assert_eq!(payload["allSucceeded"], true);
        assert_eq!(payload["failedLogs"].as_array().unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_watch_cancellation_stops_polling() {
        let _override_lock = TEST_GH_OVERRIDE_LOCK.lock().await;
        let workspace = tempfile::tempdir().expect("workspace");
        let fake_gh = workspace.path().join("gh");
        write_executable_script(
            &fake_gh,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo 'gh version test'; exit 0; fi\n\
             case \"$2\" in\n\
             *check-runs*)\n\
               echo '{\"total_count\":1,\"check_runs\":[{\"id\":1,\"name\":\"build\",\"status\":\"in_progress\",\"conclusion\":null}]}'\n\
               ;;\n\
             *)\n\
               echo '{\"head\":{\"sha\":\"deadbeefsha\"}}'\n\
               ;;\n\
             esac\n",
        );
        let _override = TestGhOverride::install(fake_gh);

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let execution = tokio::spawn(async move {
            watch_checks_with_interval(
                42,
                600,
                Duration::from_millis(50),
                None,
                Some(&cancel_for_task),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), execution)
            .await
            .expect("cancelled checks_watch must stop within the shutdown bound")
            .expect("checks_watch task should not panic");
        assert!(!result.success);
        assert_eq!(
            result
                .details
                .as_ref()
                .and_then(|d| d.get("cancelled"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }
}
