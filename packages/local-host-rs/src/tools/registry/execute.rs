use super::*;
use crate::headless::report_diagnostic_nonblocking;
use crate::hooks::{HookResult, IntegratedHookSystem};
#[cfg(windows)]
use crate::tools::bash::resolve_shell_config;
use std::collections::VecDeque;
use tokio::io::{AsyncRead, AsyncReadExt as _};
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

const STREAM_CREDENTIAL_MARKERS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "credential",
    "credentials",
    "api_key",
    "api-key",
    "apikey",
    "api-token",
    "token",
    "secret",
    "secret_key",
    "aws_secret_key",
    "aws_secret_access_key",
    "aws-secret-access-key",
    "access_token",
    "refresh_token",
    "auth_token",
    "bearer",
    "authorization",
    "sk-",
    "pk-",
    "sk_live_",
    "sk_test_",
    "pk_live_",
    "pk_test_",
    "sk-ant-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xoxb-",
    "xoxa-",
    "xoxp-",
    "xoxr-",
    "xoxs-",
    "akia",
    "aiza",
    "ya29.",
    "eyj",
    "-----begin ",
];

#[derive(Default)]
struct StreamingToolOutputRedactor {
    pending: String,
    pending_emitted_prefix: usize,
}

impl StreamingToolOutputRedactor {
    fn push(&mut self, vault: &CredentialVault, generation: u64, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let lower = self.pending.to_ascii_lowercase();
        let partial_scheme_start = stream_uri_partial_credential_start(&lower);
        let incomplete_start = self.incomplete_credential_start();

        // Emit a possible URI scheme immediately, but retain an already
        // emitted copy as context for a delimiter arriving in the next read.
        // This keeps ordinary low-volume output live without exposing a URI
        // credential when the scheme itself was split from `://`.
        if partial_scheme_start.is_some() && partial_scheme_start == incomplete_start {
            let output_start = self.pending_emitted_prefix;
            let output =
                vault.vault_in_text_at_generation(generation, &self.pending[output_start..]);
            let retained = self.pending[partial_scheme_start.unwrap_or_default()..].to_string();
            self.pending = retained;
            self.pending_emitted_prefix = self.pending.len();
            return output;
        }

        let mut safe_len = incomplete_start.unwrap_or(self.pending.len());
        while safe_len > 0 && !self.pending.is_char_boundary(safe_len) {
            safe_len -= 1;
        }
        let output = if safe_len > self.pending_emitted_prefix {
            vault.vault_in_text_at_generation(
                generation,
                &self.pending[self.pending_emitted_prefix..safe_len],
            )
        } else {
            String::new()
        };
        self.pending.drain(..safe_len);
        if safe_len >= self.pending_emitted_prefix {
            self.pending_emitted_prefix = 0;
        } else {
            self.pending_emitted_prefix -= safe_len;
        }
        output
    }

    fn finish(mut self, vault: &CredentialVault, generation: u64) -> String {
        let pending = std::mem::take(&mut self.pending);
        let redacted = vault.vault_in_text_at_generation(generation, &pending);
        redacted
            .get(self.pending_emitted_prefix..)
            .unwrap_or_default()
            .to_string()
    }

    fn incomplete_credential_start(&self) -> Option<usize> {
        let lower = self.pending.to_ascii_lowercase();
        let mut incomplete_start: Option<usize> = None;

        for marker in STREAM_CREDENTIAL_MARKERS {
            let mut search_from = 0;
            while let Some(relative) = lower[search_from..].find(marker) {
                let start = search_from + relative;
                if credential_marker_is_candidate(&lower, start, marker)
                    && !credential_candidate_is_complete(&lower, start, marker)
                {
                    incomplete_start =
                        Some(incomplete_start.map_or(start, |current| current.min(start)));
                }
                search_from = start + marker.len();
            }
        }

        if let Some(start) = stream_uri_credential_start(&lower) {
            incomplete_start = Some(incomplete_start.map_or(start, |current| current.min(start)));
        }

        // Preserve a marker that is itself split across pipe reads (for
        // example, "pass" followed by "word=secret").
        for marker in STREAM_CREDENTIAL_MARKERS {
            for prefix_len in 1..marker.len() {
                if !lower.ends_with(&marker[..prefix_len]) {
                    continue;
                }
                let start = lower.len() - prefix_len;
                let preceded_by_word = start > 0
                    && lower[..start].chars().next_back().is_some_and(|character| {
                        character.is_ascii_alphanumeric() || character == '_'
                    });
                // A one-byte prefix is only held at a token boundary. This
                // avoids buffering ordinary words ending in a character that
                // happens to begin a longer credential marker, such as the
                // final y in ready versus ya29.
                if prefix_len > 1 || !preceded_by_word {
                    incomplete_start =
                        Some(incomplete_start.map_or(start, |current| current.min(start)));
                }
            }
        }

        incomplete_start
    }
}

fn is_stream_credential_key(marker: &str) -> bool {
    matches!(
        marker,
        "password"
            | "passwd"
            | "pwd"
            | "credential"
            | "credentials"
            | "api_key"
            | "api-key"
            | "apikey"
            | "api-token"
            | "token"
            | "secret"
            | "secret_key"
            | "aws_secret_key"
            | "aws_secret_access_key"
            | "aws-secret-access-key"
            | "access_token"
            | "refresh_token"
            | "auth_token"
    )
}

fn credential_marker_is_candidate(input: &str, start: usize, marker: &str) -> bool {
    let after = start + marker.len();
    if after == input.len() {
        return true;
    }

    let next = input[after..].chars().next().unwrap_or_default();
    if is_stream_credential_key(marker) {
        return next.is_ascii_whitespace() || matches!(next, '\'' | '"' | ':' | '=');
    }
    if matches!(marker, "bearer" | "authorization") {
        return next.is_ascii_whitespace() || matches!(next, ':' | '=');
    }
    true
}

fn stream_credential_boundary(character: char) -> bool {
    character.is_ascii_whitespace()
        || matches!(
            character,
            ';' | ',' | '|' | '&' | '<' | '>' | '(' | ')' | '{' | '}' | '[' | ']'
        )
}

fn stream_uri_credential_start(input: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(relative) = input[search_from..].find("://") {
        let marker_start = search_from + relative;
        let bytes = input.as_bytes();
        let mut scheme_start = marker_start;
        while scheme_start > 0
            && (bytes[scheme_start - 1].is_ascii_alphanumeric()
                || matches!(bytes[scheme_start - 1], b'+' | b'-' | b'.'))
        {
            scheme_start -= 1;
        }
        if scheme_start < marker_start {
            let authority_start = marker_start + 3;
            let authority = &input[authority_start..];
            let authority_end = authority
                .char_indices()
                .find(|(_, character)| {
                    character.is_ascii_whitespace() || matches!(character, '/' | '?' | '#')
                })
                .map_or(authority.len(), |(offset, _)| offset);
            let authority_is_complete = authority_end == authority.len();
            let authority = &authority[..authority_end];
            if authority.is_empty() && authority_is_complete {
                // Keep a complete delimiter until the authority arrives in a
                // later pipe read. Otherwise a following `user:secret@host`
                // chunk cannot be associated with this URI anymore.
                return Some(scheme_start);
            }
            if let Some(colon) = authority.find(':') {
                if authority[colon + 1..].contains('@') || authority_is_complete {
                    // A trailing `user:` or `user:secret` may become a
                    // credential-bearing authority in the next chunk.
                    return Some(scheme_start);
                }
            }
        }
        search_from = marker_start + 3;
    }
    stream_uri_partial_credential_start(input)
}

fn stream_uri_partial_credential_start(input: &str) -> Option<usize> {
    // A pipe read can end after the scheme and before the `://` delimiter.
    // Retain that possible scheme token so the next read can be joined into a
    // complete URI before it is published. Once a full delimiter exists, the
    // complete-URI scan above owns the decision and we must not hold ordinary
    // authority text here.
    let partial_marker_end = if input.ends_with(":/") {
        input.len() - 2
    } else if input.ends_with(':') {
        input.len() - 1
    } else {
        // A possible scheme split such as "https" ends with a valid scheme
        // character. Do not classify a hyphen-terminated credential marker
        // (for example "AWS-SECRET-ACCESS-") as a URI scheme, or the
        // redactor will mark its prefix as already emitted context.
        if !input
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            return None;
        }
        input.len()
    };
    if partial_marker_end == 0 || input[..partial_marker_end].contains("://") {
        return None;
    }

    let bytes = input.as_bytes();
    let mut scheme_start = partial_marker_end;
    while scheme_start > 0
        && (bytes[scheme_start - 1].is_ascii_alphanumeric()
            || matches!(bytes[scheme_start - 1], b'+' | b'-' | b'.'))
    {
        scheme_start -= 1;
    }
    (scheme_start < partial_marker_end
        && bytes[scheme_start].is_ascii_alphabetic()
        && (scheme_start == 0 || bytes[scheme_start - 1] != b'_'))
        .then_some(scheme_start)
}

fn credential_value_is_complete(input: &str, start: usize, quote: Option<char>) -> bool {
    let mut escaped = false;
    for character in input[start..].chars() {
        if let Some(quote) = quote {
            if character == quote && !escaped {
                return true;
            }
        } else if stream_credential_boundary(character) {
            return true;
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    false
}

fn credential_candidate_is_complete(input: &str, start: usize, marker: &str) -> bool {
    let after = start + marker.len();
    if marker == "-----begin " {
        return input[after..].contains("-----end ");
    }
    if marker == "authorization" {
        return input[after..]
            .chars()
            .any(|character| matches!(character, '\n' | '\r'));
    }

    let mut value_start = after;
    if is_stream_credential_key(marker) {
        let mut cursor = after;
        let mut skipped_quote = None;
        while let Some(character) = input[cursor..].chars().next() {
            if character.is_ascii_whitespace() {
                skipped_quote = None;
                cursor += character.len_utf8();
            } else if matches!(character, '\'' | '"') {
                skipped_quote = Some(character);
                cursor += character.len_utf8();
            } else {
                break;
            }
        }
        let Some(separator) = input[cursor..].chars().next() else {
            return false;
        };
        if !matches!(separator, ':' | '=') {
            // Whitespace-separated form such as `token <value>`. Hold the
            // candidate until the value reaches a boundary; completing on the
            // first value byte would release a value split across pipe reads
            // before the vault can recognize it.
            if !input[after..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_whitespace())
            {
                return false;
            }
            return credential_value_is_complete(input, cursor, skipped_quote);
        }
        cursor += separator.len_utf8();
        while let Some(character) = input[cursor..].chars().next() {
            if character.is_ascii_whitespace() {
                cursor += character.len_utf8();
            } else {
                break;
            }
        }
        let quote = input[cursor..]
            .chars()
            .next()
            .filter(|character| matches!(character, '\'' | '"'));
        if let Some(quote) = quote {
            cursor += quote.len_utf8();
        }
        value_start = cursor;
        return value_start < input.len()
            && credential_value_is_complete(input, value_start, quote);
    }

    if marker == "bearer" {
        while let Some(character) = input[value_start..].chars().next() {
            if character.is_ascii_whitespace() {
                value_start += character.len_utf8();
            } else {
                break;
            }
        }
    }
    value_start < input.len() && credential_value_is_complete(input, value_start, None)
}

fn forward_streamed_output(
    tx: &mpsc::UnboundedSender<FromAgent>,
    redactor: &mut StreamingToolOutputRedactor,
    vault: &CredentialVault,
    generation: u64,
    call_id: &str,
    chunk: &str,
    streamed: &mut bool,
) {
    let content = redactor.push(vault, generation, chunk);
    if !content.is_empty() {
        *streamed = true;
        let _ = tx.send(FromAgent::ToolOutput {
            call_id: call_id.to_string(),
            content,
        });
    }
}

async fn begin_mutation_commit(cancellation: Option<&CancellationToken>) -> bool {
    match cancellation {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => false,
                () = std::future::ready(()) => true,
            }
        }
        None => true,
    }
}

pub(super) fn indeterminate_mcp_cancellation_result(error: &crate::mcp::McpError) -> ToolResult {
    ToolResult::failure(format!(
        "MCP cancellation did not produce an authoritative terminal outcome: {error}; remote \
         outcome is unknown and must be reconciled before retry"
    ))
    .with_details(serde_json::json!({
        "cancelled": true,
        "remoteOutcome": "unknown",
        "retryable": false,
        "requiresReconciliation": true
    }))
}

pub(super) async fn run_pure_blocking<F, T>(work: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work).await
}

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
struct ProcessJobObject(Option<OwnedWindowsHandle>);

#[cfg(windows)]
impl ProcessJobObject {
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

        Ok(Self(Some(job)))
    }

    fn disarm(&mut self) -> std::io::Result<()> {
        let Some(job) = self.0.as_ref() else {
            return Ok(());
        };
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: limits has the layout and size required by this information
        // class, and this guard still owns a live job handle for the call.
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
        Ok(())
    }

    fn terminate(&mut self) {
        // Closing the kill-on-close handle terminates every process assigned
        // to the job before callers wait on inherited stdout/stderr pipes.
        self.0.take();
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

pub(super) struct ProcessRunResult {
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) exit_code: i32,
    truncated: bool,
}

pub(super) const MAX_PROCESS_STDOUT_LINE_BYTES: usize = 64 * 1024;
pub(super) const MAX_PROCESS_STDERR_BYTES: usize = 64 * 1024;

fn lossy_line(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// Read a child process stream into at most `byte_limit` bytes, keeping the
/// first and last halves.
///
/// Head-only capping discarded exactly the part that carries the verdict:
/// `cargo test`, `npm install`, and most build tools print their failure
/// summary at the end of the stream. The tail is held in a fixed-size
/// `VecDeque` of `byte_limit / 2` bytes, so an unbounded producer still costs
/// bounded memory.
///
/// Keeps both the beginning and end of bounded output.
async fn read_limited_lossy<R>(mut reader: R, byte_limit: usize, truncated_label: &str) -> String
where
    R: AsyncRead + Unpin,
{
    let head_limit = byte_limit / 2;
    let tail_limit = byte_limit - head_limit;
    let mut buffer = [0_u8; 8192];
    let mut head = Vec::with_capacity(head_limit.min(buffer.len()));
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut elided: u64 = 0;

    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let mut chunk = &buffer[..read];

        if head.len() < head_limit {
            let keep = (head_limit - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..keep]);
            chunk = &chunk[keep..];
        }
        if chunk.is_empty() {
            continue;
        }
        if tail_limit == 0 {
            elided += chunk.len() as u64;
            continue;
        }
        tail.extend(chunk.iter().copied());
        if tail.len() > tail_limit {
            let drop = tail.len() - tail_limit;
            tail.drain(..drop);
            elided += drop as u64;
        }
    }

    let tail_bytes: Vec<u8> = tail.into_iter().collect();
    if elided == 0 {
        let mut all = head;
        all.extend_from_slice(&tail_bytes);
        return String::from_utf8_lossy(&all).into_owned();
    }

    let mut output = String::from_utf8_lossy(&head).into_owned();
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(truncated_label);
    output.push('\n');
    output.push_str(&format!("[... {elided} bytes elided ...]\n"));
    output.push_str(&String::from_utf8_lossy(&tail_bytes));
    output
}

/// Text handed to the model for one MCP tool result.
///
/// Joins the text content blocks (falling back to a pretty-printed dump of
/// non-text content) and strips terminal control characters. The Native agent
/// owns the later model-facing clamp because only that layer knows whether the
/// current tool allowlist lets the model retrieve a spill file with `read`.
fn mcp_model_output(content: &[McpContent]) -> String {
    let text_output = content
        .iter()
        .filter_map(|content| match content {
            McpContent::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let output = if text_output.is_empty() {
        serde_json::to_string_pretty(content)
            .unwrap_or_else(|_| "MCP tool returned non-text content".to_string())
    } else {
        text_output
    };
    crate::output_sanitize::sanitize_control_chars(&output)
}

fn collect_string_values(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn process_path_arg(path: &str) -> String {
    if path.starts_with('-')
        && !Path::new(path).is_absolute()
        && !path.starts_with("./")
        && !path.starts_with("../")
    {
        format!("./{path}")
    } else {
        path.to_string()
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut tokio::process::Command) {}

async fn terminate_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let process_group = -(pid as libc::pid_t);
        // Kill the process group so shell children close inherited stdio promptly.
        // SAFETY: `kill` only takes integer args (pid/signal); no memory-safety
        // precondition crosses the FFI boundary. `pid` comes from
        // `tokio::process::Child::id()` for a child we spawned and still hold a
        // handle to, so the PID-reuse race is narrow: it would require the
        // child to have already exited and the OS to have recycled its pid (and
        // process-group id) to an unrelated process before this call runs.
        // `child.kill().await` below still runs regardless, so this is
        // best-effort early cleanup rather than the sole termination path.
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }

    let _ = child.kill().await;
}

#[cfg(any(test, windows))]
fn shell_quote_arg(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(any(test, windows))]
pub(super) fn build_windows_list_shell_command(shell_path: &str, recursive: bool) -> String {
    let quoted_path = shell_quote_arg(shell_path);
    if recursive {
        format!("find -- {quoted_path} -type f")
    } else {
        format!("ls -la -- {quoted_path}")
    }
}

#[cfg(any(test, windows))]
pub(super) fn build_windows_grep_shell_command(pattern: &str, shell_path: &str) -> String {
    format!(
        "grep -rn -- {} {}",
        shell_quote_arg(pattern),
        shell_quote_arg(shell_path)
    )
}

fn build_direct_grep_fallback_process(
    pattern: &str,
    display_path: &str,
) -> (String, Vec<String>, &'static str) {
    (
        "grep".to_string(),
        vec![
            "-rn".to_string(),
            "--".to_string(),
            pattern.to_string(),
            display_path.to_string(),
        ],
        "grep",
    )
}

#[cfg(windows)]
fn build_list_process(
    _display_path: &str,
    shell_path: &str,
    recursive: bool,
) -> Result<(String, Vec<String>, &'static str), String> {
    let (program, mut args) = resolve_shell_config()?;
    args.push(build_windows_list_shell_command(shell_path, recursive));
    let process_name = if recursive { "find" } else { "ls" };
    Ok((program, args, process_name))
}

#[cfg(not(windows))]
fn build_list_process(
    display_path: &str,
    _shell_path: &str,
    recursive: bool,
) -> Result<(String, Vec<String>, &'static str), String> {
    let process_path = process_path_arg(display_path);
    if recursive {
        Ok((
            "find".to_string(),
            vec![process_path, "-type".to_string(), "f".to_string()],
            "find",
        ))
    } else {
        Ok((
            "ls".to_string(),
            vec!["-la".to_string(), process_path],
            "ls",
        ))
    }
}

#[cfg(windows)]
fn build_grep_fallback_process(
    pattern: &str,
    display_path: &str,
    shell_path: &str,
) -> (String, Vec<String>, &'static str) {
    build_windows_grep_fallback_process_from_shell_config(
        pattern,
        display_path,
        shell_path,
        resolve_shell_config(),
    )
}

#[cfg(any(test, windows))]
pub(super) fn build_windows_grep_fallback_process_from_shell_config(
    pattern: &str,
    display_path: &str,
    shell_path: &str,
    shell_config: Result<(String, Vec<String>), String>,
) -> (String, Vec<String>, &'static str) {
    match shell_config {
        Ok((program, mut args)) => {
            args.push(build_windows_grep_shell_command(pattern, shell_path));
            (program, args, "grep")
        }
        Err(_) => build_direct_grep_fallback_process(pattern, display_path),
    }
}

#[cfg(not(windows))]
fn build_grep_fallback_process(
    pattern: &str,
    display_path: &str,
    _shell_path: &str,
) -> (String, Vec<String>, &'static str) {
    build_direct_grep_fallback_process(pattern, display_path)
}

pub(super) async fn run_process_limited_stdout_lines(
    program: &str,
    args: &[String],
    cwd: &str,
    timeout_ms: u64,
    line_limit: usize,
) -> Result<ProcessRunResult, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Suspend before a shell can spawn descendants, assign it to a
        // kill-on-close job after spawn, then resume it below.
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
    }

    let mut child = command
        .spawn()
        .map_err(|err| format!("Failed to start {program}: {err}"))?;
    #[cfg(windows)]
    let mut process_job = ProcessJobObject::assign(&child)
        .map_err(|err| format!("Failed to contain {program} process tree: {err}"))?;
    #[cfg(windows)]
    resume_suspended_process(&child)
        .map_err(|err| format!("Failed to resume {program} process: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{program} stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{program} stderr unavailable"))?;
    let stderr_task = tokio::spawn(async move {
        read_limited_lossy(stderr, MAX_PROCESS_STDERR_BYTES, "[stderr truncated]").await
    });

    let read_and_wait = async {
        let mut stdout = stdout;
        let mut buffer = [0_u8; 8192];
        let mut current_line = Vec::new();
        let mut lines = Vec::new();
        let mut truncated = false;
        if line_limit == 0 {
            truncated = true;
            #[cfg(windows)]
            process_job.terminate();
            terminate_child(&mut child).await;
        }
        'read_stdout: loop {
            let read = stdout
                .read(&mut buffer)
                .await
                .map_err(|err| format!("{program} stdout read failed: {err}"))?;
            if read == 0 {
                break;
            }

            for byte in &buffer[..read] {
                if lines.len() >= line_limit {
                    truncated = true;
                    #[cfg(windows)]
                    process_job.terminate();
                    terminate_child(&mut child).await;
                    break 'read_stdout;
                }
                if *byte == b'\n' {
                    lines.push(lossy_line(&current_line));
                    current_line.clear();
                    if lines.len() >= line_limit {
                        truncated = true;
                        #[cfg(windows)]
                        process_job.terminate();
                        terminate_child(&mut child).await;
                        break 'read_stdout;
                    }
                    continue;
                }
                if current_line.len() >= MAX_PROCESS_STDOUT_LINE_BYTES {
                    truncated = true;
                    lines.push(lossy_line(&current_line));
                    current_line.clear();
                    #[cfg(windows)]
                    process_job.terminate();
                    terminate_child(&mut child).await;
                    break 'read_stdout;
                }
                current_line.push(*byte);
            }
        }

        if !current_line.is_empty() {
            if lines.len() >= line_limit {
                truncated = true;
                #[cfg(windows)]
                process_job.terminate();
                terminate_child(&mut child).await;
            } else {
                lines.push(lossy_line(&current_line));
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|err| format!("{program} wait failed: {err}"))?;
        Ok::<_, String>((lines, truncated, status.code().unwrap_or(-1)))
    };

    let (lines, truncated, exit_code) =
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read_and_wait)
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                #[cfg(windows)]
                process_job.terminate();
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                let _ = stderr_task.await;
                return Err(format!("{program} timed out after {timeout_ms}ms"));
            }
        };
    let stderr = stderr_task.await.unwrap_or_default();
    #[cfg(windows)]
    process_job
        .disarm()
        .map_err(|err| format!("Failed to release {program} process tree: {err}"))?;

    Ok(ProcessRunResult {
        stdout: lines.join("\n"),
        stderr,
        exit_code,
        truncated,
    })
}

pub(super) fn process_succeeded_or_truncated(
    result: &ProcessRunResult,
    success_codes: &[i32],
) -> bool {
    result.truncated || success_codes.contains(&result.exit_code)
}

pub(super) async fn run_grep_with_fallback(
    rg_program: &str,
    rg_args: &[String],
    grep_program: &str,
    grep_args: &[String],
    cwd: &str,
    timeout_ms: u64,
    line_limit: usize,
) -> Result<(ProcessRunResult, &'static str), String> {
    match run_process_limited_stdout_lines(rg_program, rg_args, cwd, timeout_ms, line_limit).await {
        Ok(result) => Ok((result, "rg")),
        Err(message) if message.starts_with(&format!("Failed to start {rg_program}:")) => {
            run_process_limited_stdout_lines(grep_program, grep_args, cwd, timeout_ms, line_limit)
                .await
                .map(|result| (result, "grep"))
                .map_err(|fallback_message| {
                    format!("{message}; fallback grep failed: {fallback_message}")
                })
        }
        Err(message) => Err(message),
    }
}

impl ToolExecutor {
    async fn execute_repository_symbols(&self, args: &serde_json::Value) -> ToolResult {
        let symbol = args
            .get("symbol")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let max_results = match args.get("maxResults") {
            Some(value) => match value.as_u64() {
                Some(value @ 1..=100) => value as usize,
                _ => return ToolResult::failure("maxResults must be between 1 and 100"),
            },
            None => 20,
        };
        match self.repository_symbols.query(symbol, max_results) {
            Ok(_) | Err(maestro_workspace::symbols::SymbolIndexError::NotReady) => {}
            Err(error) => return ToolResult::failure(error.to_string()),
        }

        if self.repository_symbols.needs_blocking_refresh() {
            for _ in 0..2 {
                let index = Arc::clone(&self.repository_symbols);
                match tokio::task::spawn_blocking(move || index.refresh()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => return ToolResult::failure(error.to_string()),
                    Err(error) => {
                        return ToolResult::failure(format!(
                            "repository symbol refresh failed: {error}"
                        ));
                    }
                }
                if !self.repository_symbols.needs_blocking_refresh() {
                    break;
                }
            }
            if self.repository_symbols.needs_blocking_refresh() {
                return ToolResult::failure(
                    "repository changed repeatedly while the symbol index was refreshing",
                );
            }
        } else if self.repository_symbols.needs_background_refresh()
            && self.repository_symbols.begin_background_refresh()
        {
            let index = Arc::clone(&self.repository_symbols);
            tokio::spawn(async move {
                let worker = Arc::clone(&index);
                let _ = tokio::task::spawn_blocking(move || worker.refresh()).await;
                index.end_background_refresh();
            });
        }

        let result = match self.repository_symbols.query(symbol, max_results) {
            Ok(result) => result,
            Err(error) => return ToolResult::failure(error.to_string()),
        };
        let details = serde_json::json!({
            "revision": result.revision.clone(),
            "indexedFiles": result.indexed_files,
            "skippedFiles": result.skipped_files,
            "indexTruncated": result.index_truncated,
            "indexAgeMs": result.index_age_ms,
            "truncated": result.truncated,
        });
        match serde_json::to_string(&result) {
            Ok(output) => ToolResult::success(output).with_details(details),
            Err(error) => ToolResult::failure(format!("serialize repository symbols: {error}")),
        }
    }

    async fn execute_search(&self, args: &serde_json::Value) -> ToolResult {
        let start_time = Instant::now();
        let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        if pattern.is_empty() {
            return ToolResult::failure("Missing pattern argument".to_string());
        }

        let paths = collect_string_values(args.get("paths").or_else(|| args.get("path")));
        let glob_patterns = collect_string_values(args.get("glob"));
        let command_cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(cwd) => match resolve_tool_path(&self.cwd, cwd) {
                Ok(path) => path,
                Err(message) => return ToolResult::failure(message),
            },
            None => self.cwd.clone(),
        };

        let mut rg_args = vec![
            "--color=never".to_string(),
            "--no-heading".to_string(),
            "-n".to_string(),
        ];
        let output_mode = args
            .get("outputMode")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        if output_mode == "files" {
            rg_args.push("-l".to_string());
        } else if output_mode == "count" {
            rg_args.push("--count-matches".to_string());
            rg_args.push("-H".to_string());
        }
        if args
            .get("ignoreCase")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("-i".to_string());
        }
        if args
            .get("literal")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("-F".to_string());
        }
        if args
            .get("word")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("-w".to_string());
        }
        if args
            .get("multiline")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("--multiline".to_string());
        }
        if let Some(context) = args.get("context").and_then(serde_json::Value::as_u64) {
            rg_args.push("-C".to_string());
            rg_args.push(context.to_string());
        } else {
            if let Some(before) = args
                .get("beforeContext")
                .and_then(serde_json::Value::as_u64)
            {
                rg_args.push("-B".to_string());
                rg_args.push(before.to_string());
            }
            if let Some(after) = args.get("afterContext").and_then(serde_json::Value::as_u64) {
                rg_args.push("-A".to_string());
                rg_args.push(after.to_string());
            }
        }
        for glob in &glob_patterns {
            rg_args.push("-g".to_string());
            rg_args.push(glob.clone());
        }
        if args
            .get("includeHidden")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("--hidden".to_string());
        }
        if args
            .get("useGitIgnore")
            .and_then(serde_json::Value::as_bool)
            .is_some_and(|v| !v)
        {
            rg_args.push("--no-ignore".to_string());
        }
        if args
            .get("invertMatch")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("--invert-match".to_string());
        }
        if args
            .get("onlyMatching")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            rg_args.push("--only-matching".to_string());
        }

        if args
            .get("format")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|format| format == "json")
            && output_mode == "content"
        {
            rg_args.push("--json".to_string());
        }

        for path in &paths {
            if path.trim().is_empty() {
                return ToolResult::failure("paths entries must be non-empty strings");
            }
        }

        let head_limit = args
            .get("headLimit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(MAX_GREP_LINES as u64)
            .max(1) as usize;
        if let Some(max_results) = args.get("maxResults").and_then(serde_json::Value::as_u64) {
            rg_args.push("-m".to_string());
            rg_args.push(max_results.to_string());
        } else if output_mode == "content" {
            rg_args.push("-m".to_string());
            rg_args.push(head_limit.to_string());
        }

        rg_args.push("--".to_string());
        rg_args.push(pattern.to_string());
        if paths.is_empty() {
            rg_args.push(".".to_string());
        } else {
            rg_args.extend(paths.iter().cloned());
        }

        let result = match run_process_limited_stdout_lines(
            "rg",
            &rg_args,
            &command_cwd,
            30_000,
            head_limit,
        )
        .await
        {
            Ok(result) => result,
            Err(message) => return ToolResult::failure(message),
        };

        let duration_ms = start_time.elapsed().as_millis() as u64;
        let output_lines: Vec<&str> = result.stdout.lines().collect();
        let matches_count = output_lines.len();
        let truncated = result.truncated || matches_count >= head_limit;
        let output = output_lines
            .iter()
            .take(head_limit)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");

        let mut details = GrepDetails::new(pattern)
            .with_path(paths.join(", "))
            .with_matches(matches_count)
            .with_duration(duration_ms)
            .with_search_tool("rg");
        if truncated {
            details = details.with_truncation();
        }

        if process_succeeded_or_truncated(&result, &[0, 1]) {
            ToolResult::success(output).with_details(details.to_json())
        } else {
            let message = result.stderr.trim();
            let message = if message.is_empty() {
                format!("ripgrep exited with status {}", result.exit_code)
            } else {
                message.to_string()
            };
            ToolResult::failure(message).with_details(details.to_json())
        }
    }

    /// Deny a native-process file mutation (`write`/`edit`/`notebook_edit`)
    /// that a sandbox policy would not permit.
    ///
    /// Unlike `bash`/`background_tasks`, these tools run their file I/O
    /// directly in the Maestro process rather than through
    /// `spawn_sandboxed_command`, so the OS-level sandbox (Seatbelt/Landlock)
    /// never sees them and provides no containment. This is the explicit
    /// preflight path check: it runs early with the fully resolved target
    /// path so obviously-out-of-sandbox writes fail with a clear message
    /// before any side effects (backups, temp files). The actual mutation
    /// must then go through [`crate::sandbox::commit_native_write`], which
    /// revalidates the policy against the directory descriptor it writes
    /// through — the preflight alone is racy (a path swap between check and
    /// write), the commit-time revalidation is not.
    ///
    /// Returns `Some(ToolResult::failure(..))` to deny, or `None` to allow
    /// (either there is no active sandbox policy, or the policy permits a
    /// write to `resolved_path`).
    fn deny_native_write_outside_sandbox(&self, resolved_path: &str) -> Option<ToolResult> {
        crate::sandbox::preflight_native_write(
            self.sandbox_policy.as_ref(),
            std::path::Path::new(&self.cwd),
            std::path::Path::new(resolved_path),
        )
        .err()
        .map(ToolResult::failure)
    }

    /// Execute bounded local exploration while applying the native hook
    /// pipeline to every nested operation.
    async fn execute_explore(
        &self,
        args: &serde_json::Value,
        hook_event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        cancel: Option<CancellationToken>,
        mut hooks: Option<&mut IntegratedHookSystem>,
    ) -> ToolResult {
        let operations = match args.get("operations").and_then(Value::as_array) {
            Some(operations) if !operations.is_empty() => operations,
            _ => {
                return ToolResult::failure(
                    "explore requires a non-empty operations array".to_string(),
                );
            }
        };
        if operations.len() > 8 {
            return ToolResult::failure("explore accepts at most 8 operations");
        }

        struct PreparedExploreOperation {
            tool_name: String,
            args: Value,
            call_id: String,
            extra_context: Option<String>,
            skip_reason: Option<String>,
        }

        let mut prepared = Vec::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            let operation_object = match operation.as_object() {
                Some(operation) => operation,
                None => return ToolResult::failure("Each explore operation must be an object"),
            };
            let sub_tool = operation_object
                .get("tool")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if !matches!(
                sub_tool.as_str(),
                "read" | "glob" | "grep" | "find" | "list" | "search" | "parallel_ripgrep" | "diff"
            ) {
                return ToolResult::failure(format!(
                    "Unsupported explore operation: {sub_tool}. Only local read/search tools are allowed"
                ));
            }
            let sub_args = match operation_object.get("args") {
                Some(Value::Object(args)) => Value::Object(args.clone()),
                _ => return ToolResult::failure("Each explore operation requires an args object"),
            };
            let sub_call_id = format!("{call_id}:explore:{index}");
            let mut effective_args = sub_args;
            let mut extra_context = None;
            let mut skip_reason = None;
            if let Some(hooks) = hooks.as_deref_mut() {
                match hooks.execute_pre_tool_use(&sub_tool, &sub_call_id, &effective_args) {
                    HookResult::Block { reason } => {
                        if let Some(tx) = hook_event_tx {
                            let _ = tx.send(FromAgent::HookBlocked {
                                call_id: sub_call_id.clone(),
                                tool: sub_tool.clone(),
                                reason: reason.clone(),
                            });
                        }
                        skip_reason = Some(format!("Tool blocked by hook: {reason}"));
                    }
                    HookResult::ModifyInput { new_input } => {
                        effective_args = new_input;
                    }
                    HookResult::InjectContext { context } => {
                        extra_context = Some(context);
                    }
                    HookResult::Continue => {}
                }
            }
            if skip_reason.is_none() {
                let missing = self.missing_required(&sub_tool, &effective_args);
                if !missing.is_empty() {
                    skip_reason = Some(format!(
                        "Missing required fields for tool '{}': {}",
                        sub_tool,
                        missing.join(", ")
                    ));
                }
            }
            prepared.push(PreparedExploreOperation {
                tool_name: sub_tool,
                args: effective_args,
                call_id: sub_call_id,
                extra_context,
                skip_reason,
            });
        }

        let executions = prepared.into_iter().map(|operation| {
            let cancel = cancel.clone();
            async move {
                if let Some(reason) = operation.skip_reason.clone() {
                    return (operation, ToolResult::failure(reason), false);
                }
                let result = Box::pin(self.execute_at_generation(
                    &operation.tool_name,
                    &operation.args,
                    None,
                    &operation.call_id,
                    generation,
                    cancel,
                ))
                .await;
                (operation, result, true)
            }
        });
        let results = futures::future::join_all(executions).await;

        let mut success = true;
        let output = results
            .into_iter()
            .enumerate()
            .map(|(index, (operation, mut result, executed))| {
                if executed {
                    if let Some(hooks) = hooks.as_deref_mut() {
                        let post_output = result.error.as_ref().map_or_else(
                            || result.output.clone(),
                            |error| {
                                if result.output.is_empty() {
                                    error.clone()
                                } else {
                                    format!("{}\n{error}", result.output)
                                }
                            },
                        );
                        // Sub-operations of one tool call are timed as a
                        // group by the caller, not individually, so this
                        // reports 0 rather than inventing a per-operation
                        // duration.
                        let _ = hooks.execute_post_tool_use(
                            &operation.tool_name,
                            &operation.call_id,
                            &operation.args,
                            &post_output,
                            !result.success,
                            0,
                        );
                    }
                }
                if let Some(context) = operation.extra_context {
                    result.output = if result.output.is_empty() {
                        context
                    } else {
                        format!("{}\n\n{context}", result.output)
                    };
                }
                success &= result.success;
                serde_json::json!({
                    "index": index,
                    "success": result.success,
                    "output": result.output,
                    "error": result.error,
                })
            })
            .collect::<Vec<_>>();
        let output = serde_json::to_string_pretty(&output)
            .unwrap_or_else(|_| "explore results could not be serialized".to_string());
        if success {
            ToolResult::success(output)
        } else {
            ToolResult {
                success: false,
                output,
                error: Some("One or more explore operations failed".to_string()),
                details: None,
            }
        }
    }

    /// Internal implementation of tool execution (without caching)
    pub(super) async fn execute_impl(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        event_tx: Option<&mpsc::UnboundedSender<FromAgent>>,
        call_id: &str,
        generation: u64,
        execution_context: ToolExecutionContext<'_>,
    ) -> ToolResult {
        let ToolExecutionContext {
            code_decision,
            cancel,
            approved_inline_env,
            hooks,
            emit_tool_events,
        } = execution_context;
        if code_decision
            .as_ref()
            .is_some_and(|decision| !decision.is_current())
        {
            return ToolResult::failure("Code tool authority expired before execution");
        }
        let lifecycle_event_tx = emit_tool_events.then_some(event_tx).flatten();
        if super::is_reserved_orb_tool(tool_name) {
            return ToolResult::failure(
                "Raw Computer MCP lifecycle tools are reserved; use the durable subagent tools",
            );
        }
        // A resumed transcript can carry a call recorded against a tool whose
        // definition has since changed, or whose server has been swapped for
        // another one exporting the same name. Refuse rather than execute the
        // new tool under the old approval. See `tools::tool_call_contract`.
        if McpClient::is_mcp_tool(tool_name) {
            if let Some(recorded) = crate::tools::tool_call_contract::recorded_contract(tool_name) {
                let live = self.live_mcp_tool_contract(call_id, tool_name).await;
                if let Err(reason) =
                    crate::tools::tool_call_contract::validate_identity(&recorded, live.as_ref())
                {
                    // Refuse once, then forget the pin: a re-issued call is
                    // judged against the current definition and needs its own
                    // approval, rather than being refused forever.
                    crate::tools::tool_call_contract::drop_contract(tool_name);
                    return ToolResult::failure(reason);
                }
            }
        }
        if McpClient::is_mcp_tool(tool_name) {
            let client = match cancel.as_ref() {
                Some(token) => tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        return cancelled_tool_result("MCP tool cancelled");
                    }
                    client = self.ensure_mcp_client() => client,
                },
                None => self.ensure_mcp_client().await,
            };
            let client = match client {
                Ok(client) => client,
                Err(err) => return ToolResult::failure(err),
            };

            let call_result = match cancel.as_ref() {
                Some(token) => {
                    match client
                        .call_tool_with_metadata_cancellable(tool_name, args.clone(), token)
                        .await
                    {
                        Err(crate::mcp::McpError::Cancelled) => {
                            return cancelled_tool_result("MCP tool cancelled");
                        }
                        Err(error @ crate::mcp::McpError::Indeterminate(_)) => {
                            return indeterminate_mcp_cancellation_result(&error);
                        }
                        result => result,
                    }
                }
                None => {
                    client
                        .call_tool_with_metadata(tool_name, args.clone())
                        .await
                }
            };

            match call_result {
                Ok((server_name, tool_label, result)) => {
                    let output = mcp_model_output(&result.content);
                    let details = serde_json::json!({
                        "server": server_name,
                        "tool": tool_label,
                        "content": result.content,
                        "isError": result.is_error
                    });
                    return if result.is_error {
                        ToolResult {
                            success: false,
                            output,
                            error: Some("MCP tool reported an error".to_string()),
                            details: Some(details),
                        }
                    } else {
                        ToolResult::success(output).with_details(details)
                    };
                }
                Err(err) => {
                    return ToolResult::failure(format!("MCP tool error: {err}"));
                }
            }
        }

        let needs_orb_adapter = match tool_name {
            "spawn_subagent" => {
                args.get("backend")
                    .and_then(Value::as_str)
                    .is_some_and(|backend| {
                        backend.eq_ignore_ascii_case("computer")
                            || backend.eq_ignore_ascii_case("orb")
                    })
            }
            "list_subagents" => self.subagents.has_orb_records(),
            "get_subagent" | "wait_subagent" | "resume_subagent" | "cancel_subagent"
            | "control_subagent" => self.subagents.uses_orb_backend(args),
            _ => false,
        };
        if needs_orb_adapter {
            if let Err(error) = self.ensure_orb_delegation_adapter().await {
                return ToolResult::failure(error);
            }
        }

        // Every name (canonical or alias) matched below dispatches before
        // the wildcard arm's inline-tool fallback ever runs. Adding a new
        // arm/alias here also needs a matching entry in
        // `registry::is_reserved_execute_dispatch_name`, or an inline tool
        // registered under that name would silently never execute despite
        // passing the collision check.
        match tool_name {
            "coding_task" => {
                // Keep the workflow future out of every tool dispatch frame,
                // including ordinary Bash calls that do not use acceptance.
                Box::pin(self.execute_coding_task(args, call_id, cancel.as_ref())).await
            }
            "explore" | "Explore" => {
                if hooks.is_some() {
                    return Box::pin(
                        self.execute_explore(args, event_tx, call_id, generation, cancel, hooks),
                    )
                    .await;
                }

                let operations = match args.get("operations").and_then(Value::as_array) {
                    Some(operations) if !operations.is_empty() => operations,
                    _ => {
                        return ToolResult::failure(
                            "explore requires a non-empty operations array".to_string(),
                        );
                    }
                };
                if operations.len() > 8 {
                    return ToolResult::failure("explore accepts at most 8 operations");
                }

                let mut executions = Vec::with_capacity(operations.len());
                for (index, operation) in operations.iter().enumerate() {
                    let operation_object = match operation.as_object() {
                        Some(operation) => operation,
                        None => {
                            return ToolResult::failure("Each explore operation must be an object");
                        }
                    };
                    let sub_tool = operation_object
                        .get("tool")
                        .and_then(Value::as_str)
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_default();
                    if !matches!(
                        sub_tool.as_str(),
                        "read"
                            | "glob"
                            | "grep"
                            | "find"
                            | "list"
                            | "search"
                            | "parallel_ripgrep"
                            | "diff"
                    ) {
                        return ToolResult::failure(format!(
                            "Unsupported explore operation: {sub_tool}. Only local read/search tools are allowed"
                        ));
                    }
                    let sub_args = match operation_object.get("args") {
                        Some(Value::Object(args)) => Value::Object(args.clone()),
                        _ => {
                            return ToolResult::failure(
                                "Each explore operation requires an args object",
                            );
                        }
                    };
                    let sub_call_id = format!("{call_id}:explore:{index}");
                    let sub_cancel = cancel.clone();
                    executions.push(async move {
                        Box::pin(self.execute_at_generation(
                            &sub_tool,
                            &sub_args,
                            None,
                            &sub_call_id,
                            generation,
                            sub_cancel,
                        ))
                        .await
                    });
                }

                let results = futures::future::join_all(executions).await;
                let success = results.iter().all(|result| result.success);
                let output = results
                    .into_iter()
                    .enumerate()
                    .map(|(index, result)| {
                        serde_json::json!({
                            "index": index,
                            "success": result.success,
                            "output": result.output,
                            "error": result.error,
                        })
                    })
                    .collect::<Vec<_>>();
                let output = serde_json::to_string_pretty(&output)
                    .unwrap_or_else(|_| "explore results could not be serialized".to_string());
                if success {
                    ToolResult::success(output)
                } else {
                    ToolResult {
                        success: false,
                        output,
                        error: Some("One or more explore operations failed".to_string()),
                        details: None,
                    }
                }
            }
            "bash" | "Bash" => {
                let bash_args: BashArgs = match serde_json::from_value(args.clone()) {
                    Ok(a) => a,
                    Err(e) => {
                        return ToolResult::failure(format!("Invalid bash arguments: {e}"));
                    }
                };

                if let Err(err) = crate::plan_mode::gate_mutation("bash", None, &self.cwd) {
                    return ToolResult::failure(err);
                }
                if let Err(error) = self.check_coding_bash_revision(&bash_args.command) {
                    return ToolResult::failure(error);
                }

                let result = if let Some(tx) = event_tx {
                    if emit_tool_events {
                        let _ = tx.send(FromAgent::ToolStart {
                            call_id: call_id.to_string(),
                        });
                    }

                    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
                    let execution = self.bash.execute_with_cancellation_and_output(
                        bash_args,
                        cancel,
                        Some(stream_tx),
                    );
                    tokio::pin!(execution);
                    let mut streamed = false;
                    // stdout and stderr are independent byte streams. Keep a
                    // separate redactor for each so a boundary on one pipe
                    // cannot complete a credential candidate on the other.
                    let mut stdout_redactor = StreamingToolOutputRedactor::default();
                    let mut stderr_redactor = StreamingToolOutputRedactor::default();
                    let uncached_result = loop {
                        tokio::select! {
                            biased;
                            result = &mut execution => break result,
                            Some(chunk) = stream_rx.recv() => {
                                let redactor = match chunk.stream {
                                    BashOutputStream::Stdout => &mut stdout_redactor,
                                    BashOutputStream::Stderr => &mut stderr_redactor,
                                };
                                forward_streamed_output(
                                    tx,
                                    redactor,
                                    &self.credential_vault,
                                    generation,
                                    call_id,
                                    &chunk.content,
                                    &mut streamed,
                                );
                            }
                        }
                    };
                    while let Ok(chunk) = stream_rx.try_recv() {
                        let redactor = match chunk.stream {
                            BashOutputStream::Stdout => &mut stdout_redactor,
                            BashOutputStream::Stderr => &mut stderr_redactor,
                        };
                        forward_streamed_output(
                            tx,
                            redactor,
                            &self.credential_vault,
                            generation,
                            call_id,
                            &chunk.content,
                            &mut streamed,
                        );
                    }
                    for tail in [
                        stdout_redactor.finish(&self.credential_vault, generation),
                        stderr_redactor.finish(&self.credential_vault, generation),
                    ] {
                        if !tail.is_empty() {
                            streamed = true;
                            let _ = tx.send(FromAgent::ToolOutput {
                                call_id: call_id.to_string(),
                                content: tail,
                            });
                        }
                    }
                    let result = vault_tool_result_credentials(
                        &self.credential_vault,
                        generation,
                        uncached_result,
                    );
                    let result_was_truncated = result
                        .details
                        .as_ref()
                        .and_then(|details| details.get("truncated"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let result = if streamed {
                        let mut result = result;
                        let mut details = result
                            .details
                            .take()
                            .unwrap_or_else(|| serde_json::json!({}));
                        details["streamed"] = serde_json::json!(true);
                        result.details = Some(details);
                        result
                    } else {
                        result
                    };
                    if streamed && result_was_truncated && !result.output.is_empty() {
                        // Live output is prefix-bounded, while the completed
                        // Bash result is the authoritative tail. Emit it as
                        // a second segment so failures at the end of a large
                        // command are not hidden by the live cap.
                        let _ = tx.send(FromAgent::ToolOutput {
                            call_id: call_id.to_string(),
                            content: result.output.clone(),
                        });
                    }
                    if emit_tool_events {
                        if !streamed && !result.output.is_empty() {
                            let _ = tx.send(FromAgent::ToolOutput {
                                call_id: call_id.to_string(),
                                content: result.output.clone(),
                            });
                        }
                        let _ = tx.send(FromAgent::ToolEnd {
                            call_id: call_id.to_string(),
                            success: result.success,
                            result: Some(result.clone()),
                            receipt: None,
                        });
                    }
                    result
                } else {
                    vault_tool_result_credentials(
                        &self.credential_vault,
                        generation,
                        self.bash.execute_with_cancellation(bash_args, cancel).await,
                    )
                };

                if let Err(error) = self.observe_coding_bash(call_id, &result) {
                    return ToolResult::failure(format!("Coding evidence capture failed: {error}"));
                }
                result
            }
            "read" | "Read" => {
                let start_time = Instant::now();
                let raw_path = args
                    .get("path")
                    .or_else(|| args.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = match resolve_tool_path(&self.cwd, raw_path) {
                    Ok(resolved) => resolved,
                    Err(message) => return ToolResult::failure(message),
                };

                let path_buf = std::path::Path::new(&path);
                let extension = path_buf
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase);

                // Optional line offset (1-indexed, defaults to 1)
                let offset = args
                    .get("offset")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(1, |v| v.max(1) as usize);

                // Optional line limit (defaults to reading all)
                let limit = args
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .map(|v| v as usize);

                let mode = args
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("normal");

                let line_numbers = args
                    .get("lineNumbers")
                    .or_else(|| args.get("line_numbers"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                let wrap_in_code_fence = args
                    .get("wrapInCodeFence")
                    .or_else(|| args.get("wrap_in_code_fence"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                let as_base64 = args
                    .get("asBase64")
                    .or_else(|| args.get("as_base64"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                let with_diagnostics = args
                    .get("withDiagnostics")
                    .or_else(|| args.get("diagnostics"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                let language = args.get("language").and_then(|v| v.as_str());

                if let Some(ext) = extension.as_deref() {
                    let is_image =
                        matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg");
                    if is_image {
                        let image_args = ReadImageArgs {
                            file_path: path.clone(),
                            max_dimension: None,
                        };
                        return self.image.read_image(image_args).await;
                    }
                }

                if let Some(ext) = extension.as_deref() {
                    if ext == "pdf" {
                        if let Ok(metadata) = tokio::fs::metadata(&path).await {
                            let size_bytes = metadata.len();
                            if size_bytes > MAX_READ_SIZE_BYTES {
                                let size_mb = (size_bytes as f64) / (1024.0 * 1024.0);
                                let details = ReadDetails::new(path.clone())
                                    .with_size(size_bytes)
                                    .with_mime_type("application/pdf")
                                    .with_duration(start_time.elapsed().as_millis() as u64);
                                return ToolResult::failure(format!(
                                    "File is too large ({size_mb:.2}MB). Maximum size is 10MB."
                                ))
                                .with_details(details.to_json());
                            }
                        }
                        let bytes = match tokio::fs::read(&path).await {
                            Ok(data) => data,
                            Err(err) => {
                                let details = ReadDetails::new(path.clone())
                                    .with_duration(start_time.elapsed().as_millis() as u64);
                                return ToolResult::failure(format!("Failed to read PDF: {err}"))
                                    .with_details(details.to_json());
                            }
                        };
                        let size_bytes = bytes.len() as u64;
                        let text = match run_pure_blocking(move || {
                            pdf_extract::extract_text_from_mem(&bytes)
                                .map_err(|err| err.to_string())
                        })
                        .await
                        {
                            Ok(Ok(text)) => text,
                            Ok(Err(err)) => {
                                let details = ReadDetails::new(path.clone())
                                    .with_duration(start_time.elapsed().as_millis() as u64)
                                    .with_mime_type("application/pdf");
                                return ToolResult::failure(format!(
                                    "Failed to extract PDF: {err}"
                                ))
                                .with_details(details.to_json());
                            }
                            Err(err) => {
                                let details = ReadDetails::new(path.clone())
                                    .with_duration(start_time.elapsed().as_millis() as u64)
                                    .with_mime_type("application/pdf");
                                return ToolResult::failure(format!(
                                    "PDF extraction worker failed: {err}"
                                ))
                                .with_details(details.to_json());
                            }
                        };
                        let mut output = text;
                        if wrap_in_code_fence {
                            let fence_language = language.unwrap_or("");
                            output = format!("```{fence_language}\n{output}\n```");
                        }
                        let details = ReadDetails::new(path.clone())
                            .with_size(size_bytes)
                            .with_mime_type("application/pdf")
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::success(output).with_details(details.to_json());
                    }
                }

                if let Some(ext) = extension.as_deref() {
                    if ext == "ipynb" {
                        let content = match tokio::fs::read_to_string(&path).await {
                            Ok(text) => text,
                            Err(err) => {
                                let details = ReadDetails::new(path.clone())
                                    .with_duration(start_time.elapsed().as_millis() as u64);
                                return ToolResult::failure(format!(
                                    "Failed to read notebook: {err}"
                                ))
                                .with_details(details.to_json());
                            }
                        };
                        let notebook: serde_json::Value = match serde_json::from_str(&content) {
                            Ok(val) => val,
                            Err(err) => {
                                let details = ReadDetails::new(path.clone())
                                    .with_duration(start_time.elapsed().as_millis() as u64);
                                return ToolResult::failure(format!(
                                    "Failed to parse notebook: {err}"
                                ))
                                .with_details(details.to_json());
                            }
                        };
                        let cells = notebook.get("cells").and_then(|v| v.as_array()).cloned();
                        let cells = match cells {
                            Some(val) => val,
                            None => {
                                return ToolResult::failure(
                                    "Invalid notebook format: missing cells".to_string(),
                                );
                            }
                        };
                        let mut lines = Vec::new();
                        for (idx, cell) in cells.iter().enumerate() {
                            let cell_type = cell
                                .get("cell_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("code");
                            let cell_id = cell.get("id").and_then(|v| v.as_str());
                            let source = cell.get("source").map(|v| {
                                if v.is_array() {
                                    v.as_array()
                                        .unwrap_or(&Vec::new())
                                        .iter()
                                        .filter_map(|line| line.as_str())
                                        .collect::<Vec<_>>()
                                        .join("")
                                } else {
                                    v.as_str().unwrap_or("").to_string()
                                }
                            });
                            let preview = source.unwrap_or_default();
                            let preview_lines: Vec<&str> = preview.lines().take(3).collect();
                            let truncated = if preview.lines().count() > 3 {
                                "..."
                            } else {
                                ""
                            };
                            let id_suffix =
                                cell_id.map(|id| format!(" (id: {id})")).unwrap_or_default();
                            lines.push(format!(
                                "[{}] {}{}:\n{}{}",
                                idx,
                                cell_type,
                                id_suffix,
                                preview_lines.join("\n"),
                                truncated
                            ));
                            lines.push(String::new());
                        }
                        let output = lines.join("\n");
                        let details = ReadDetails::new(path.clone())
                            .with_size(content.len() as u64)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::success(output).with_details(details.to_json());
                    }
                }

                if let Ok(metadata) = tokio::fs::metadata(&path).await {
                    let size_bytes = metadata.len();
                    if size_bytes > MAX_READ_SIZE_BYTES {
                        let size_mb = (size_bytes as f64) / (1024.0 * 1024.0);
                        let details = ReadDetails::new(path.clone())
                            .with_size(size_bytes)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::failure(format!(
                            "File is too large ({size_mb:.2}MB). Maximum size is 10MB. Use offset/limit or bash head/tail for large files."
                        ))
                        .with_details(details.to_json());
                    }
                }

                let bytes = match tokio::fs::read(&path).await {
                    Ok(data) => data,
                    Err(e) => {
                        let details = ReadDetails::new(path.clone())
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::failure(format!("Failed to read file: {e}"))
                            .with_details(details.to_json());
                    }
                };

                if is_probably_binary(&bytes) && !as_base64 {
                    let details = ReadDetails::new(path.clone())
                        .with_size(bytes.len() as u64)
                        .with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::failure(
                        "Binary file detected. Re-run with asBase64=true or use the bash tool.",
                    )
                    .with_details(details.to_json());
                }

                if as_base64 {
                    let encoded = STANDARD.encode(&bytes);
                    let details = ReadDetails::new(path.clone())
                        .with_size(bytes.len() as u64)
                        .with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::success(encoded).with_details(details.to_json());
                }

                let content = if let Ok(text) = String::from_utf8(bytes) {
                    text
                } else {
                    let details = ReadDetails::new(path.clone())
                        .with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::failure(
                        "File is not valid UTF-8. Re-run with asBase64=true or use the bash tool.",
                    )
                    .with_details(details.to_json());
                };

                let lines: Vec<&str> = content.lines().collect();
                let total_lines = lines.len();

                let mut start_idx = (offset - 1).min(total_lines);
                let mut max_lines = limit.unwrap_or(total_lines);

                match mode {
                    "head" => {
                        start_idx = 0;
                        max_lines = limit.unwrap_or(total_lines);
                    }
                    "tail" => {
                        max_lines = limit.unwrap_or(total_lines);
                        start_idx = total_lines.saturating_sub(max_lines);
                    }
                    "normal" => {}
                    _ => {
                        let details = ReadDetails::new(path.clone())
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::failure("Invalid mode. Use normal, head, or tail.")
                            .with_details(details.to_json());
                    }
                }

                let end_idx = (start_idx + max_lines).min(total_lines);
                let lines_read = end_idx.saturating_sub(start_idx);
                let truncated = limit.is_some() && end_idx < total_lines;

                let mut output: String = lines[start_idx..end_idx]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        if line_numbers {
                            format!("{:>6}\t{}", start_idx + i + 1, line)
                        } else {
                            (*line).to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                if wrap_in_code_fence {
                    let fence_language = language.unwrap_or("");
                    output = format!("```{fence_language}\n{output}\n```");
                }

                // Implicit diagnostics spawn an unsandboxed language server;
                // skip them when the active policy cannot contain that launch.
                if with_diagnostics && self.may_launch_native_language_server() {
                    if let Ok(diagnostics) = lsp::diagnostics_for_file(&self.cwd, &path).await {
                        if !diagnostics.is_empty() {
                            let errors: Vec<_> = diagnostics
                                .iter()
                                .filter(|d| d.severity == Some(1) || d.severity.is_none())
                                .collect();
                            let warnings: Vec<_> = diagnostics
                                .iter()
                                .filter(|d| d.severity == Some(2))
                                .collect();

                            if !errors.is_empty() || !warnings.is_empty() {
                                output.push_str("\n\n--- LSP Diagnostics ---\n");
                                let max_diagnostics = lsp::max_diagnostics_per_file();
                                let mut count = 0usize;

                                for diag in &errors {
                                    if count >= max_diagnostics {
                                        break;
                                    }
                                    let message = lsp::sanitize_diagnostic_message(&diag.message);
                                    output.push_str(&format!(
                                        "ERROR (line {}): {}\n",
                                        diag.range.start.line + 1,
                                        message
                                    ));
                                    count += 1;
                                }

                                for diag in &warnings {
                                    if count >= max_diagnostics {
                                        break;
                                    }
                                    let message = lsp::sanitize_diagnostic_message(&diag.message);
                                    output.push_str(&format!(
                                        "WARN (line {}): {}\n",
                                        diag.range.start.line + 1,
                                        message
                                    ));
                                    count += 1;
                                }

                                if errors.len() + warnings.len() > max_diagnostics {
                                    let remaining = errors.len() + warnings.len() - max_diagnostics;
                                    output.push_str(&format!(
                                        "...and {} more {} hidden.\n",
                                        remaining,
                                        if remaining == 1 {
                                            "diagnostic"
                                        } else {
                                            "diagnostics"
                                        }
                                    ));
                                }
                            }
                        }
                    }
                }

                let details = ReadDetails::new(path.clone())
                    .with_size(content.len() as u64)
                    .with_lines(lines_read)
                    .with_truncated(truncated)
                    .with_offset(if offset > 1 { Some(offset) } else { None })
                    .with_limit(limit)
                    .with_duration(start_time.elapsed().as_millis() as u64);

                ToolResult::success(output).with_details(details.to_json())
            }
            "write" | "Write" => {
                let start_time = Instant::now();
                let raw_path = args
                    .get("file_path")
                    .or_else(|| args.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = match resolve_tool_path(&self.cwd, raw_path) {
                    Ok(resolved) => resolved,
                    Err(message) => return ToolResult::failure(message),
                };

                if let Some(err) = self.deny_native_write_outside_sandbox(&path) {
                    return err;
                }

                if let Err(err) = crate::plan_mode::gate_mutation(
                    "write",
                    Some(std::path::Path::new(&path)),
                    &self.cwd,
                ) {
                    return ToolResult::failure(err);
                }

                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let preview_diff = args
                    .get("previewDiff")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                let backup = args
                    .get("backup")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                let file_existed = std::path::Path::new(&path).exists();
                let mut previous_content: Option<String> = None;
                if file_existed {
                    if let Ok(text) = tokio::fs::read_to_string(&path).await {
                        previous_content = Some(text);
                    }
                }

                if !begin_mutation_commit(cancel.as_ref()).await {
                    return cancelled_tool_result("write cancelled");
                }

                // Backup and directory creation must not use path-based
                // `create_dir_all` / `rename` / `write` before the pinned
                // `commit_native_write`: a background process can swap an
                // ancestor for a symlink after the preflight and those side
                // effects would mutate outside the writable roots even if
                // the later commit rejects the final write (review finding
                // on #3144). Route the backup through the same atomic
                // check-and-write API; parent creation is handled inside it.
                let mut backup_path: Option<String> = None;
                if file_existed && backup {
                    if let Some(prev) = &previous_content {
                        let backup_target = format!("{path}.bak");
                        match crate::sandbox::commit_native_write(
                            self.sandbox_policy.as_ref(),
                            std::path::Path::new(&self.cwd),
                            std::path::Path::new(&backup_target),
                            prev.as_bytes(),
                        ) {
                            Ok(()) => backup_path = Some(backup_target),
                            Err(error) => report_diagnostic_nonblocking(format!(
                                "[write] failed to write backup {backup_target}: {error}"
                            )),
                        }
                    }
                }

                // Atomic check-and-write: revalidates the policy against the
                // directory the bytes actually land in (see
                // `sandbox::commit_native_write`), closing the
                // check-then-write symlink-swap race.
                let write_result = crate::sandbox::commit_native_write(
                    self.sandbox_policy.as_ref(),
                    std::path::Path::new(&self.cwd),
                    std::path::Path::new(&path),
                    content.as_bytes(),
                );

                if let Err(e) = write_result {
                    if let Some(prev) = &previous_content {
                        if let Err(error) = crate::sandbox::commit_native_write(
                            self.sandbox_policy.as_ref(),
                            std::path::Path::new(&self.cwd),
                            std::path::Path::new(&path),
                            prev.as_bytes(),
                        ) {
                            report_diagnostic_nonblocking(format!(
                                "[write] failed to restore original content of {path}: {error}"
                            ));
                        }
                    }
                    let details = WriteDetails::new(path.clone())
                        .with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::failure(e).with_details(details.to_json());
                }

                // Mirror plan-mode plan files into the session plan location.
                if crate::plan_mode::is_plan_file_path(&self.cwd, std::path::Path::new(&path)) {
                    if let Err(error) = crate::plan_mode::record_plan_write(
                        &self.cwd,
                        std::path::Path::new(&path),
                        &content,
                    ) {
                        report_diagnostic_nonblocking(format!(
                            "[write] failed to mirror plan file {path}: {error}"
                        ));
                    }
                }

                let diff = if preview_diff {
                    previous_content.as_ref().map(|old| {
                        let diff = similar::TextDiff::from_lines(old, &content);
                        diff.unified_diff().to_string()
                    })
                } else {
                    None
                };

                let display_path = if raw_path.is_empty() { &path } else { raw_path };
                let mut linter_output = String::new();
                let lsp_diagnostics = if self.may_launch_native_language_server() {
                    match lsp::collect_diagnostics_for_paths(&self.cwd, std::slice::from_ref(&path))
                        .await
                    {
                        Ok(map) => {
                            if let Some(file_diags) =
                                map.get(&path).or_else(|| map.get(display_path))
                            {
                                linter_output =
                                    lsp::format_lsp_summary(display_path, file_diags.as_slice());
                            }
                            Some(map)
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };

                let validators = if self.ambient_mutation_validators_enabled {
                    match run_validators_with_diagnostics(
                        std::slice::from_ref(&path),
                        lsp_diagnostics.as_ref(),
                    )
                    .await
                    {
                        Ok(results) => Some(results),
                        Err(err) => {
                            if let Some(prev) = &previous_content {
                                if let Err(error) = crate::sandbox::commit_native_write(
                                    self.sandbox_policy.as_ref(),
                                    std::path::Path::new(&self.cwd),
                                    std::path::Path::new(&path),
                                    prev.as_bytes(),
                                ) {
                                    report_diagnostic_nonblocking(format!(
                                        "[write] failed to restore original content of {path}: {error}"
                                    ));
                                }
                            }
                            return ToolResult::failure(err);
                        }
                    }
                } else {
                    None
                };

                self.invalidate_file_cache(&path);

                let mut details = WriteDetails::new(path.clone())
                    .with_bytes(content.len() as u64)
                    .with_created(!file_existed)
                    .with_duration(start_time.elapsed().as_millis() as u64);
                if let Some(diff) = diff {
                    details = details.with_diff(diff);
                }
                if let Some(backup_path) = backup_path {
                    details = details.with_backup(backup_path);
                }
                if let Some(validators) = validators {
                    details = details.with_validators(validators);
                }

                let mut summary = format!("File written successfully: {path}");
                if !linter_output.is_empty() {
                    summary.push_str(&linter_output);
                }

                ToolResult::success(summary).with_details(details.to_json())
            }
            "glob" | "Glob" => {
                let start_time = Instant::now();
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");

                let base_path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&self.cwd);

                let full_pattern = build_glob_pattern(base_path, pattern);

                // Use native glob crate
                match glob::glob(&full_pattern) {
                    Ok(paths) => {
                        const MAX_GLOB_RESULTS: usize = 100;
                        let mut matches: Vec<String> = Vec::new();
                        let mut truncated = false;

                        for entry in paths {
                            let Ok(path) = entry else {
                                continue;
                            };
                            if matches.len() >= MAX_GLOB_RESULTS {
                                truncated = true;
                                break;
                            }
                            matches.push(path.display().to_string());
                        }

                        let details = GlobDetails::new(pattern)
                            .with_base_path(base_path)
                            .with_matches(matches.len())
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        let details = if truncated {
                            details.with_truncation()
                        } else {
                            details
                        };

                        ToolResult::success(matches.join("\n")).with_details(details.to_json())
                    }
                    Err(e) => {
                        let details = GlobDetails::new(pattern)
                            .with_base_path(base_path)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        ToolResult::failure(format!("Glob error: {e}"))
                            .with_details(details.to_json())
                    }
                }
            }
            "grep" | "Grep" => {
                let start_time = Instant::now();
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let raw_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let (display_path, shell_path) = match normalize_shell_path(raw_path) {
                    Ok(result) => result,
                    Err(message) => {
                        return ToolResult::failure(message);
                    }
                };

                if pattern.is_empty() {
                    let details =
                        GrepDetails::new("").with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::failure("Missing pattern argument")
                        .with_details(details.to_json());
                }

                let rg_args = vec![
                    "--no-heading".to_string(),
                    "-n".to_string(),
                    "--".to_string(),
                    pattern.to_string(),
                    display_path.clone(),
                ];
                let (grep_program, grep_args, grep_search_tool) =
                    build_grep_fallback_process(pattern, &display_path, &shell_path);
                let (result, search_tool) = match run_grep_with_fallback(
                    "rg",
                    &rg_args,
                    &grep_program,
                    &grep_args,
                    &self.cwd,
                    30_000,
                    MAX_GREP_LINES,
                )
                .await
                {
                    Ok(result) => result,
                    Err(message) => {
                        let details = GrepDetails::new(pattern)
                            .with_path(&display_path)
                            .with_duration(start_time.elapsed().as_millis() as u64)
                            .with_search_tool("rg");
                        return ToolResult::failure(message).with_details(details.to_json());
                    }
                };

                // Build grep details from result
                let duration_ms = start_time.elapsed().as_millis() as u64;
                let output_lines = result.stdout.lines().collect::<Vec<_>>();
                let matches_count = output_lines.len();
                let files_matched = output_lines
                    .iter()
                    .copied()
                    .filter_map(extract_grep_path)
                    .collect::<std::collections::HashSet<_>>()
                    .len();
                let truncated = result.truncated || matches_count >= MAX_GREP_LINES;
                let output = output_lines
                    .iter()
                    .take(MAX_GREP_LINES)
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");

                let details = GrepDetails::new(pattern)
                    .with_path(&display_path)
                    .with_matches(matches_count)
                    .with_files_matched(files_matched)
                    .with_duration(duration_ms)
                    .with_search_tool(if search_tool == "grep" {
                        grep_search_tool
                    } else {
                        search_tool
                    });

                let details = if truncated {
                    details.with_truncation()
                } else {
                    details
                };

                if process_succeeded_or_truncated(&result, &[0, 1]) {
                    ToolResult::success(output).with_details(details.to_json())
                } else {
                    let message = result.stderr.trim();
                    let message = if message.is_empty() {
                        format!("ripgrep exited with status {}", result.exit_code)
                    } else {
                        message.to_string()
                    };
                    ToolResult::failure(message).with_details(details.to_json())
                }
            }
            "edit" | "Edit" => {
                let start_time = Instant::now();
                let raw_path = args
                    .get("file_path")
                    .or_else(|| args.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = match resolve_tool_path(&self.cwd, raw_path) {
                    Ok(resolved) => resolved,
                    Err(message) => return ToolResult::failure(message),
                };

                if let Some(err) = self.deny_native_write_outside_sandbox(&path) {
                    return err;
                }

                if let Err(err) = crate::plan_mode::gate_mutation(
                    "edit",
                    Some(std::path::Path::new(&path)),
                    &self.cwd,
                ) {
                    return ToolResult::failure(err);
                }

                let replace_all = args
                    .get("replaceAll")
                    .or_else(|| args.get("replace_all"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let occurrence = args
                    .get("occurrence")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1) as usize;
                let dry_run = args
                    .get("dryRun")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                let edits_value = args.get("edits").and_then(|v| v.as_array());
                let mut edits: Vec<(String, String)> = Vec::new();

                if let Some(edits_array) = edits_value {
                    if replace_all || occurrence != 1 {
                        return ToolResult::failure(
                            "Cannot use replaceAll or occurrence with edits array".to_string(),
                        );
                    }
                    for edit in edits_array {
                        let old = edit
                            .get("oldText")
                            .or_else(|| edit.get("old_string"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if old.is_empty() {
                            return ToolResult::failure("Edit entry missing oldText".to_string());
                        }
                        let new = edit
                            .get("newText")
                            .or_else(|| edit.get("new_string"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        edits.push((old, new));
                    }
                } else {
                    let old = args
                        .get("oldText")
                        .or_else(|| args.get("old_string"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if old.is_empty() {
                        return ToolResult::failure("Missing oldText argument".to_string());
                    }
                    let new = args
                        .get("newText")
                        .or_else(|| args.get("new_string"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    edits.push((old, new));
                }

                // Read file content
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        let details = EditDetails::new(path.clone())
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::failure(format!("Failed to read file: {e}"))
                            .with_details(details.to_json());
                    }
                };

                let mut new_content = content.clone();
                let mut replacements_total = 0;
                for (old_text, new_text) in &edits {
                    let positions: Vec<usize> = new_content
                        .match_indices(old_text)
                        .map(|(i, _)| i)
                        .collect();
                    if positions.is_empty() {
                        let mut details = EditDetails::new(path.clone())
                            .with_replacements(replacements_total)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        details.text_not_found = true;
                        return ToolResult::failure(
                            "oldText not found in file. Make sure the string matches exactly."
                                .to_string(),
                        )
                        .with_details(details.to_json());
                    }
                    if replace_all && edits.len() == 1 {
                        replacements_total += positions.len();
                        new_content = new_content.replace(old_text, new_text);
                        continue;
                    }
                    let idx = occurrence.saturating_sub(1);
                    if idx >= positions.len() {
                        return ToolResult::failure(format!(
                            "Occurrence {} out of range ({} matches)",
                            occurrence,
                            positions.len()
                        ));
                    }
                    let pos = positions[idx];
                    let mut updated = String::new();
                    updated.push_str(&new_content[..pos]);
                    updated.push_str(new_text);
                    updated.push_str(&new_content[pos + old_text.len()..]);
                    new_content = updated;
                    replacements_total += 1;
                }

                let diff = similar::TextDiff::from_lines(&content, &new_content)
                    .unified_diff()
                    .to_string();

                if dry_run {
                    let details = EditDetails::new(path.clone())
                        .with_replacements(replacements_total)
                        .with_diff(diff)
                        .with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::success(
                        "Dry run complete (no changes written)".to_string(),
                    )
                    .with_details(details.to_json());
                }

                if !begin_mutation_commit(cancel.as_ref()).await {
                    return cancelled_tool_result("edit cancelled");
                }
                // Atomic check-and-write (see `sandbox::commit_native_write`):
                // the policy is revalidated against the directory the bytes
                // actually land in, closing the check-then-write
                // symlink-swap race.
                if let Err(e) = crate::sandbox::commit_native_write(
                    self.sandbox_policy.as_ref(),
                    std::path::Path::new(&self.cwd),
                    std::path::Path::new(&path),
                    new_content.as_bytes(),
                ) {
                    let details = EditDetails::new(path.clone())
                        .with_duration(start_time.elapsed().as_millis() as u64);
                    return ToolResult::failure(e).with_details(details.to_json());
                }

                let display_path = if raw_path.is_empty() { &path } else { raw_path };
                let mut linter_output = String::new();
                let lsp_diagnostics = if self.may_launch_native_language_server() {
                    match lsp::collect_diagnostics_for_paths(&self.cwd, std::slice::from_ref(&path))
                        .await
                    {
                        Ok(map) => {
                            if let Some(file_diags) =
                                map.get(&path).or_else(|| map.get(display_path))
                            {
                                linter_output =
                                    lsp::format_lsp_summary(display_path, file_diags.as_slice());
                            }
                            Some(map)
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };

                let validators = if self.ambient_mutation_validators_enabled {
                    match run_validators_with_diagnostics(
                        std::slice::from_ref(&path),
                        lsp_diagnostics.as_ref(),
                    )
                    .await
                    {
                        Ok(results) => Some(results),
                        Err(err) => {
                            if let Err(error) = tokio::fs::write(&path, &content).await {
                                report_diagnostic_nonblocking(format!(
                                    "[edit] failed to restore original content of {path}: {error}"
                                ));
                            }
                            return ToolResult::failure(err);
                        }
                    }
                } else {
                    None
                };

                self.invalidate_file_cache(&path);

                let mut details = EditDetails::new(path.clone())
                    .with_replacements(replacements_total)
                    .with_diff(diff)
                    .with_duration(start_time.elapsed().as_millis() as u64)
                    .with_line_changes(&content, &new_content);
                if let Some(validators) = validators {
                    details = details.with_validators(validators);
                }

                let mut summary =
                    format!("Successfully replaced {replacements_total} occurrence(s) in {path}");
                if !linter_output.is_empty() {
                    summary.push_str(&linter_output);
                }

                ToolResult::success(summary).with_details(details.to_json())
            }
            "diff" | "Diff" => {
                let start_time = Instant::now();
                // Git diff tool - shows changes in working tree or between commits
                let target = args
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HEAD");
                // `target` lands in argv right after `git diff` with no `--`
                // separator; an option-like value (e.g. `--output=...`) would
                // make git write to arbitrary files.
                if target.starts_with('-') {
                    return ToolResult::failure("diff target must be a git ref, not an option");
                }

                let path = args.get("path").and_then(|v| v.as_str());
                let normalized_path = path.map(|raw_path| normalize_git_path(&self.cwd, raw_path));
                let (display_path, git_path) = match normalized_path.transpose() {
                    Ok(Some((display, _))) => (Some(display.clone()), Some(display)),
                    Ok(None) => (None, None),
                    Err(message) => {
                        return ToolResult::failure(message);
                    }
                };

                let mut git_args = vec!["diff".to_string(), target.to_string()];
                if let Some(path) = git_path.as_ref() {
                    git_args.push("--".to_string());
                    git_args.push(path.clone());
                }
                let result = match run_process_limited_stdout_lines(
                    "git",
                    &git_args,
                    &self.cwd,
                    30_000,
                    MAX_DIFF_LINES,
                )
                .await
                {
                    Ok(result) => result,
                    Err(message) => {
                        let mut details = DiffDetails::new(target)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        if let Some(p) = display_path.as_ref() {
                            details = details.with_path(p);
                        }
                        return ToolResult::failure(message).with_details(details.to_json());
                    }
                };

                // Build diff details
                let duration_ms = start_time.elapsed().as_millis() as u64;
                let mut details = DiffDetails::new(target).with_duration(duration_ms);

                if let Some(p) = display_path.as_ref() {
                    details = details.with_path(p);
                }

                // Parse diff stats from output (count +/- lines)
                let output_lines = result.stdout.lines().collect::<Vec<_>>();
                let insertions = output_lines
                    .iter()
                    .copied()
                    .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                    .count();
                let deletions = output_lines
                    .iter()
                    .copied()
                    .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                    .count();
                let files_changed = output_lines
                    .iter()
                    .copied()
                    .filter(|line| line.starts_with("diff --git"))
                    .count();

                if files_changed > 0 || insertions > 0 || deletions > 0 {
                    details = details.with_stats(files_changed, insertions, deletions);
                }

                let truncated = result.truncated;
                if truncated {
                    details = details.with_truncation();
                }
                let output = output_lines.join("\n");

                if process_succeeded_or_truncated(&result, &[0]) {
                    ToolResult::success(output).with_details(details.to_json())
                } else {
                    let message = result.stderr.trim();
                    let message = if message.is_empty() {
                        format!("git diff exited with status {}", result.exit_code)
                    } else {
                        message.to_string()
                    };
                    ToolResult::failure(message).with_details(details.to_json())
                }
            }
            "list" | "List" | "ls" => {
                let start_time = Instant::now();
                // Directory listing tool
                let raw_path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&self.cwd);
                let (display_path, shell_path) = match normalize_shell_path(raw_path) {
                    Ok(result) => result,
                    Err(message) => {
                        return ToolResult::failure(message);
                    }
                };

                let recursive = args
                    .get("recursive")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                let (program, process_args, process_name) =
                    match build_list_process(&display_path, &shell_path, recursive) {
                        Ok(result) => result,
                        Err(message) => {
                            let details = ListDetails::new(&display_path)
                                .with_duration(start_time.elapsed().as_millis() as u64);
                            return ToolResult::failure(message).with_details(details.to_json());
                        }
                    };
                let result = match run_process_limited_stdout_lines(
                    &program,
                    &process_args,
                    &self.cwd,
                    10_000,
                    MAX_LIST_LINES,
                )
                .await
                {
                    Ok(result) => result,
                    Err(message) => {
                        let details = ListDetails::new(&display_path)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::failure(message).with_details(details.to_json());
                    }
                };

                // Build list details
                let duration_ms = start_time.elapsed().as_millis() as u64;
                let output_lines = result.stdout.lines().collect::<Vec<_>>();
                let entries_count = output_lines.len();
                let truncated = result.truncated;
                let output = output_lines.join("\n");

                let mut details = ListDetails::new(&display_path)
                    .with_entries(entries_count)
                    .with_duration(duration_ms);

                if recursive {
                    details = details.with_recursive();
                }

                if truncated {
                    details = details.with_truncation();
                }

                if process_succeeded_or_truncated(&result, &[0]) {
                    ToolResult::success(output).with_details(details.to_json())
                } else {
                    let message = result.stderr.trim();
                    let message = if message.is_empty() {
                        format!("{process_name} exited with status {}", result.exit_code)
                    } else {
                        message.to_string()
                    };
                    ToolResult::failure(message).with_details(details.to_json())
                }
            }
            "find" | "Find" => {
                let start_time = Instant::now();
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                if pattern.is_empty() {
                    return ToolResult::failure("Missing pattern argument".to_string());
                }
                let raw_path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&self.cwd);
                let (display_path, _) = match normalize_shell_path(raw_path) {
                    Ok(result) => result,
                    Err(message) => return ToolResult::failure(message),
                };
                let limit = args
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1000)
                    .max(1) as usize;
                let include_hidden = args
                    .get("includeHidden")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                let mut rg_args = vec!["--files".to_string(), "--color=never".to_string()];
                if include_hidden {
                    rg_args.push("--hidden".to_string());
                }
                rg_args.push("-g".to_string());
                rg_args.push(pattern.to_string());
                rg_args.push("--".to_string());
                rg_args.push(display_path.clone());

                let result = match run_process_limited_stdout_lines(
                    "rg", &rg_args, &self.cwd, 20_000, limit,
                )
                .await
                {
                    Ok(result) => result,
                    Err(message) => {
                        let details = ListDetails::new(&display_path)
                            .with_duration(start_time.elapsed().as_millis() as u64);
                        return ToolResult::failure(message).with_details(details.to_json());
                    }
                };

                let duration_ms = start_time.elapsed().as_millis() as u64;
                let output_lines = result.stdout.lines().collect::<Vec<_>>();
                let count = output_lines.len();
                let truncated = result.truncated;
                let output = output_lines.join("\n");
                let mut details = ListDetails::new(&display_path)
                    .with_entries(count)
                    .with_duration(duration_ms);
                if truncated {
                    details = details.with_truncation();
                }

                if process_succeeded_or_truncated(&result, &[0, 1]) {
                    ToolResult::success(output).with_details(details.to_json())
                } else {
                    let message = result.stderr.trim();
                    let message = if message.is_empty() {
                        format!("ripgrep exited with status {}", result.exit_code)
                    } else {
                        message.to_string()
                    };
                    ToolResult::failure(message).with_details(details.to_json())
                }
            }
            "search" | "Search" => self.execute_search(args).await,
            "repository_symbols" => self.execute_repository_symbols(args).await,
            "parallel_ripgrep" | "ParallelRipgrep" => {
                let patterns = args.get("patterns").and_then(|v| v.as_array()).cloned();
                let patterns = match patterns {
                    Some(p) if !p.is_empty() => p,
                    _ => return ToolResult::failure("patterns array required".to_string()),
                };

                let mut combined = Vec::new();
                let mut commands = Vec::new();
                let mut total_matches = 0usize;
                for pattern_value in patterns {
                    let pattern = match pattern_value.as_str() {
                        Some(p) => p.to_string(),
                        None => continue,
                    };
                    let mut search_args = args.clone();
                    if let Some(obj) = search_args.as_object_mut() {
                        obj.insert("pattern".to_string(), Value::String(pattern.clone()));
                        obj.remove("patterns");
                    }
                    let result = self.execute_search(&search_args).await;
                    commands.push(pattern);
                    if result.success {
                        let line_count = result.output.lines().count();
                        total_matches += line_count;
                        combined.push(result.output);
                    } else {
                        combined.push(result.error.unwrap_or_default());
                    }
                }
                let details = serde_json::json!({
                    "commands": commands,
                    "matchCount": total_matches
                });
                ToolResult::success(combined.join("\n\n")).with_details(details)
            }
            "status" | "Status" => status::git_status(args.clone(), &self.cwd).await,
            "background_tasks" => {
                let action = args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("list");
                match action {
                    "start" => {
                        if let Err(err) =
                            crate::plan_mode::gate_mutation("background_tasks", None, &self.cwd)
                        {
                            return ToolResult::failure(err);
                        }
                        let command = match args.get("command").and_then(|v| v.as_str()) {
                            Some(cmd) => cmd.to_string(),
                            None => {
                                return ToolResult::failure(
                                    "command required for start".to_string(),
                                );
                            }
                        };
                        let requested_cwd = args.get("cwd").and_then(|v| v.as_str());
                        let cwd = match requested_cwd {
                            Some(raw) if !raw.trim().is_empty() => {
                                let raw = raw.trim();
                                // Resolve relative to the session workspace
                                // (not whatever directory the Maestro process
                                // itself happens to be running from), so the
                                // sandbox check below and the spawned
                                // process see the same path.
                                if std::path::Path::new(raw).is_absolute() {
                                    raw.to_string()
                                } else {
                                    std::path::Path::new(&self.cwd)
                                        .join(raw)
                                        .to_string_lossy()
                                        .to_string()
                                }
                            }
                            _ => self.cwd.clone(),
                        };
                        // `background_tasks::start` passes `cwd` straight
                        // through as the sandbox spawn cwd, and the sandbox
                        // policy automatically treats a spawn's cwd as a
                        // writable root (see `get_writable_roots_with_cwd`).
                        // A model-supplied cwd must not be allowed to expand
                        // the writable footprint beyond what the workspace
                        // sandbox already grants -- otherwise `background_tasks
                        // { cwd: "$HOME" }` silently makes the whole home
                        // directory writable under a policy advertised as
                        // workspace-write.
                        if let Some(policy) = &self.sandbox_policy {
                            if !policy.allows_write_to(
                                std::path::Path::new(&self.cwd),
                                std::path::Path::new(&cwd),
                            ) {
                                return ToolResult::failure(format!(
                                    "background_tasks cwd '{cwd}' is outside the sandbox's \
                                     writable roots; omit cwd to use the workspace or pick a \
                                     directory the sandbox already allows"
                                ));
                            }
                        }
                        let shell = args
                            .get("shell")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let env = args.get("env").and_then(|v| v.as_object()).map(|map| {
                            map.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect::<std::collections::HashMap<_, _>>()
                        });
                        match background_tasks::start(
                            command,
                            cwd,
                            self.cwd.clone(),
                            shell,
                            env,
                            self.sandbox_policy.clone(),
                        )
                        .await
                        {
                            Ok(task) => {
                                let details = serde_json::json!({
                                    "id": task.id,
                                    "pid": task.pid,
                                    "status": "running",
                                    "logPath": task.log_path
                                });
                                ToolResult::success(format!("Started task {}", task.id))
                                    .with_details(details)
                            }
                            Err(err) => ToolResult::failure(err),
                        }
                    }
                    "stop" => {
                        let id = match args.get("taskId").and_then(|v| v.as_str()) {
                            Some(id) => id,
                            None => {
                                return ToolResult::failure("taskId required for stop".to_string());
                            }
                        };
                        match background_tasks::stop(id) {
                            Ok(task) => ToolResult::success(format!("Stopped task {}", task.id)),
                            Err(err) => ToolResult::failure(err),
                        }
                    }
                    "logs" => {
                        let id = match args.get("taskId").and_then(|v| v.as_str()) {
                            Some(id) => id,
                            None => {
                                return ToolResult::failure("taskId required for logs".to_string());
                            }
                        };
                        let lines = args
                            .get("lines")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(40) as usize;
                        match background_tasks::logs(id, lines) {
                            Ok(logs) => ToolResult::success(logs),
                            Err(err) => ToolResult::failure(err),
                        }
                    }
                    "waitForRotation" | "wait_for_rotation" => {
                        let id = match args.get("taskId").and_then(|v| v.as_str()) {
                            Some(id) => id,
                            None => {
                                return ToolResult::failure(
                                    "taskId required for waitForRotation".to_string(),
                                );
                            }
                        };
                        // Default 0 = non-blocking snapshot (do not stall the turn).
                        let timeout_ms = args
                            .get("timeoutMs")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        match background_tasks::wait_for_rotation(
                            id,
                            Duration::from_millis(timeout_ms),
                        )
                        .await
                        {
                            Ok(info) => {
                                let rotated_at = info
                                    .rotated_at
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .ok()
                                    .map(|duration| duration.as_millis() as u64);
                                let details = serde_json::json!({
                                    "logPath": info.log_path.to_string_lossy(),
                                    "archivePath": info.archive_path.to_string_lossy(),
                                    "rotatedAt": rotated_at
                                });
                                ToolResult::success(format!("Log rotated for task {}", id))
                                    .with_details(details)
                            }
                            Err(err) => ToolResult::failure(err),
                        }
                    }
                    _ => {
                        let tasks = background_tasks::list();
                        let summary = tasks
                            .iter()
                            .map(|t| {
                                let mut line = format!("{} {:?} {}", t.id, t.status, t.command);
                                if t.log_write_failed {
                                    if let Some(reason) = &t.log_write_error {
                                        let reason = reason.replace(['\n', '\r'], " ");
                                        line.push_str(&format!(" [log write failed: {reason}]"));
                                    } else {
                                        line.push_str(" [log write failed]");
                                    }
                                }
                                line
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let details = serde_json::json!({ "count": tasks.len() });
                        ToolResult::success(if summary.is_empty() {
                            "No background tasks".to_string()
                        } else {
                            summary
                        })
                        .with_details(details)
                    }
                }
            }
            "spawn_subagent" => {
                self.subagents
                    .spawn(
                        args,
                        call_id,
                        self.sandbox_policy.clone(),
                        self.credential_vault.clone(),
                        cancel.as_ref(),
                    )
                    .await
            }
            "list_subagents" => self.subagents.list().await,
            "get_subagent" => {
                if self.subagents.uses_orb_backend(args) {
                    self.subagents.get_remote(args, cancel.as_ref()).await
                } else {
                    self.subagents.get(args)
                }
            }
            "inspect_subagent" => self.subagents.inspect(args),
            "cleanup_subagent" => self.subagents.cleanup(args),
            "wait_subagent" => self.subagents.wait(args, cancel.as_ref()).await,
            "resume_subagent" => {
                self.subagents
                    .resume(
                        args,
                        call_id,
                        self.sandbox_policy.clone(),
                        self.credential_vault.clone(),
                        cancel.as_ref(),
                    )
                    .await
            }
            "cancel_subagent" => self.subagents.cancel(args, cancel.as_ref()).await,
            "control_subagent" => {
                self.subagents
                    .control(args, call_id, self.credential_vault.clone())
                    .await
            }
            "get_goal" => crate::tools::goal_tools::get_goal(),
            "update_goal" => crate::tools::goal_tools::update_goal(args.clone()),
            "get_harness_context" => crate::tools::context_tools::get_harness_context(),
            "propose_harness_refinement" => {
                crate::tools::context_tools::propose_harness_refinement(args.clone())
            }
            "apply_harness_refinement" => {
                crate::tools::context_tools::apply_harness_refinement(args.clone())
            }
            "reject_harness_refinement" => {
                crate::tools::context_tools::reject_harness_refinement(args.clone())
            }
            "get_rlm_context" => crate::tools::context_tools::get_rlm_context(),
            "set_rlm_context" => crate::tools::context_tools::set_rlm_context(args.clone()),
            "append_rlm_context" => crate::tools::context_tools::append_rlm_context(args.clone()),
            "render_rlm_context" => crate::tools::context_tools::render_rlm_context(args.clone()),
            "clear_rlm_context" => crate::tools::context_tools::clear_rlm_context(args.clone()),
            "get_mailbox" => crate::tools::context_tools::get_mailbox(&self.mailbox_identity),
            "send_mailbox" => {
                crate::tools::context_tools::send_mailbox(args.clone(), &self.mailbox_identity)
            }
            "read_mailbox" => {
                crate::tools::context_tools::read_mailbox(args.clone(), &self.mailbox_identity)
            }
            "ack_mailbox" => crate::tools::context_tools::acknowledge_mailbox(
                args.clone(),
                &self.mailbox_identity,
            ),
            "compact_mailbox" => crate::tools::context_tools::compact_mailbox(),
            "todo" => todo::todo_with_cancellation(args.clone(), cancel.as_ref()).await,
            "draft_feedback" => {
                if event_tx.is_none() {
                    ToolResult::failure("Feedback drafts require an interactive session.")
                } else {
                    crate::bug_report::draft_tool(args.clone())
                }
            }
            "ask_user" => ask_user::ask_user(args.clone()),
            "extract_document" => {
                extract_document::extract_document_with_cancellation(args.clone(), cancel).await
            }
            "notebook_edit" => {
                // `notebook_edit` resolves its own `path` argument the same
                // way (absolute path used as-is, otherwise joined to cwd);
                // mirror that resolution here so the sandbox containment
                // check below sees the exact path the tool will write to.
                let raw_path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let trimmed = raw_path.trim();
                if !trimmed.is_empty() {
                    let resolved = if std::path::Path::new(trimmed).is_absolute() {
                        trimmed.to_string()
                    } else {
                        std::path::Path::new(&self.cwd)
                            .join(trimmed)
                            .to_string_lossy()
                            .to_string()
                    };
                    if let Some(err) = self.deny_native_write_outside_sandbox(&resolved) {
                        return err;
                    }
                }
                notebook_edit::notebook_edit_with_cancellation(
                    args.clone(),
                    &self.cwd,
                    cancel.as_ref(),
                    self.sandbox_policy.as_ref(),
                )
                .await
            }
            "websearch" => exa::websearch(args.clone()).await,
            "codesearch" => exa::codesearch(args.clone()).await,
            "gh_pr" => gh::gh_pr(args.clone(), &self.cwd, cancel.as_ref()).await,
            "gh_issue" => gh::gh_issue(args.clone(), cancel.as_ref()).await,
            "gh_repo" => gh::gh_repo(args.clone(), &self.cwd, cancel.as_ref()).await,
            "mcp_list_resources" => {
                let server_filter = args.get("server").and_then(|v| v.as_str());
                let client = match self.ensure_mcp_client().await {
                    Ok(client) => client,
                    Err(err) => return ToolResult::failure(err),
                };

                let mut resources = client.list_all_resources().await;
                if let Some(filter) = server_filter {
                    resources.retain(|(name, _)| name == filter);
                }

                let mut servers = Vec::new();
                for (name, uris) in resources {
                    if uris.is_empty() {
                        continue;
                    }
                    servers.push(serde_json::json!({
                        "name": name,
                        "resources": uris
                    }));
                }

                if servers.is_empty() {
                    return ToolResult::success(
                        "No MCP resources available. Either no servers are connected or they don't expose resources.".to_string(),
                    )
                    .with_details(serde_json::json!({ "servers": [] }));
                }

                let mut lines = Vec::new();
                lines.push("# Available MCP Resources".to_string());
                lines.push(String::new());
                for server in &servers {
                    let name = server
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    lines.push(format!("## {name}"));
                    if let Some(resources) = server.get("resources").and_then(|v| v.as_array()) {
                        for uri in resources {
                            if let Some(uri_str) = uri.as_str() {
                                lines.push(format!("- {uri_str}"));
                            }
                        }
                    }
                    lines.push(String::new());
                }

                ToolResult::success(lines.join("\n"))
                    .with_details(serde_json::json!({ "servers": servers }))
            }
            "mcp_list_prompts" => {
                let server_filter = args.get("server").and_then(|v| v.as_str());
                let prompt_servers = match self.mcp_prompt_details(server_filter).await {
                    Ok(entries) => entries,
                    Err(err) => return ToolResult::failure(err),
                };

                let mut servers = Vec::new();
                let mut lines = Vec::new();
                lines.push("# Available MCP Prompts".to_string());
                lines.push(String::new());
                for (name, prompts) in prompt_servers {
                    if prompts.is_empty() {
                        continue;
                    }
                    lines.push(format!("## {name}"));
                    for prompt in &prompts {
                        append_mcp_prompt_summary(&mut lines, prompt, "- ", "  ");
                    }
                    lines.push(String::new());
                    servers.push(serde_json::json!({
                        "name": name,
                        "prompts": prompts
                    }));
                }

                if servers.is_empty() {
                    return ToolResult::success(
                        "No MCP prompts available. Either no servers are connected or they don't expose prompts.".to_string(),
                    )
                    .with_details(serde_json::json!({ "servers": [] }));
                }

                ToolResult::success(lines.join("\n"))
                    .with_details(serde_json::json!({ "servers": servers }))
            }
            "mcp_read_resource" => {
                let server = args.get("server").and_then(|v| v.as_str()).unwrap_or("");
                let uri = args.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                if server.is_empty() || uri.is_empty() {
                    return ToolResult::failure("server and uri are required".to_string());
                }

                let client = match self.ensure_mcp_client().await {
                    Ok(client) => client,
                    Err(err) => return ToolResult::failure(err),
                };

                match client.read_resource(server, uri).await {
                    Ok(result) => {
                        if result.contents.is_empty() {
                            return ToolResult::success(format!("Resource '{uri}' is empty."))
                                .with_details(serde_json::json!({
                                    "server": server,
                                    "uri": uri,
                                    "contents": []
                                }));
                        }

                        let text_output = result
                            .contents
                            .iter()
                            .filter_map(|content| content.text.clone())
                            .collect::<Vec<_>>()
                            .join("\n---\n");
                        let output = if text_output.is_empty() {
                            serde_json::to_string_pretty(&result.contents).unwrap_or_else(|_| {
                                "MCP resource returned non-text content".to_string()
                            })
                        } else {
                            text_output
                        };

                        ToolResult::success(output).with_details(serde_json::json!({
                            "server": server,
                            "uri": uri,
                            "contents": result.contents
                        }))
                    }
                    Err(err) => ToolResult::failure(format!("Failed to read MCP resource: {err}")),
                }
            }
            "mcp_get_prompt" => {
                let server = args.get("server").and_then(|v| v.as_str()).unwrap_or("");
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if server.is_empty() || name.is_empty() {
                    return ToolResult::failure("server and name are required".to_string());
                }

                let arguments = args
                    .get("arguments")
                    .and_then(|v| v.as_object())
                    .map(|obj| {
                        obj.iter()
                            .map(|(key, value)| {
                                let value = match value {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                (key.clone(), value)
                            })
                            .collect::<HashMap<String, String>>()
                    });

                let client = match self.ensure_mcp_client().await {
                    Ok(client) => client,
                    Err(err) => return ToolResult::failure(err),
                };

                match client.get_prompt(server, name, arguments).await {
                    Ok(result) => {
                        let description = result.description.clone();
                        let messages = result.messages;
                        let mut lines = Vec::new();
                        lines.push(format!("Prompt: {name}"));
                        if let Some(desc) = &description {
                            lines.push(String::new());
                            lines.push(format!("Description: {desc}"));
                        }
                        lines.push(String::new());
                        for msg in &messages {
                            lines.push(format!("[{}]", msg.role));
                            let content = msg.content.as_text().unwrap_or("[non-text content]");
                            lines.push(content.to_string());
                            lines.push(String::new());
                        }

                        ToolResult::success(lines.join("\n")).with_details(serde_json::json!({
                            "server": server,
                            "name": name,
                            "description": description,
                            "messages": messages,
                        }))
                    }
                    Err(err) => ToolResult::failure(format!("Failed to get MCP prompt: {err}")),
                }
            }
            "vscode_get_diagnostics" | "jetbrains_get_diagnostics" => {
                let uri = args.get("uri").and_then(|v| v.as_str());
                let diagnostics = if let Some(uri) = uri {
                    let uri = normalize_uri_input(uri);
                    let path = match resolve_tool_path(&self.cwd, &uri) {
                        Ok(resolved) => resolved,
                        Err(message) => return ToolResult::failure(message),
                    };
                    match lsp::diagnostics_for_file(&self.cwd, &path).await {
                        Ok(entries) => entries,
                        Err(err) => return ToolResult::failure(err),
                    }
                } else {
                    match lsp::collect_workspace_diagnostics(&self.cwd).await {
                        Ok(map) => map.values().flat_map(|entries| entries.clone()).collect(),
                        Err(err) => return ToolResult::failure(err),
                    }
                };

                let output =
                    serde_json::to_string_pretty(&diagnostics).unwrap_or_else(|_| "[]".to_string());
                ToolResult::success(output)
            }
            "vscode_get_definition" | "jetbrains_get_definition" => {
                let raw_uri = args.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                if raw_uri.is_empty() {
                    return ToolResult::failure("uri is required".to_string());
                }
                let line = match args.get("line").and_then(serde_json::Value::as_i64) {
                    Some(value) if value >= 0 => value as u32,
                    _ => {
                        return ToolResult::failure(
                            "line must be a non-negative integer".to_string(),
                        );
                    }
                };
                let character = match args.get("character").and_then(serde_json::Value::as_i64) {
                    Some(value) if value >= 0 => value as u32,
                    _ => {
                        return ToolResult::failure(
                            "character must be a non-negative integer".to_string(),
                        );
                    }
                };

                let uri = normalize_uri_input(raw_uri);
                let path = match resolve_tool_path(&self.cwd, &uri) {
                    Ok(resolved) => resolved,
                    Err(message) => return ToolResult::failure(message),
                };

                let locations =
                    match lsp::definition_for_position(&self.cwd, &path, line, character).await {
                        Ok(entries) => entries,
                        Err(err) => return ToolResult::failure(err),
                    };
                let normalized: Vec<_> = locations
                    .into_iter()
                    .map(|mut location| {
                        location.uri = normalize_uri_input(&location.uri);
                        location
                    })
                    .collect();

                let output =
                    serde_json::to_string_pretty(&normalized).unwrap_or_else(|_| "[]".to_string());
                ToolResult::success(output)
            }
            "vscode_find_references" | "jetbrains_find_references" => {
                let raw_uri = args.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                if raw_uri.is_empty() {
                    return ToolResult::failure("uri is required".to_string());
                }
                let line = match args.get("line").and_then(serde_json::Value::as_i64) {
                    Some(value) if value >= 0 => value as u32,
                    _ => {
                        return ToolResult::failure(
                            "line must be a non-negative integer".to_string(),
                        );
                    }
                };
                let character = match args.get("character").and_then(serde_json::Value::as_i64) {
                    Some(value) if value >= 0 => value as u32,
                    _ => {
                        return ToolResult::failure(
                            "character must be a non-negative integer".to_string(),
                        );
                    }
                };
                let include_declaration = args
                    .get("includeDeclaration")
                    .or_else(|| args.get("include_declaration"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                let uri = normalize_uri_input(raw_uri);
                let path = match resolve_tool_path(&self.cwd, &uri) {
                    Ok(resolved) => resolved,
                    Err(message) => return ToolResult::failure(message),
                };

                let locations = match lsp::references_for_position(
                    &self.cwd,
                    &path,
                    line,
                    character,
                    include_declaration,
                )
                .await
                {
                    Ok(entries) => entries,
                    Err(err) => return ToolResult::failure(err),
                };
                let normalized: Vec<_> = locations
                    .into_iter()
                    .map(|mut location| {
                        location.uri = normalize_uri_input(&location.uri);
                        location
                    })
                    .collect();

                let output =
                    serde_json::to_string_pretty(&normalized).unwrap_or_else(|_| "[]".to_string());
                ToolResult::success(output)
            }
            "vscode_read_file_range" | "jetbrains_read_file_range" => {
                let start_time = Instant::now();
                let raw_uri = args.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                if raw_uri.is_empty() {
                    return ToolResult::failure("uri is required".to_string());
                }
                let start_line = match args.get("startLine").and_then(serde_json::Value::as_i64) {
                    Some(value) if value >= 0 => value as usize,
                    _ => {
                        return ToolResult::failure(
                            "startLine must be a non-negative integer".to_string(),
                        );
                    }
                };
                let end_line = match args.get("endLine").and_then(serde_json::Value::as_i64) {
                    Some(value) if value >= 0 => value as usize,
                    _ => {
                        return ToolResult::failure(
                            "endLine must be a non-negative integer".to_string(),
                        );
                    }
                };

                let uri = normalize_uri_input(raw_uri);
                let path = match resolve_tool_path(&self.cwd, &uri) {
                    Ok(resolved) => resolved,
                    Err(message) => return ToolResult::failure(message),
                };

                let (output, lines_read) = match read_file_range(&path, start_line, end_line).await
                {
                    Ok(result) => result,
                    Err(err) => return ToolResult::failure(err),
                };

                let size_bytes = tokio::fs::metadata(&path).await.ok().map(|m| m.len());
                let mut details = ReadDetails::new(path.clone())
                    .with_lines(lines_read)
                    .with_offset(Some(start_line + 1))
                    .with_limit(Some(end_line.saturating_sub(start_line) + 1))
                    .with_duration(start_time.elapsed().as_millis() as u64);
                if let Some(size) = size_bytes {
                    details = details.with_size(size);
                }

                ToolResult::success(output).with_details(details.to_json())
            }
            "web_fetch" | "WebFetch" | "webfetch" => {
                let fetch_args: WebFetchArgs = match serde_json::from_value(args.clone()) {
                    Ok(a) => a,
                    Err(e) => {
                        return ToolResult::failure(format!("Invalid web_fetch arguments: {e}"));
                    }
                };

                // Send tool start event
                if let Some(tx) = lifecycle_event_tx {
                    let _ = tx.send(FromAgent::ToolStart {
                        call_id: call_id.to_string(),
                    });
                }

                // Race the fetch against per-turn cancellation (Ctrl+C) so a
                // slow or hung request doesn't keep the turn alive until its
                // own timeout. Unlike bash there is no child process tree to
                // reap: dropping the losing `execute` future drops its
                // in-flight HTTP request along with it.
                let result = match cancel {
                    Some(token) => {
                        tokio::select! {
                            result = self.web_fetch.execute(fetch_args) => result,
                            () = token.cancelled() => {
                                cancelled_tool_result("web_fetch cancelled")
                            }
                        }
                    }
                    None => self.web_fetch.execute(fetch_args).await,
                };

                // Send tool output event
                if let Some(tx) = lifecycle_event_tx {
                    if !result.output.is_empty() {
                        let _ = tx.send(FromAgent::ToolOutput {
                            call_id: call_id.to_string(),
                            content: result.output.clone(),
                        });
                    }

                    let _ = tx.send(FromAgent::ToolEnd {
                        call_id: call_id.to_string(),
                        success: result.success,
                        result: Some(result.clone()),
                        receipt: None,
                    });
                }

                result
            }
            "read_image" | "ReadImage" | "readimage" => {
                let image_args: ReadImageArgs = match serde_json::from_value(args.clone()) {
                    Ok(a) => a,
                    Err(e) => {
                        return ToolResult::failure(format!("Invalid read_image arguments: {e}"));
                    }
                };

                // Send tool start event
                if let Some(tx) = lifecycle_event_tx {
                    let _ = tx.send(FromAgent::ToolStart {
                        call_id: call_id.to_string(),
                    });
                }

                let result = self.image.read_image(image_args).await;

                // Send tool output event
                if let Some(tx) = lifecycle_event_tx {
                    if !result.output.is_empty() {
                        let _ = tx.send(FromAgent::ToolOutput {
                            call_id: call_id.to_string(),
                            content: result.output.clone(),
                        });
                    }

                    let _ = tx.send(FromAgent::ToolEnd {
                        call_id: call_id.to_string(),
                        success: result.success,
                        result: Some(result.clone()),
                        receipt: None,
                    });
                }

                result
            }
            "screenshot" | "Screenshot" => {
                let screenshot_args: ScreenshotArgs = match serde_json::from_value(args.clone()) {
                    Ok(a) => a,
                    Err(e) => {
                        return ToolResult::failure(format!("Invalid screenshot arguments: {e}"));
                    }
                };

                // Send tool start event
                if let Some(tx) = lifecycle_event_tx {
                    let _ = tx.send(FromAgent::ToolStart {
                        call_id: call_id.to_string(),
                    });
                }

                let result = self.image.screenshot(screenshot_args).await;

                // Send tool output event
                if let Some(tx) = lifecycle_event_tx {
                    if !result.output.is_empty() {
                        let _ = tx.send(FromAgent::ToolOutput {
                            call_id: call_id.to_string(),
                            content: result.output.clone(),
                        });
                    }

                    let _ = tx.send(FromAgent::ToolEnd {
                        call_id: call_id.to_string(),
                        success: result.success,
                        result: Some(result.clone()),
                        receipt: None,
                    });
                }

                result
            }
            _ => {
                // Check if this is an inline tool
                if let Some(inline_tool) = self.get_inline_tool(tool_name) {
                    // Send tool start event
                    if let Some(tx) = lifecycle_event_tx {
                        let _ = tx.send(FromAgent::ToolStart {
                            call_id: call_id.to_string(),
                        });
                    }

                    let result = vault_tool_result_credentials(
                        &self.credential_vault,
                        generation,
                        self.inline_executor
                            .execute_cancellable_with_environment(
                                inline_tool,
                                args.clone(),
                                cancel.as_ref(),
                                approved_inline_env,
                            )
                            .await,
                    );

                    // Send tool output and end events
                    if let Some(tx) = lifecycle_event_tx {
                        if !result.output.is_empty() {
                            let _ = tx.send(FromAgent::ToolOutput {
                                call_id: call_id.to_string(),
                                content: result.output.clone(),
                            });
                        }

                        let _ = tx.send(FromAgent::ToolEnd {
                            call_id: call_id.to_string(),
                            success: result.success,
                            result: Some(result.clone()),
                            receipt: None,
                        });
                    }

                    result
                } else {
                    ToolResult::failure(format!("Unknown tool: {tool_name}"))
                }
            }
        }
    }
}

fn cancelled_tool_result(message: &str) -> ToolResult {
    ToolResult::failure(message).with_details(serde_json::json!({"cancelled": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_redactor_holds_split_uri_delimiter_until_redaction() {
        let vault = CredentialVault::new();
        let generation = vault.generation();
        let mut redactor = StreamingToolOutputRedactor::default();

        assert_eq!(redactor.push(&vault, generation, "https:/"), "https:/");
        assert_eq!(redactor.push(&vault, generation, "/alice:"), "");
        assert_eq!(
            redactor.push(&vault, generation, "uri-secret@example.test\n"),
            ""
        );
        let tail = redactor.finish(&vault, generation);
        assert!(tail.contains("{{CRED|password|"));
        assert!(!tail.contains("uri-secret"));
    }

    #[test]
    fn streaming_redactor_holds_whitespace_separated_value_until_boundary() {
        let vault = CredentialVault::new();
        let generation = vault.generation();
        let mut redactor = StreamingToolOutputRedactor::default();

        // The value splits across pipe reads; nothing may be released until a
        // value boundary arrives, or the raw prefix escapes unredacted.
        assert_eq!(redactor.push(&vault, generation, "token ABCDEFGHIJ"), "");
        assert_eq!(redactor.push(&vault, generation, "KLMNOPQRSTUVWXYZ"), "");
        let output = redactor.push(&vault, generation, "\ndone\n");
        assert!(!output.contains("ABCDEFGHIJ"));
        assert!(output.contains("{{CRED|"));
        assert!(output.contains("done"));
    }

    #[test]
    fn streaming_redactor_releases_complete_whitespace_separated_value() {
        let vault = CredentialVault::new();
        let generation = vault.generation();
        let mut redactor = StreamingToolOutputRedactor::default();

        let output = redactor.push(
            &vault,
            generation,
            "token ABCDEFGHIJKLMNOPQRSTUVWXYZ\nready\n",
        );
        assert!(!output.contains("ABCDEFGHIJ"));
        assert!(output.contains("{{CRED|"));
        assert!(output.contains("ready"));
    }

    #[test]
    fn streaming_redactor_holds_quoted_whitespace_separated_value() {
        let vault = CredentialVault::new();
        let generation = vault.generation();
        let mut redactor = StreamingToolOutputRedactor::default();

        assert_eq!(
            redactor.push(&vault, generation, "secret \"ABCDEFGHIJKLMNOP"),
            ""
        );
        let tail = redactor.finish(&vault, generation);
        assert!(tail.contains("ABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn streaming_redactor_holds_split_uri_scheme_until_redaction() {
        let vault = CredentialVault::new();
        let generation = vault.generation();
        let mut redactor = StreamingToolOutputRedactor::default();

        assert_eq!(redactor.push(&vault, generation, "https"), "https");
        assert_eq!(redactor.push(&vault, generation, "://alice:"), "");
        assert_eq!(
            redactor.push(&vault, generation, "uri-secret@example.test\n"),
            ""
        );
        let tail = redactor.finish(&vault, generation);
        assert!(tail.contains("{{CRED|password|"));
        assert!(!tail.contains("uri-secret"));
    }

    #[tokio::test]
    async fn read_limited_lossy_keeps_head_and_tail_of_a_large_stream() {
        // 1 MiB of filler with a marker at each end. Head-only capping kept
        // `HEAD` and dropped `TAILMARK`, which is where build tools print the
        // failure summary.
        let mut body = String::from("HEAD");
        body.push_str(&"f".repeat(1024 * 1024));
        body.push_str("TAILMARK");

        let out = read_limited_lossy(
            std::io::Cursor::new(body.into_bytes()),
            MAX_PROCESS_STDERR_BYTES,
            "[stderr truncated]",
        )
        .await;

        assert!(out.starts_with("HEAD"), "head lost: {}", &out[..32]);
        assert!(out.ends_with("TAILMARK"), "tail lost");
        assert!(out.contains("[stderr truncated]"));
        assert!(out.contains("bytes elided ...]"));
        assert!(
            out.len() <= MAX_PROCESS_STDERR_BYTES + 128,
            "output not bounded: {}",
            out.len()
        );
    }

    #[tokio::test]
    async fn read_limited_lossy_returns_short_output_unchanged() {
        let out = read_limited_lossy(
            std::io::Cursor::new(b"short output\n".to_vec()),
            MAX_PROCESS_STDERR_BYTES,
            "[stderr truncated]",
        )
        .await;
        assert_eq!(out, "short output\n");
    }

    #[test]
    fn mcp_model_output_preserves_full_payload_for_the_native_spill_boundary() {
        let body = format!("HEAD{}TAIL", "y".repeat(1024 * 1024));
        let content = vec![McpContent::Text { text: body.clone() }];

        let out = mcp_model_output(&content);

        assert_eq!(out, body);
        assert!(!out.contains("bytes elided"));
    }

    #[test]
    fn mcp_model_output_strips_nul_bytes() {
        let content = vec![McpContent::Text {
            text: "before\u{0}after".to_string(),
        }];
        let out = mcp_model_output(&content);
        assert_eq!(out, "beforeafter");
    }
}
