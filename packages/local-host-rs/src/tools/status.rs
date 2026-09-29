//! Git status tool helper.
//!
//! This module provides a wrapper around `git status --porcelain=v2` that parses
//! the output into structured data. It extracts:
//!
//! - Branch information (head, upstream, ahead/behind counts)
//! - File counts (modified, added, deleted, untracked, ignored)
//!
//! # Options
//!
//! - `branch_summary` - Include branch information (default: true)
//! - `include_ignored` - Include ignored files in the count (default: false)
//! - `paths` - Filter to specific paths (optional)

use serde::Deserialize;
use serde_json::Value;
use std::process::{Output, Stdio};
use tokio::process::Command;

use crate::agent::ToolResult;

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
        // SAFETY: this type exclusively owns the valid handle returned by
        // the corresponding Win32 API call.
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
        // SAFETY: limits has the layout and size required by this information
        // class, and job remains valid for the call.
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
        // SAFETY: limits has the layout and size required by this information
        // class, and this guard still owns a live job handle for the call.
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

    // SAFETY: entry has the required structure size and remains valid while
    // the snapshot is enumerated.
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

/// Run `git status` so dropping the future on turn cancellation terminates the
/// child and any subprocesses it spawned.
async fn run_status_command(mut command: Command) -> std::io::Result<Output> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Suspend before git can spawn hooks or helpers, assign it to a
        // kill-on-close job, then resume it below.
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }

    let child = command.spawn()?;
    #[cfg(unix)]
    let mut process_group = ProcessGroupGuard::new(child.id());
    #[cfg(windows)]
    let mut job = JobObjectGuard::assign(&child)?;
    #[cfg(windows)]
    resume_suspended_process(&child)?;
    let output = child.wait_with_output().await;
    #[cfg(unix)]
    process_group.disarm();
    #[cfg(windows)]
    if output.is_ok() {
        job.disarm()?;
    }
    output
}

#[derive(Debug, Deserialize)]
struct StatusArgs {
    #[serde(default, alias = "branchSummary")]
    branch_summary: Option<bool>,
    #[serde(default, alias = "includeIgnored")]
    include_ignored: Option<bool>,
    #[serde(default)]
    paths: Option<Value>,
}

fn normalize_paths(paths: Option<Value>) -> Vec<String> {
    match paths {
        None => Vec::new(),
        Some(Value::String(s)) => vec![s],
        Some(Value::Array(values)) => values
            .into_iter()
            .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

pub async fn git_status(args: Value, cwd: &str) -> ToolResult {
    let parsed: StatusArgs = match serde_json::from_value(args) {
        Ok(val) => val,
        Err(err) => return ToolResult::failure(format!("Invalid status arguments: {err}")),
    };

    let branch_summary = parsed.branch_summary.unwrap_or(true);
    let include_ignored = parsed.include_ignored.unwrap_or(false);
    let paths = normalize_paths(parsed.paths);

    let mut cmd = Command::new("git");
    cmd.arg("status").arg("--porcelain=v2").arg("-z");
    if branch_summary {
        cmd.arg("-b");
    }
    if include_ignored {
        cmd.arg("--ignored=matching");
    }
    if !paths.is_empty() {
        cmd.arg("--");
        cmd.args(&paths);
    }
    cmd.current_dir(cwd).stdin(Stdio::null());

    let output = match run_status_command(cmd).await {
        Ok(out) => out,
        Err(err) => return ToolResult::failure(format!("Failed to run git status: {err}")),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return ToolResult::failure(if stderr.is_empty() {
            "git status exited with a non-zero status".to_string()
        } else {
            stderr
        });
    }

    let stdout = output.stdout;
    let parts: Vec<&[u8]> = stdout.split(|b| *b == b'\0').collect();
    let mut file_count = 0;
    let mut branch_head = None;
    let mut branch_upstream = None;
    let mut ahead = None;
    let mut behind = None;

    for part in parts {
        if part.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(part);
        if line.starts_with("# branch.head ") {
            branch_head = Some(line.trim_start_matches("# branch.head ").trim().to_string());
        } else if line.starts_with("# branch.upstream ") {
            branch_upstream = Some(
                line.trim_start_matches("# branch.upstream ")
                    .trim()
                    .to_string(),
            );
        } else if line.starts_with("# branch.ab ") {
            let remainder = line.trim_start_matches("# branch.ab ").trim();
            for token in remainder.split_whitespace() {
                if let Some(val) = token.strip_prefix('+') {
                    ahead = val.parse::<u64>().ok();
                } else if let Some(val) = token.strip_prefix('-') {
                    behind = val.parse::<u64>().ok();
                }
            }
        } else if line.starts_with('1')
            || line.starts_with('2')
            || line.starts_with('u')
            || line.starts_with('?')
            || line.starts_with('!')
        {
            file_count += 1;
        }
    }

    let mut summary_lines = Vec::new();
    if branch_summary {
        let mut branch_line = format!(
            "Branch: {}",
            branch_head
                .clone()
                .unwrap_or_else(|| "(detached)".to_string())
        );
        if let Some(upstream) = &branch_upstream {
            branch_line.push_str(&format!(" -> {upstream}"));
        }
        if ahead.is_some() || behind.is_some() {
            branch_line.push_str(&format!(
                " (ahead {}, behind {})",
                ahead.unwrap_or(0),
                behind.unwrap_or(0)
            ));
        }
        summary_lines.push(branch_line);
    }
    summary_lines.push(format!("Files: {file_count}"));

    let details = serde_json::json!({
        "command": "git status --porcelain=v2 -z",
        "branch": {
            "head": branch_head,
            "upstream": branch_upstream,
            "ahead": ahead,
            "behind": behind
        },
        "files": file_count
    });

    ToolResult::success(summary_lines.join("\n")).with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_status_future_kills_spawned_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 60 & child=$!; echo \"$child\" > \"$1\"; wait")
            .arg("sh")
            .arg(&pid_file);

        let task = tokio::spawn(run_status_command(command));
        let pid: libc::pid_t = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = contents.trim().parse() {
                        break pid;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subprocess must publish a complete child pid");

        task.abort();
        let _ = task.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                // SAFETY: signal 0 only probes process existence.
                let reaped = unsafe { libc::kill(pid, 0) } != 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                // A killed grandchild may remain a zombie until its new
                // parent reaps it. It cannot execute or keep pipes open, but
                // kill(pid, 0) still reports it as an existing process.
                #[cfg(target_os = "linux")]
                let stopped = reaped
                    || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .ok()
                        .is_some_and(|stat| {
                            stat.rsplit_once(") ").is_some_and(|(_, fields)| {
                                matches!(fields.chars().next(), Some('Z' | 'X' | 'x'))
                            })
                        });
                #[cfg(not(target_os = "linux"))]
                let stopped = reaped;
                if stopped {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("grandchild process {pid} survived cancellation"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_status_future_kills_spawned_job_tree() {
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

        let task = tokio::spawn(run_status_command(command));
        for _ in 0..200 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("status subprocess must publish child pid")
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
        panic!("status grandchild process {pid} survived cancellation");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn successful_status_command_keeps_spawned_descendant_alive() {
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

        let output = run_status_command(command).await.unwrap();
        assert!(output.status.success());
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("status subprocess must publish child pid")
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
            "successful status command killed its descendant"
        );
        // SAFETY: handle grants PROCESS_TERMINATE and is exclusively closed
        // by OwnedWindowsHandle below.
        assert_ne!(unsafe { TerminateProcess(handle, 0) }, 0);
        drop(OwnedWindowsHandle(handle));
    }

    // ========================================================================
    // StatusArgs Deserialization Tests
    // ========================================================================

    #[test]
    fn test_args_deserialize_empty() {
        let json = serde_json::json!({});
        let args: StatusArgs = serde_json::from_value(json).unwrap();
        assert!(args.branch_summary.is_none());
        assert!(args.include_ignored.is_none());
        assert!(args.paths.is_none());
    }

    #[test]
    fn test_args_deserialize_snake_case() {
        let json = serde_json::json!({
            "branch_summary": false,
            "include_ignored": true
        });
        let args: StatusArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.branch_summary, Some(false));
        assert_eq!(args.include_ignored, Some(true));
    }

    #[test]
    fn test_args_deserialize_camel_case_aliases() {
        let json = serde_json::json!({
            "branchSummary": true,
            "includeIgnored": false
        });
        let args: StatusArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.branch_summary, Some(true));
        assert_eq!(args.include_ignored, Some(false));
    }

    #[test]
    fn test_args_deserialize_paths_string() {
        let json = serde_json::json!({
            "paths": "src/main.rs"
        });
        let args: StatusArgs = serde_json::from_value(json).unwrap();
        assert!(args.paths.is_some());
        assert_eq!(args.paths.unwrap().as_str(), Some("src/main.rs"));
    }

    #[test]
    fn test_args_deserialize_paths_array() {
        let json = serde_json::json!({
            "paths": ["src/", "tests/"]
        });
        let args: StatusArgs = serde_json::from_value(json).unwrap();
        assert!(args.paths.is_some());
        assert!(args.paths.unwrap().is_array());
    }

    // ========================================================================
    // normalize_paths Tests
    // ========================================================================

    #[test]
    fn test_normalize_paths_none() {
        let result = normalize_paths(None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_paths_string() {
        let result = normalize_paths(Some(Value::String("src/main.rs".to_string())));
        assert_eq!(result, vec!["src/main.rs"]);
    }

    #[test]
    fn test_normalize_paths_array() {
        let array = Value::Array(vec![
            Value::String("src/".to_string()),
            Value::String("tests/".to_string()),
            Value::String("lib/".to_string()),
        ]);
        let result = normalize_paths(Some(array));
        assert_eq!(result, vec!["src/", "tests/", "lib/"]);
    }

    #[test]
    fn test_normalize_paths_empty_array() {
        let array = Value::Array(vec![]);
        let result = normalize_paths(Some(array));
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_paths_mixed_array() {
        // Array with non-string values should filter them out
        let array = Value::Array(vec![
            Value::String("valid".to_string()),
            Value::Number(serde_json::Number::from(42)),
            Value::String("also_valid".to_string()),
            Value::Bool(true),
        ]);
        let result = normalize_paths(Some(array));
        assert_eq!(result, vec!["valid", "also_valid"]);
    }

    #[test]
    fn test_normalize_paths_invalid_type() {
        // Number value should return empty vec
        let result = normalize_paths(Some(Value::Number(serde_json::Number::from(42))));
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_paths_object() {
        // Object value should return empty vec
        let result = normalize_paths(Some(serde_json::json!({"path": "src/"})));
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_paths_null() {
        let result = normalize_paths(Some(Value::Null));
        assert!(result.is_empty());
    }
}
